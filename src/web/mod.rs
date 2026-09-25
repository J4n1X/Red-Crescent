//! The HTTP layer: one catch-all handler that resolves the request path
//! safely inside the serve directory, then either renders a `.lhtml` template
//! -- on a blocking thread, or on the worker once it has proven fast -- or
//! serves a static file. Multipart uploads are streamed to the spool
//! directory before rendering; leftovers are deleted after the request.

mod body;
mod dispatch;
mod response;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use actix_web::http::header::CONTENT_TYPE;
use actix_web::{HttpRequest, HttpResponse, web};

use crate::config::Config;
use crate::resolve::{Resolved, display_name, is_lhtml, is_private, resolve};
use crate::runtime::{RenderConfig, RequestBody, RequestData};
use response::{forbidden, not_found};

pub struct AppState {
    pub config: Config,
    /// Canonicalized at startup; every served path must stay under it.
    pub serve_dir: PathBuf,
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

    // `resolve` is authoritative; this just rejects the obvious case without
    // touching the filesystem.
    let relative = decoded_path.trim_start_matches('/');
    if relative.split(['/', '\\']).any(|seg| seg == "..") {
        log::trace!("rejected path with parent-directory segment: {decoded_path}");
        return forbidden();
    }

    let index = Some(data.config.index.as_str());
    let static_read = |p: &Path| data.config.static_files && !is_lhtml(p);
    let (abs, meta, file) = match resolve(&data.serve_dir, relative, index, static_read) {
        Resolved::Found(found) => (found.path, found.meta, found.file),
        Resolved::Escaped(abs) => {
            log::warn!(
                "prevented path escape: {} -> {}",
                decoded_path,
                abs.display()
            );
            return forbidden();
        }
        // Nothing on disk. A configured front controller gets the request
        // instead of a 404, with the original path still in `request.path`.
        Resolved::Missing => match &data.fallback {
            Some(path) => (path.clone(), None, None),
            None => return not_found(),
        },
    };

    if !is_lhtml(&abs) {
        return serve_static(&req, &data, &abs, file).await;
    }

    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_lowercase);
    let body =
        match body::read_body(&req, payload, &data, content_type.as_deref(), &decoded_path).await {
            Ok(body) => body,
            Err(response) => return response,
        };

    // Paths of spooled upload files: whatever Lua didn't rename away gets
    // deleted after the request, success or failure.
    let spooled: Vec<PathBuf> = match &body {
        RequestBody::Multipart { files, .. } => files.iter().map(|f| f.path.clone()).collect(),
        RequestBody::Raw(_) => Vec::new(),
    };

    let display_name = display_name(&data.serve_dir, &abs, &decoded_path);
    let request_data = build_request_data(&req, &decoded_path, content_type, body);
    let budget = Duration::from_micros(data.config.inline_render_budget_us);
    let outcome = dispatch::render_dispatched(
        &data.render_cfg,
        abs,
        meta,
        display_name,
        request_data,
        budget,
    )
    .await;

    body::remove_spooled(spooled).await;

    match outcome {
        Ok(Ok(rendered)) => {
            let redirected = rendered
                .headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("location"));
            if !redirected && rendered.send_file.is_some() {
                response::serve_send_file(&req, rendered).await
            } else {
                response::build_response(rendered)
            }
        }
        Ok(Err(e)) => response::render_error_response(&e, &decoded_path, data.config.dev),
        Err(e) => {
            log::error!("render failed for {decoded_path}: {e}");
            response::internal_error()
        }
    }
}

/// Static files smaller than this are read on the async worker instead of
/// hopping to the blocking pool, which cost more than the read itself.
const SYNC_READ_BELOW: u64 = 64 * 1024;

/// `file` is `abs` already opened, if resolving did that.
async fn serve_static(
    req: &HttpRequest,
    data: &AppState,
    abs: &Path,
    file: Option<std::fs::File>,
) -> HttpResponse {
    if !data.config.static_files {
        return not_found();
    }
    if is_private(&data.serve_dir, abs) {
        return not_found();
    }

    let opened = match file {
        Some(file) => Ok(file),
        None => std::fs::File::open(abs),
    }
    .and_then(|file| actix_files::NamedFile::from_file(file, abs));
    match opened {
        Ok(file) => file.read_mode_threshold(SYNC_READ_BELOW).into_response(req),
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
