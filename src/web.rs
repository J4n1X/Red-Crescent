//! The HTTP layer: one catch-all handler that resolves the request path
//! safely inside the serve directory, then either renders a `.lhtml` template
//! -- on a blocking thread, or on the worker once it has proven fast -- or
//! serves a static file. Multipart uploads are
//! streamed to the spool directory before rendering; leftovers are deleted
//! after the request.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::cookie::{Cookie, SameSite};
use actix_web::http::StatusCode;
use actix_web::http::header::{
    CONTENT_LENGTH, CONTENT_TYPE, ContentDisposition, EXPECT, HeaderName, HeaderValue,
};
use actix_web::{HttpRequest, HttpResponse, web};
use futures_util::StreamExt;

use crate::api::html_escape;
use crate::config::Config;
use crate::runtime::{
    RenderConfig, RenderError, RenderedResponse, RequestBody, RequestData, SendFileSpec,
    UploadedFile, render,
};

pub struct AppState {
    pub config: Config,
    /// Canonicalized at startup; every served path must stay under it.
    pub serve_dir: PathBuf,
    /// Canonicalized writable sandbox for sqlite/send_file/uploads.
    pub data_dir: PathBuf,
    /// `<data-dir>/.spool` — where multipart file parts land first.
    pub spool_dir: PathBuf,
    /// Template rendered when a path resolves to nothing, canonicalized at
    /// startup. `None` — the default — makes an unresolved path a 404.
    pub fallback: Option<PathBuf>,
    pub render_cfg: Arc<RenderConfig>,
}

pub async fn handler(
    req: HttpRequest,
    payload: web::Payload,
    data: web::Data<AppState>,
) -> HttpResponse {
    let decoded_path = match percent_encoding::percent_decode_str(req.path()).decode_utf8() {
        Ok(p) if !p.contains('\0') => p.into_owned(),
        _ => return HttpResponse::BadRequest().body("400 Bad Request: invalid path encoding"),
    };

    // Canonicalization below is authoritative; this just rejects the obvious
    // case without touching the filesystem.
    let relative = decoded_path.trim_start_matches('/');
    if relative.split(['/', '\\']).any(|seg| seg == "..") {
        log::trace!("rejected path with parent-directory segment: {decoded_path}");
        return forbidden();
    }

    let mut target = data.serve_dir.join(relative);
    if target.is_dir() {
        target.push(&data.config.index);
    }

    let abs = match target.canonicalize() {
        Ok(abs) if abs.starts_with(&data.serve_dir) => abs,
        Ok(abs) => {
            log::warn!(
                "prevented path escape: {} -> {}",
                decoded_path,
                abs.display()
            );
            return forbidden();
        }
        // Nothing on disk. A configured front controller gets the request
        // instead of a 404, with the original path still in `request.path`.
        Err(_) => match &data.fallback {
            Some(path) => path.clone(),
            None => return not_found(),
        },
    };

    let is_lhtml = abs
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lhtml"));
    if !is_lhtml {
        return serve_static(&req, &data, &abs).await;
    }

    // --- read the body (multipart → spool, everything else → bytes) -----

    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_lowercase);

    let is_multipart = content_type
        .as_deref()
        .is_some_and(|ct| ct.starts_with("multipart/form-data"));
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
        log::warn!(
            "{decoded_path}: rejected upload of {declared} bytes, over the {size_limit} byte limit"
        );
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
        return payload_too_large(size_limit);
    }

    let body = if is_multipart {
        match read_multipart(&req, payload, &data, &decoded_path).await {
            Ok((fields, files)) => RequestBody::Multipart { fields, files },
            Err(resp) => return resp,
        }
    } else {
        match payload.to_bytes_limited(data.config.max_body_size).await {
            Ok(Ok(bytes)) => RequestBody::Raw(bytes.to_vec()),
            Ok(Err(e)) => {
                log::warn!("failed to read request body for {decoded_path}: {e}");
                return HttpResponse::BadRequest().body("400 Bad Request: unreadable body");
            }
            Err(_) => {
                log::warn!(
                    "{decoded_path}: request body exceeded the {} byte limit",
                    data.config.max_body_size
                );
                return payload_too_large(data.config.max_body_size);
            }
        }
    };

    // Paths of spooled upload files: whatever Lua didn't rename away gets
    // deleted after the request, success or failure.
    let spooled: Vec<PathBuf> = match &body {
        RequestBody::Multipart { files, .. } => files.iter().map(|f| f.path.clone()).collect(),
        RequestBody::Raw(_) => Vec::new(),
    };

    let display_name = abs
        .strip_prefix(&data.serve_dir)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| decoded_path.clone());

    let request_data = build_request_data(&req, &decoded_path, content_type, body);
    let dev = data.config.dev;
    let budget = Duration::from_micros(data.config.inline_render_budget_us);

    let outcome = if !budget.is_zero() && runs_inline(&abs) {
        let cfg = &data.render_cfg;
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            timed_render(cfg, &abs, &display_name, &request_data)
        }))
        .map_err(|_| "render panicked".to_string())
    } else {
        let render_cfg = Arc::clone(&data.render_cfg);
        let path = abs.clone();
        web::block(move || timed_render(&render_cfg, &path, &display_name, &request_data))
            .await
            .map_err(|e| e.to_string())
    };

    remove_spooled(spooled).await;

    let result = match outcome {
        Ok(timed) => {
            if !budget.is_zero() {
                record_render(abs, timed.cost, timed.blocked, budget);
            }
            timed.result
        }
        Err(e) => {
            log::error!("render failed for {decoded_path}: {e}");
            if !budget.is_zero() {
                record_render(abs, Duration::MAX, true, budget);
            }
            return internal_error();
        }
    };

    match result {
        Ok(rendered) => {
            let redirected = rendered
                .headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("location"));
            if !redirected && rendered.send_file.is_some() {
                serve_send_file(&req, rendered).await
            } else {
                build_response(rendered)
            }
        }
        Err(e) => render_error_response(&e, &decoded_path, dev),
    }
}

