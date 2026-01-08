use std::collections::HashMap;
use std::path::Path;

use crate::config::SERVE_DIR;
use crate::lua_runtime::{LuaRequest, ManagedLuaInstance, process_lhtml};
use actix_web::{HttpRequest, HttpResponse, Responder, web};

// Theoretically, we could make one single Lua instance handle multiple
// requests, like a worker pool, for that, we'd use mutexes and better
// dispatch. For now, it's a new instance per request.
async fn process_request(
    data: web::Data<ManagedLuaInstance>,
    req: HttpRequest,
    query: HashMap<String, String>,
    body: Option<HashMap<String, String>>,
) -> actix_web::Result<impl Responder> {
    let path = req.path();

    if !Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lhtml"))
    {
        return Ok(HttpResponse::BadRequest().body("400 Bad Request: Only .lhtml files are served"));
    }

    // Sanitize paths passed to prevent directory traversal
    let serve_dir_path = match Path::new(SERVE_DIR).canonicalize() {
        Ok(path) => path,
        Err(e) => {
            log::error!("Failed to canonicalize SERVE_DIR '{SERVE_DIR}': {e}");
            return Ok(HttpResponse::InternalServerError()
                .body("500 Internal Server Error: Misconfigured SERVE_DIR"));
        }
    };
    let relative_path = path.trim_start_matches('/');
    let safe_path = serve_dir_path.join(relative_path);

    let Ok(absolute_path) = safe_path.canonicalize() else {
        log::trace!("Invalid file path: {}", safe_path.display());
        return Ok(HttpResponse::NotFound().body("404 File Not Found"));
    };

    if !absolute_path.starts_with(&serve_dir_path) {
        log::trace!(
            "Prevented attempted directory traversal attack: {}",
            absolute_path.display()
        );
        return Ok(HttpResponse::Forbidden().body("403 Forbidden"));
    }

    let Ok(content) = tokio::fs::read_to_string(&absolute_path).await else {
        log::trace!("File not found: {}", absolute_path.display());
        return Ok(HttpResponse::NotFound().body("404 File Not Found"));
    };

    // Extract request information
    let method = req.method().to_string();
    let path = req.path().to_string();

    // Extract headers
    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(value_str) = value.to_str() {
            headers.insert(name.to_string(), value_str.to_string());
        }
    }

    let lua_request = LuaRequest {
        method: method.clone(),
        path,
        headers,
        query,
        body,
    };

    let instance = data.get_ref();

    // Process the file with the request context
    let result = match process_lhtml(instance, &content, &lua_request) {
        Ok(final_html) => {
            log::trace!("Successfully processed {}", absolute_path.display());
            // Check if Lua set a custom status code or headers
            let globals = instance.lua.globals();
            let mut response = HttpResponse::Ok();
            let mut content_type_set = false;

            if let Ok(response_table) = globals.get::<mlua::Table>("response") {
                // Get status code
                if let Ok(status) = response_table.get::<u16>("status") {
                    log::trace!("Setting status from Lua: {status}");
                    match actix_web::http::StatusCode::from_u16(status) {
                        Ok(code) => {
                            response.status(code);
                        }
                        Err(e) => {
                            log::warn!("Invalid status code from Lua: {status}, using 200 OK: {e}");
                            response.status(actix_web::http::StatusCode::OK);
                        }
                    }
                }

                // Get custom headers
                if let Ok(headers_table) = response_table.get::<mlua::Table>("headers") {
                    for (key, value) in headers_table.pairs::<String, String>().flatten() {
                        log::trace!("Setting header from Lua: {key}: {value}");
                        if key.eq_ignore_ascii_case("content-type") {
                            content_type_set = true;
                        }
                        response.insert_header((key, value));
                    }
                }
            }

            if !content_type_set {
                response.content_type("text/html");
            }
            Ok(response.body(final_html))
        }
        Err(e) => {
            log::error!("Lua error processing {}: {e}", absolute_path.display());
            Ok(HttpResponse::InternalServerError()
                .body(format!("<h1>Lua Runtime Error</h1><p>{e}</p>")))
        }
    };
    instance.reset().unwrap();
    result
}

pub async fn lua_handler(
    data: web::Data<ManagedLuaInstance>,
    req: HttpRequest,
    query: web::Query<HashMap<String, String>>,
) -> actix_web::Result<impl Responder> {
    process_request(data, req, query.into_inner(), None).await
}

pub async fn lua_post_handler(
    data: web::Data<ManagedLuaInstance>,
    req: HttpRequest,
    query: web::Query<HashMap<String, String>>,
    form: web::Form<HashMap<String, String>>,
) -> actix_web::Result<impl Responder> {
    process_request(data, req, query.into_inner(), Some(form.into_inner())).await
}
