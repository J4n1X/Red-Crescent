//! Request bodies: size limits, multipart parts streamed to the spool, and
//! draining a rejected body so the client can read the error.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use actix_multipart::{Field, Multipart};
use actix_web::http::header::{CONTENT_LENGTH, EXPECT};
use actix_web::web::{self, Bytes};
use actix_web::{HttpRequest, HttpResponse};
use futures_util::StreamExt;

use super::AppState;
use super::response::{internal_error, payload_too_large, payload_too_large_files};
use crate::runtime::{RequestBody, UploadedFile};

/// The body as the template will see it: multipart parts spooled, anything
/// else in memory. `Err` is the response that rejects the request.
pub(super) async fn read_body(
    req: &HttpRequest,
    payload: web::Payload,
    data: &AppState,
    content_type: Option<&str>,
    path: &str,
) -> Result<RequestBody, HttpResponse> {
    let is_multipart = content_type.is_some_and(|ct| ct.starts_with("multipart/form-data"));
    let size_limit = if is_multipart {
        data.config.max_upload_size
    } else {
        data.config.max_body_size
    };

    // Reject oversized bodies from the Content-Length header, before reading
    // a single byte. Browsers always send it for uploads, so this turns what
    // would be a mid-transfer connection reset into a clean error page (and
    // with `Expect: 100-continue`, the body is never sent at all).
    if let Some(declared) = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        && declared > size_limit as u64
    {
        log::warn!("{path}: rejected upload of {declared} bytes, over the {size_limit} byte limit");
        // A client waiting on `Expect: 100-continue` has not sent anything
        // yet — reading the payload is what makes actix send the continue,
        // so draining here would provoke the very upload we are refusing.
        let awaiting_continue = req
            .headers()
            .get(EXPECT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("100-continue"));
        if !awaiting_continue {
            drain_briefly(payload).await;
        }
        return Err(payload_too_large(size_limit));
    }

    if is_multipart {
        let (fields, files) = read_multipart(req, payload, data, path).await?;
        return Ok(RequestBody::Multipart { fields, files });
    }
    match payload.to_bytes_limited(data.config.max_body_size).await {
        Ok(Ok(bytes)) => Ok(RequestBody::Raw(bytes.to_vec())),
        Ok(Err(e)) => {
            log::warn!("failed to read request body for {path}: {e}");
            Err(HttpResponse::BadRequest().body("400 Bad Request: unreadable body"))
        }
        Err(_) => {
            log::warn!(
                "{path}: request body exceeded the {} byte limit",
                data.config.max_body_size
            );
            Err(payload_too_large(data.config.max_body_size))
        }
    }
}

/// How much of a rejected request we are willing to read and throw away so the
/// client can finish sending and actually receive our error response. Without
/// it the client's write fails with EPIPE and browsers report a network error
/// instead of showing the page (this is nginx's "lingering close" in spirit).
///
/// Time is the real bound — discarding bytes is nearly free, while a client
/// blocked mid-write cannot read our response until it finishes sending. The
/// byte ceiling only stops a fast local client from making us spin. A body too
/// big to drain inside the window still gets cut off; that is unavoidable, and
/// the reason the Content-Length check in `read_body` matters so much.
const REJECT_DRAIN_BYTES: usize = 128 * 1024 * 1024;
const REJECT_DRAIN_TIME: Duration = Duration::from_secs(5);

/// Read and discard a bounded amount of an unwanted request body.
async fn drain_briefly(mut payload: web::Payload) {
    let deadline = Instant::now() + REJECT_DRAIN_TIME;
    let mut drained = 0usize;
    while let Some(chunk) = payload.next().await {
        match chunk {
            Ok(c) => drained += c.len(),
            Err(_) => return,
        }
        if drained >= REJECT_DRAIN_BYTES || Instant::now() >= deadline {
            return;
        }
    }
}

/// Same, for a body already being parsed as multipart.
async fn drain_multipart_briefly(multipart: &mut Multipart) {
    let deadline = Instant::now() + REJECT_DRAIN_TIME;
    let mut drained = 0usize;
    while let Some(Ok(mut field)) = multipart.next().await {
        while let Some(chunk) = field.next().await {
            match chunk {
                Ok(c) => drained += c.len(),
                Err(_) => return,
            }
            if drained >= REJECT_DRAIN_BYTES || Instant::now() >= deadline {
                return;
            }
        }
    }
}

/// Delete spooled files off the async threads — a rejected bulk upload can
/// leave thousands of them behind.
pub(super) async fn remove_spooled(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let _ = web::block(move || {
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    })
    .await;
}