/// A render and what dispatch needs to know about it.
struct Timed {
    result: Result<RenderedResponse, RenderError>,
    /// How long it held its thread: wall time if it waited on anything, CPU
    /// time otherwise, so being preempted on a busy box is not held against it.
    cost: Duration,
    /// Called something that can wait for seconds: `thread.join`, `process.run`.
    blocked: bool,
}

fn timed_render(
    cfg: &RenderConfig,
    abs: &Path,
    display_name: &str,
    request: &RequestData,
) -> Timed {
    crate::api::take_blocking();
    let started = Instant::now();
    let usage = ThreadUsage::now();
    let result = render(cfg, abs, display_name, request);
    let wall = started.elapsed();
    Timed {
        result,
        cost: usage.cost_since(wall),
        blocked: crate::api::take_blocking(),
    }
}

/// CPU time and voluntary context switches of the calling thread.
#[cfg(target_os = "linux")]
struct ThreadUsage {
    cpu: Duration,
    waits: i64,
}

#[cfg(target_os = "linux")]
impl ThreadUsage {
    fn now() -> Self {
        // getrusage's CPU times are tick-derived and jump by milliseconds, so
        // only the switch count comes from it.
        // SAFETY: both calls only write the struct they are handed.
        let (ru, ts) = unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_THREAD, &mut ru);
            let mut ts: libc::timespec = std::mem::zeroed();
            libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
            (ru, ts)
        };
        ThreadUsage {
            cpu: Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32),
            waits: ru.ru_nvcsw,
        }
    }

    fn cost_since(&self, wall: Duration) -> Duration {
        let now = ThreadUsage::now();
        if now.waits > self.waits {
            wall
        } else {
            now.cpu.saturating_sub(self.cpu)
        }
    }
}

/// No per-thread accounting to separate waiting from preemption: wall time.
#[cfg(not(target_os = "linux"))]
struct ThreadUsage;

#[cfg(not(target_os = "linux"))]
impl ThreadUsage {
    fn now() -> Self {
        ThreadUsage
    }

