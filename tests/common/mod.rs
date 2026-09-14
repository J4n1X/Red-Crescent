#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use actix_web::dev::{Service, ServiceResponse};
use actix_web::{App, test, web};

use red_crescent::config::Config;
use red_crescent::runtime::{Limits, RenderConfig, ThreadLimits};
use red_crescent::template::TemplateCache;
use red_crescent::threads::ThreadRegistry;
use red_crescent::web::{AppState, handler};

static NEXT_DATA_DIR: AtomicU64 = AtomicU64::new(0);

/// A unique, throwaway data directory per app instance (under the OS temp
/// dir; cleaned up on reboot like any temp file).
pub fn unique_data_dir() -> PathBuf {
    let n = NEXT_DATA_DIR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rc-test-{}-{n}", std::process::id()))
}

pub fn test_config(serve_dir: &str) -> Config {
    Config {
        bind: "127.0.0.1:0".to_string(),
        serve_dir: serve_dir.into(),
        workers: 1,
        timeout_ms: 400,
        memory_limit_mb: 8,
        max_body_size: 4096,
        data_dir: unique_data_dir(),
        max_upload_size: 64 * 1024,
        max_upload_files: 8,
        threads: Vec::new(),
        // Matches the production default: threads run without a deadline
        // unless a test asks for one.
        thread_timeout_ms: None,
        thread_memory_limit_mb: 8,
        sqlite_idle_connections: 2,
        static_files: true,
        index: "index.lhtml".to_string(),
        dev: false,
        c_module_dirs: Vec::new(),
    }
}

pub async fn app_with(
    config: Config,
) -> (
    impl Service<actix_http::Request, Response = ServiceResponse, Error = actix_web::Error>,
    PathBuf,
) {
    let serve_dir = config
        .serve_dir
        .canonicalize()
        .expect("serve directory must exist");
    let (data_dir, spool_dir) =
        red_crescent::setup_data_dir(&config.data_dir, &serve_dir).expect("data dir setup");

    let render_cfg = Arc::new(RenderConfig {
        serve_dir: serve_dir.clone(),
        data_dir: data_dir.clone(),
        cache: Arc::new(TemplateCache::new(!config.dev)),
        limits: Limits {
            timeout: Duration::from_millis(config.timeout_ms),
            memory_bytes: config.memory_limit_mb * 1024 * 1024,
        },
        max_body_size: config.max_body_size,
        max_upload_size: config.max_upload_size,
        max_upload_files: config.max_upload_files,
        threads: Arc::new(ThreadRegistry::new()),
        sqlite_idle_connections: config.sqlite_idle_connections,
        thread_limits: ThreadLimits {
            timeout: config
                .thread_timeout_ms
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis),
            memory_bytes: config.thread_memory_limit_mb * 1024 * 1024,
        },
        c_module_path: red_crescent::resolve_c_module_dirs(&config.c_module_dirs, &serve_dir)
            .expect("c module directories")
            .map(Arc::from),
    });

    let state = web::Data::new(AppState {
        serve_dir,
        data_dir: data_dir.clone(),
        spool_dir,
        render_cfg,
        config,
    });
    let app = test::init_service(
        App::new()
            .app_data(state)
            .default_service(web::route().to(handler)),
    )
    .await;
    (app, data_dir)
}

pub async fn body_string(resp: ServiceResponse) -> String {
    let bytes = test::read_body(resp).await;
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Hand-build a multipart/form-data body. Each part is (name, filename, content);
/// filename None makes it a plain text field.
pub fn multipart_body(boundary: &str, parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, filename, content) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match filename {
            Some(f) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Find the substring between `pre` and `post` in `hay`.
pub fn find_between<'a>(hay: &'a str, pre: &str, post: &str) -> Option<&'a str> {
    let start = hay.find(pre)? + pre.len();
    let end = hay[start..].find(post)? + start;
    Some(&hay[start..end])
}
