use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer, middleware, web};
use clap::Parser;

use red_crescent::config::{Config, Settings, load_file_config};
use red_crescent::runtime::{ConfigId, Limits, RenderConfig, ThreadLimits};
use red_crescent::template::TemplateCache;
use red_crescent::threads::{self, ThreadRegistry};
use red_crescent::web::{AppState, handler};
use red_crescent::{resolve_c_module_dirs, resolve_fallback, setup_data_dir};

/// Startup problems are the operator's to fix, so they get a plain message
/// rather than a debug-formatted error struct.
fn fatal(message: impl std::fmt::Display) -> ! {
    eprintln!("Red Crescent cannot start: {message}");
    std::process::exit(1);
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let cli = Settings::parse();

    pretty_env_logger::formatted_builder()
        .parse_filters(&std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();

    // The serve directory has to be known first: the config file lives in it.
    let serve_dir = cli.serve_dir();
    let serve_dir = serve_dir.canonicalize().unwrap_or_else(|e| {
        fatal(format!(
            "serve directory '{}' is not usable: {e}",
            serve_dir.display()
        ))
    });

    let file_config = match cli.config_path(&serve_dir) {
        Some(path) => {
            let loaded = load_file_config(&path).unwrap_or_else(|e| fatal(e));
            if path.exists() {
                log::info!("loaded configuration from {}", path.display());
            }
            loaded
        }
        None => Default::default(),
    };
    let config = Config::resolve(&cli, file_config);

    let (data_dir, spool_dir) =
        setup_data_dir(&config.data_dir, &serve_dir).unwrap_or_else(|e| fatal(e));

    let fallback =
        resolve_fallback(config.fallback.as_deref(), &serve_dir).unwrap_or_else(|e| fatal(e));

    // Enabling native modules takes every Lua state out of safe mode, so it is
    // announced rather than left to the config file.
    let c_module_path =
        resolve_c_module_dirs(&config.c_module_dirs, &serve_dir).unwrap_or_else(|e| fatal(e));
    // Without the feature the Lua C API is not exported, so a `require` would
    // die on "undefined symbol" mid-request. Refuse at boot instead.
    #[cfg(not(feature = "c-modules"))]
    if c_module_path.is_some() {
        fatal(
            "c_module_dirs is set, but this binary was built without native module support.\n\
             Rebuild with: cargo build --release --features c-modules",
        );
    }
    if c_module_path.is_some() {
        let dirs: Vec<String> = config
            .c_module_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        log::warn!(
            "native Lua C modules ENABLED from {} — they run as the server user with no sandbox: \
             the execution timeout cannot fire inside a C call, the memory limit does not see a \
             module's own allocations, and module state is shared across all requests because \
             dlopen caches per process, not per Lua state",
            dirs.join(", ")
        );
    }

    let render_cfg = Arc::new(RenderConfig {
        id: ConfigId::new(),
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
        // 0 reads as "no deadline", same as leaving it unset.
        sqlite_idle_connections: config.sqlite_idle_connections,
        thread_limits: ThreadLimits {
            timeout: config
                .thread_timeout_ms
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis),
            memory_bytes: config.thread_memory_limit_mb * 1024 * 1024,
        },
        c_module_path: c_module_path.map(Arc::from),
        pool: config.lua_pool,
        pool_max_requests: config.lua_pool_max_requests,
    });

    // Boot-time background threads: validate every script first (fail fast),
    // then spawn each under its own path as the thread name.
    for rel in &config.threads {
        threads::resolve_script(&serve_dir, rel).unwrap_or_else(|e| fatal(e));
    }
    for rel in &config.threads {
        threads::spawn_named((*render_cfg).clone(), rel, rel, None).unwrap_or_else(|e| fatal(e));
    }

    log::info!(
        "Red Crescent listening on http://{} serving {} (data: {}){}",
        config.bind,
        serve_dir.display(),
        data_dir.display(),
        if config.dev { " (dev mode)" } else { "" }
    );

    if let Some(path) = &fallback {
        log::info!(
            "paths that resolve to no file fall back to {}",
            path.display()
        );
    }

    let state = web::Data::new(AppState {
        serve_dir,
        data_dir,
        spool_dir,
        fallback,
        render_cfg,
        config: config.clone(),
    });

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .wrap(middleware::Logger::default())
            .default_service(web::route().to(handler))
    })
    .workers(config.workers)
    // Streamed bodies (static files, send_file) write headers and data
    // separately; without TCP_NODELAY the second write waits ~40ms for the
    // client's delayed ACK on every kept-alive connection.
    .on_connect(|conn, _ext| {
        if let Some(stream) = conn.downcast_ref::<actix_web::rt::net::TcpStream>()
            && let Err(e) = stream.set_nodelay(true)
        {
            log::debug!("failed to set TCP_NODELAY: {e}");
        }
    })
    .bind(&config.bind)?
    .run()
    .await
}