    fn cost_since(&self, wall: Duration) -> Duration {
        wall
    }
}

/// Fast renders needed before a template leaves the pool, doubled per demotion.
const PROMOTE_AFTER: u32 = 16;
const MAX_BACKOFF: u32 = 12;

/// Where one template renders. Handing a render to the blocking pool costs
/// ~25us of CPU and two thread switches, more than a small page takes to
/// render; rendering on the worker instead stalls its other connections for
/// as long as the render runs. So only templates with a record of fast
/// renders run inline, and one slow render sends a template back.
#[derive(Default)]
struct Dispatch {
    inline: bool,
    streak: u32,
    demotions: u32,
    /// Made a blocking call once, so it may again, for longer than any budget.
    pinned: bool,
}

impl Dispatch {
    /// Whether this render sent the template back to the pool.
    fn record(&mut self, cost: Duration, blocked: bool, budget: Duration) -> bool {
        if blocked {
            self.pinned = true;
        }
        if self.pinned || cost > budget {
            let demoted = self.inline;
            if demoted {
                self.demotions = (self.demotions + 1).min(MAX_BACKOFF);
            }
            self.inline = false;
            self.streak = 0;
            return demoted;
        }
        if !self.inline {
            self.streak += 1;
            self.inline = self.streak >= PROMOTE_AFTER << self.demotions;
        }
        false
    }
}

thread_local! {
    /// Per HTTP worker, which is a single thread, so no lock. Each learns alone.
    static DISPATCH: RefCell<HashMap<PathBuf, Dispatch>> = RefCell::new(HashMap::new());
}

fn runs_inline(abs: &Path) -> bool {
    DISPATCH.with(|d| d.borrow().get(abs).is_some_and(|t| t.inline))
}

