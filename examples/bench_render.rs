//! Portable render benchmark: `cargo run --release --example bench_render [iters]`.
//!
//! Times the render path in process, with no HTTP and no load generator, so it
//! runs anywhere the server itself builds -- including a cross-compiled Windows
//! binary, where `profile_render` cannot go because `pprof` is unix-only.
//!
//! The templates are embedded and interpolate only digits and plain words, so
//! output is byte-identical across backends and across every version since the
//! escaping change in 125db38. That is what makes two runs comparable.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use red_crescent::runtime::*;
use red_crescent::template::TemplateCache;
use red_crescent::threads::ThreadRegistry;

const HELLO: &str = "<!DOCTYPE html><html><head><title>Hello</title></head><body>\n\
                     <h1>Hello <?lua= request.query.name or \"world\" ?></h1>\n\
                     <p>Served by Red Crescent.</p>\n</body></html>\n";

const LIST: &str = r#"<?lua local rows = tonumber(request.query.rows) or 0 ?><!DOCTYPE html>
<html><head><title>List</title></head><body><table>
<?lua for i = 1, rows do ?><tr><td><?lua= i ?></td><td><?lua= "Item " .. i ?></td><td><?lua= string.format("%.2f", i * 1.5) ?></td></tr>
<?lua end ?></table></body></html>
"#;

fn config(dir: &Path) -> RenderConfig {
    let serve_dir = dir.canonicalize().unwrap();
    let data_dir = serve_dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    RenderConfig {
        id: ConfigId::new(),
        serve_dir,
        data_dir,
        cache: Arc::new(TemplateCache::new(true)),
        limits: Limits {
            timeout: Duration::from_secs(60),
            memory_bytes: 256 << 20,
        },
        max_body_size: 4096,
        max_upload_size: 4096,
        max_upload_files: 4,
        threads: Arc::new(ThreadRegistry::new()),
        sqlite_idle_connections: 0,
        thread_limits: ThreadLimits {
            timeout: None,
            memory_bytes: 1 << 20,
        },
        c_module_path: None,
        pool: true,
        pool_max_requests: 100_000_000,
    }
}

fn request(path: &str, query: &str) -> RequestData {
    RequestData {
        method: "GET".into(),
        path: path.into(),
        query_string: query.into(),
        headers: Vec::new(),
        cookies: Vec::new(),
        content_type: None,
        body: RequestBody::Raw(Vec::new()),
        remote_addr: None,
    }
}

/// One case: how many renders to time, and how big the page is.
fn measure(cfg: &RenderConfig, page: &Path, name: &str, query: &str, iters: u32) -> (f64, usize) {
    let req = request(&format!("/{name}"), query);
    for _ in 0..50 {
        render(cfg, page, None, name, &req).expect("warm-up render failed");
    }
    let bytes = render(cfg, page, None, name, &req).unwrap().body.len();
    let start = Instant::now();
    for _ in 0..iters {
        render(cfg, page, None, name, &req).expect("render failed");
    }
    let us = start.elapsed().as_secs_f64() * 1e6 / iters as f64;
    (us, bytes)
}

fn main() {
    // Scales every case together; the default takes a handful of seconds.
    let budget: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20_000);

    let dir = std::env::temp_dir().join(format!("rc-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.lhtml"), HELLO).unwrap();
    std::fs::write(dir.join("list.lhtml"), LIST).unwrap();

    let cfg = config(&dir);
    let hello: PathBuf = dir.join("hello.lhtml").canonicalize().unwrap();
    let list: PathBuf = dir.join("list.lhtml").canonicalize().unwrap();

    let backend = if cfg!(feature = "luajit") {
        "luajit"
    } else {
        "lua54"
    };
    println!(
        "Red Crescent bench -- {} on {}, {} cores",
        backend,
        std::env::consts::OS,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    println!();
    println!(
        "{:<14} {:>12} {:>14} {:>12}",
        "page", "us/render", "renders/sec", "bytes"
    );
    println!("{}", "-".repeat(56));

    // Divisors keep each case at roughly the same wall time as the first.
    let cases: [(&str, &Path, &str, &str, u32); 4] = [
        ("hello", hello.as_path(), "hello.lhtml", "name=Janick", 1),
        ("list x100", list.as_path(), "list.lhtml", "rows=100", 4),
        ("list x1000", list.as_path(), "list.lhtml", "rows=1000", 30),
        ("list x5000", list.as_path(), "list.lhtml", "rows=5000", 140),
    ];

    for (label, page, name, query, divisor) in cases {
        let iters = (budget / divisor).max(200);
        let (us, bytes) = measure(&cfg, page, name, query, iters);
        println!(
            "{:<14} {:>12.1} {:>14.0} {:>12}",
            label,
            us,
            1e6 / us,
            bytes
        );
    }

    println!();
    println!("Lower us/render is better. Compare only runs from the same machine.");
    let _ = std::fs::remove_dir_all(&dir);
}