#[derive(Default)]
struct Parts {
    fields: Vec<(String, String)>,
    files: Vec<UploadedFile>,
}

/// Stream a multipart body: text fields into memory, file parts into the
/// spool directory, bounded by max_upload_size and max_upload_files. On
/// rejection the spooled files are deleted and the rest of the request is
/// briefly drained so the client can read the response.
async fn read_multipart(
    req: &HttpRequest,
    payload: web::Payload,
    data: &AppState,
    path: &str,
) -> Result<(Vec<(String, String)>, Vec<UploadedFile>), HttpResponse> {
    let mut multipart = Multipart::new(req.headers(), payload);
    let mut parts = Parts::default();
    match read_parts(&mut multipart, &mut parts, data, path).await {
        Ok(()) => Ok((parts.fields, parts.files)),
        Err(response) => {
            remove_spooled(parts.files.into_iter().map(|f| f.path).collect()).await;
            drain_multipart_briefly(&mut multipart).await;
            Err(response)
        }
    }
}

async fn read_parts(
    multipart: &mut Multipart,
    parts: &mut Parts,
    data: &AppState,
    path: &str,
) -> Result<(), HttpResponse> {
    let mut upload = Upload {
        total: 0,
        cap: data.config.max_upload_size,
        path,
    };
    while let Some(item) = multipart.next().await {
        let mut field = item.map_err(|e| {
            log::warn!("{path}: malformed multipart body: {e}");
            HttpResponse::BadRequest().body("400 Bad Request: malformed multipart")
        })?;
        let name = field.name().unwrap_or_default().to_string();
        let filename = field
            .content_disposition()
            .and_then(|cd| cd.get_filename())
            .map(str::to_string);
        let content_type = field.content_type().map(|m| m.to_string());

        let Some(filename) = filename else {
            let mut value = Vec::new();
            while let Some(chunk) = upload.next_chunk(&mut field).await? {
                value.extend_from_slice(&chunk);
            }
            parts
                .fields
                .push((name, String::from_utf8_lossy(&value).into_owned()));
            continue;
        };

        if parts.files.len() >= data.config.max_upload_files {
            log::warn!(
                "{path}: upload rejected, more than {} files (--max-upload-files)",
                data.config.max_upload_files
            );
            return Err(payload_too_large_files(data.config.max_upload_files));
        }
        let (spooled, size) = upload.spool(&mut field, data.spool_dir.clone()).await?;
        parts.files.push(UploadedFile {
            field: name,
            filename,
            content_type,
            size,
            path: spooled,
        });
    }
    Ok(())
}

/// Running total of one multipart body against its cap.
struct Upload<'a> {
    total: usize,
    cap: usize,
    path: &'a str,
}

impl Upload<'_> {
    async fn next_chunk(&mut self, field: &mut Field) -> Result<Option<Bytes>, HttpResponse> {
        let path = self.path;
        let Some(chunk) = field.next().await else {
            return Ok(None);
        };
        let chunk = chunk.map_err(|e| {
            log::warn!("{path}: upload interrupted: {e}");
            HttpResponse::BadRequest().body("400 Bad Request: upload interrupted")
        })?;
        self.total += chunk.len();
        if self.total > self.cap {
            log::warn!(
                "{path}: upload rejected, body exceeds {} bytes (--max-upload-size)",
                self.cap
            );
            return Err(payload_too_large(self.cap));
        }
        Ok(Some(chunk))
    }

    /// Writes one file part to a spool file, kept only once it is complete; an
    /// early return drops the temp file, which deletes it.
    async fn spool(
        &mut self,
        field: &mut Field,
        spool_dir: PathBuf,
    ) -> Result<(PathBuf, u64), HttpResponse> {
        let path = self.path;
        let mut tmp = match web::block(move || tempfile::NamedTempFile::new_in(spool_dir)).await {
            Ok(Ok(t)) => t,
            other => {
                log::error!("{path}: failed to create spool file: {other:?}");
                return Err(internal_error());
            }
        };
        let mut size: u64 = 0;
        while let Some(chunk) = self.next_chunk(field).await? {
            size += chunk.len() as u64;
            tmp = match web::block(move || {
                let mut t = tmp;
                t.write_all(&chunk).map(|_| t)
            })
            .await
            {
                Ok(Ok(t)) => t,
                other => {
                    log::error!("{path}: failed to write spool file: {other:?}");
                    return Err(internal_error());
                }
            };
        }
        match web::block(move || tmp.into_temp_path().keep()).await {
            Ok(Ok(kept)) => Ok((kept, size)),
            other => {
                log::error!("{path}: failed to keep spool file: {other:?}");
                Err(internal_error())
            }
        }
    }
}