fn record_render(abs: PathBuf, cost: Duration, blocked: bool, budget: Duration) {
    let demoted = DISPATCH.with(|d| {
        let mut d = d.borrow_mut();
        match d.get_mut(&abs) {
            Some(t) => t.record(cost, blocked, budget),
            None => d
                .entry(abs.clone())
                .or_default()
                .record(cost, blocked, budget),
        }
    });
    if demoted {
        log::debug!(
            "{}: back to the blocking pool after {cost:?}{}",
            abs.display(),
            if blocked { " (blocking call)" } else { "" }
        );
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
/// the reason the Content-Length check above matters so much.
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
async fn drain_multipart_briefly(multipart: &mut actix_multipart::Multipart) {
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
async fn remove_spooled(paths: Vec<PathBuf>) {
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

/// Stream a multipart body: text fields into memory, file parts into the
/// spool directory, bounded by max_upload_size and max_upload_files. On
/// rejection the spooled files are deleted, the rest of the request is
/// briefly drained so the client can read the response, and an error
/// response is returned.
async fn read_multipart(
    req: &HttpRequest,
    payload: web::Payload,
    data: &AppState,
    path: &str,
) -> Result<(Vec<(String, String)>, Vec<UploadedFile>), HttpResponse> {
    let mut multipart = actix_multipart::Multipart::new(req.headers(), payload);
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut files: Vec<UploadedFile> = Vec::new();
    let mut total: usize = 0;
    let cap = data.config.max_upload_size;

    // Set when the request must be rejected; the loop breaks out so the
    // remaining body can be drained before answering.
    let mut rejection: Option<HttpResponse> = None;

    'parts: while let Some(item) = multipart.next().await {
        let mut field = match item {
            Ok(f) => f,
            Err(e) => {
                log::warn!("{path}: malformed multipart body: {e}");
                rejection =
                    Some(HttpResponse::BadRequest().body("400 Bad Request: malformed multipart"));
                break 'parts;
            }
        };
        let name = field.name().unwrap_or_default().to_string();
        let filename = field
            .content_disposition()
            .and_then(|cd| cd.get_filename())
            .map(str::to_string);
        let part_content_type = field.content_type().map(|m| m.to_string());

        if let Some(filename) = filename {
            // File part → spool it.
            if files.len() >= data.config.max_upload_files {
                log::warn!(
                    "{path}: upload rejected, more than {} files (--max-upload-files)",
                    data.config.max_upload_files
                );
                rejection = Some(payload_too_large_files(data.config.max_upload_files));
                break 'parts;
            }
            let spool_dir = data.spool_dir.clone();
            let mut tmp = match web::block(move || tempfile::NamedTempFile::new_in(spool_dir)).await
            {
                Ok(Ok(t)) => t,
                other => {
                    log::error!("{path}: failed to create spool file: {other:?}");
                    rejection = Some(internal_error());
                    break 'parts;
                }
            };
            let mut size: u64 = 0;

            while let Some(chunk) = field.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("{path}: upload interrupted: {e}");
                        rejection = Some(
                            HttpResponse::BadRequest().body("400 Bad Request: upload interrupted"),
                        );
                        break 'parts;
                    }
                };
                total += chunk.len();
                size += chunk.len() as u64;
                if total > cap {
                    log::warn!(
                        "{path}: upload rejected, body exceeds {cap} bytes (--max-upload-size)"
                    );
                    rejection = Some(payload_too_large(cap));
                    break 'parts;
                }
                tmp = match web::block(move || {
                    let mut t = tmp;
                    t.write_all(&chunk).map(|_| t)
                })
                .await
                {
                    Ok(Ok(t)) => t,
                    other => {
                        log::error!("{path}: failed to write spool file: {other:?}");
                        rejection = Some(internal_error());
                        break 'parts;
                    }
                };
            }

            let path_result = web::block(move || tmp.into_temp_path().keep()).await;
            let spooled_path = match path_result {
                Ok(Ok(p)) => p,
                other => {
                    log::error!("{path}: failed to keep spool file: {other:?}");
                    rejection = Some(internal_error());
                    break 'parts;
                }
            };
            files.push(UploadedFile {
                field: name,
                filename,
                content_type: part_content_type,
                size,
                path: spooled_path,
            });
        } else {
            // Text field → memory.
            let mut value: Vec<u8> = Vec::new();
            while let Some(chunk) = field.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("{path}: upload interrupted: {e}");
                        rejection = Some(
                            HttpResponse::BadRequest().body("400 Bad Request: upload interrupted"),
                        );
                        break 'parts;
                    }
                };
                total += chunk.len();
                if total > cap {
                    log::warn!(
                        "{path}: upload rejected, body exceeds {cap} bytes (--max-upload-size)"
                    );
                    rejection = Some(payload_too_large(cap));
                    break 'parts;
                }
                value.extend_from_slice(&chunk);
            }
            fields.push((name, String::from_utf8_lossy(&value).into_owned()));
        }
    }

    if let Some(response) = rejection {
        remove_spooled(files.into_iter().map(|f| f.path).collect()).await;
        drain_multipart_briefly(&mut multipart).await;
        return Err(response);
    }

    Ok((fields, files))
}

async fn serve_send_file(req: &HttpRequest, rendered: RenderedResponse) -> HttpResponse {
    let SendFileSpec {
        path,
        download_name,
        content_type,
    } = rendered.send_file.expect("checked by caller");

    let mut file = match actix_files::NamedFile::open_async(&path).await {
        Ok(f) => f,
        Err(e) => {
            log::error!("send_file failed to open {}: {e}", path.display());
            return error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                None,
            );
        }
    };
    if let Some(name) = download_name {
        let safe: String = name
            .chars()
            .map(|c| if c.is_control() || c == '"' { '_' } else { c })
            .collect();
        file = file.set_content_disposition(ContentDisposition::attachment(safe));
    }

    let mut resp = file.into_response(req);

    if let Some(ct) = content_type
        && let Ok(value) = HeaderValue::try_from(ct.as_str())
    {
        resp.headers_mut().insert(CONTENT_TYPE, value);
    }
    for (name, value) in rendered.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            resp.headers_mut().insert(n, v);
        }
    }
    for cookie in rendered.cookies {
        if let Err(e) = resp.add_cookie(&to_actix_cookie(cookie)) {
            log::warn!("send_file: failed to attach cookie: {e}");
        }
    }
    resp
}

