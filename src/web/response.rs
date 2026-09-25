//! Turning a render into an HTTP response, and the server's own error pages.

use actix_web::cookie::{Cookie, SameSite};
use actix_web::http::StatusCode;
use actix_web::http::header::{CONTENT_TYPE, ContentDisposition, HeaderName, HeaderValue};
use actix_web::{HttpRequest, HttpResponse};

use crate::api::html_escape;
use crate::runtime::{LuaCookie, RenderError, RenderedResponse, SendFileSpec};

pub(super) async fn serve_send_file(req: &HttpRequest, rendered: RenderedResponse) -> HttpResponse {
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

pub(super) fn build_response(rendered: RenderedResponse) -> HttpResponse {
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

fn to_actix_cookie(c: LuaCookie) -> Cookie<'static> {
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

pub(super) fn render_error_response(err: &RenderError, path: &str, dev: bool) -> HttpResponse {
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

pub(super) fn not_found() -> HttpResponse {
    error_page(StatusCode::NOT_FOUND, "Not Found", None)
}

pub(super) fn forbidden() -> HttpResponse {
    error_page(StatusCode::FORBIDDEN, "Forbidden", None)
}

/// 413 pages state the limit even outside dev mode: it is a client-actionable
/// constraint, not an internal detail, and silence here is what makes an
/// oversized upload look like a mysterious connection failure.
pub(super) fn payload_too_large(limit_bytes: usize) -> HttpResponse {
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

pub(super) fn payload_too_large_files(limit: usize) -> HttpResponse {
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

pub(super) fn internal_error() -> HttpResponse {
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