async fn serve_static(req: &HttpRequest, data: &AppState, abs: &Path) -> HttpResponse {
    if !data.config.static_files {
        return not_found();
    }
    // Never serve server-side source or hidden files.
    if abs
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lua"))
    {
        return not_found();
    }
    let hidden = abs
        .strip_prefix(&data.serve_dir)
        .map(|rel| {
            rel.components().any(|c| match c {
                Component::Normal(name) => name.to_string_lossy().starts_with('.'),
                _ => false,
            })
        })
        .unwrap_or(true);
    if hidden {
        return not_found();
    }

    match actix_files::NamedFile::open_async(abs).await {
        Ok(file) => file.into_response(req),
        Err(e) => {
            log::trace!("static file open failed for {}: {e}", abs.display());
            not_found()
        }
    }
}

fn build_request_data(
    req: &HttpRequest,
    decoded_path: &str,
    content_type: Option<String>,
    body: RequestBody,
) -> RequestData {
    // Collapse repeated headers into one comma-joined value, names lowercased.
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in req.headers() {
        let Ok(value) = value.to_str() else { continue };
        headers
            .entry(name.as_str().to_lowercase())
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(value);
            })
            .or_insert_with(|| value.to_string());
    }

    let cookies = req
        .cookies()
        .map(|list| {
            list.iter()
                .map(|c| (c.name().to_string(), c.value().to_string()))
                .collect()
        })
        .unwrap_or_default();

    RequestData {
        remote_addr: req.peer_addr().map(|addr| addr.ip().to_string()),
        method: req.method().to_string(),
        path: decoded_path.to_string(),
        query_string: req.query_string().to_string(),
        headers: headers.into_iter().collect(),
        cookies,
        content_type,
        body,
    }
}

fn build_response(rendered: RenderedResponse) -> HttpResponse {
    let status = StatusCode::from_u16(rendered.status).unwrap_or_else(|_| {
        log::warn!(
            "invalid status code from Lua: {}, using 200",
            rendered.status
        );
        StatusCode::OK
    });

    let mut builder = HttpResponse::build(status);
    let mut content_type_set = false;
    for (name, value) in rendered.headers {
        if name.eq_ignore_ascii_case("content-type") {
            content_type_set = true;
        }
        builder.insert_header((name, value));
    }
    if !content_type_set {
        builder.content_type("text/html; charset=utf-8");
    }

    for cookie in rendered.cookies {
        builder.cookie(to_actix_cookie(cookie));
    }

    builder.body(rendered.body)
}

fn to_actix_cookie(c: crate::runtime::LuaCookie) -> Cookie<'static> {
    let mut builder = Cookie::build(c.name, c.value);
    if let Some(path) = c.path {
        builder = builder.path(path);
    }
    if let Some(domain) = c.domain {
        builder = builder.domain(domain);
    }
    if let Some(seconds) = c.max_age {
        builder = builder.max_age(actix_web::cookie::time::Duration::seconds(seconds));
    }
    if c.http_only {
        builder = builder.http_only(true);
    }
    if c.secure {
        builder = builder.secure(true);
    }
    if let Some(same_site) = c.same_site {
        builder = builder.same_site(match same_site.to_lowercase().as_str() {
            "strict" => SameSite::Strict,
            "none" => SameSite::None,
            _ => SameSite::Lax,
        });
    }
    builder.finish()
}

fn render_error_response(err: &RenderError, path: &str, dev: bool) -> HttpResponse {
    match err {
        RenderError::NotFound => not_found(),
        RenderError::Timeout => {
            log::error!("{path}: {err}");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                dev.then_some("Template execution timed out."),
            )
        }
        RenderError::Memory => {
            log::error!("{path}: {err}");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                dev.then_some("Template exceeded the memory limit."),
            )
        }
        RenderError::Io(_) | RenderError::Parse { .. } | RenderError::Lua(_) => {
            log::error!("{path}: {err}");
            let detail = format!("{err}");
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                dev.then_some(detail.as_str()),
            )
        }
    }
}

fn not_found() -> HttpResponse {
    error_page(StatusCode::NOT_FOUND, "Not Found", None)
}

fn forbidden() -> HttpResponse {
    error_page(StatusCode::FORBIDDEN, "Forbidden", None)
}

/// 413 pages state the limit even outside dev mode: it is a client-actionable
/// constraint, not an internal detail, and silence here is what makes an
/// oversized upload look like a mysterious connection failure.
fn payload_too_large(limit_bytes: usize) -> HttpResponse {
    error_page(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Payload Too Large",
        Some(&format!(
            "This request is larger than the server's limit of {} ({} bytes).",
            human_bytes(limit_bytes),
            limit_bytes
        )),
    )
}

fn payload_too_large_files(limit: usize) -> HttpResponse {
    error_page(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Payload Too Large",
        Some(&format!(
            "This request contains more than the server's limit of {limit} files."
        )),
    )
}

fn human_bytes(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = UNITS[0];
    for next in UNITS.iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.0} {unit}")
}

fn internal_error() -> HttpResponse {
    error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal Server Error",
        None,
    )
}

fn error_page(status: StatusCode, title: &str, detail: Option<&str>) -> HttpResponse {
    let detail_html = detail
        .map(|d| format!("<pre>{}</pre>", html_escape(d)))
        .unwrap_or_default();
    HttpResponse::build(status)
        .content_type("text/html; charset=utf-8")
        .body(format!(
            "<!DOCTYPE html><html><head><title>{code} {title}</title></head>\
             <body style=\"font-family:sans-serif;max-width:48rem;margin:3rem auto\">\
             <h1>{code} {title}</h1>{detail_html}</body></html>",
            code = status.as_u16(),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Duration = Duration::from_millis(1);
    const FAST: Duration = Duration::from_micros(100);
    const SLOW: Duration = Duration::from_millis(5);

    fn promoted(d: &mut Dispatch) -> u32 {
        let mut renders = 0;
        while !d.inline {
            d.record(FAST, false, BUDGET);
            renders += 1;
            assert!(renders < 1 << 20, "never promoted");
        }
        renders
    }

    #[test]
    fn a_run_of_fast_renders_promotes() {
        let mut d = Dispatch::default();
        assert!(!d.inline, "a template starts on the pool");
        assert_eq!(promoted(&mut d), PROMOTE_AFTER);
    }

    #[test]
    fn a_slow_render_demotes_and_backs_off() {
        let mut d = Dispatch::default();
        promoted(&mut d);
        d.record(SLOW, false, BUDGET);
        assert!(!d.inline);
        assert_eq!(promoted(&mut d), PROMOTE_AFTER * 2);
        d.record(SLOW, false, BUDGET);
        assert_eq!(promoted(&mut d), PROMOTE_AFTER * 4);
    }

    /// Nothing was stalled, so there is nothing to back off from.
    #[test]
    fn a_slow_render_on_the_pool_only_restarts_the_run() {
        let mut d = Dispatch::default();
        for _ in 0..PROMOTE_AFTER - 1 {
            d.record(FAST, false, BUDGET);
        }
        d.record(SLOW, false, BUDGET);
        assert_eq!(promoted(&mut d), PROMOTE_AFTER);
    }

    /// Idle CPU-wise, but it held the thread: that is what stalls a worker.
    #[test]
    fn waiting_is_charged_as_wall_time() {
        let usage = ThreadUsage::now();
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        assert!(usage.cost_since(started.elapsed()) >= Duration::from_millis(20));
    }

    #[test]
    fn a_blocking_call_pins_to_the_pool() {
        let mut d = Dispatch::default();
        promoted(&mut d);
        d.record(FAST, true, BUDGET);
        for _ in 0..1 << 20 {
            d.record(FAST, false, BUDGET);
        }
        assert!(!d.inline);
    }
}
