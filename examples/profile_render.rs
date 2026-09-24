//! Where a render spends its time: `cargo run --release --example
//! profile_render [rows] [iters]`, which writes a flamegraph to /tmp and
//! prints the heaviest leaf frames. `NOPROF=1` times without sampling.
//!
//! pprof's unwinder aborts on LuaJIT's generated code, so the flamegraph is
//! lua54-only; `NOPROF=1` works on both. Unix only, like pprof.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use red_crescent::runtime::*;
use red_crescent::template::TemplateCache;
use red_crescent::threads::ThreadRegistry;

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

fn request(query: &str) -> RequestData {
    RequestData {
        method: "GET".into(),
        path: "/list.lhtml".into(),
        query_string: query.into(),
        headers: Vec::new(),
        cookies: Vec::new(),
        content_type: None,
        body: RequestBody::Raw(Vec::new()),
        remote_addr: None,
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("profile_render needs pprof, which is unix-only; use bench_render");
}

#[cfg(unix)]
fn main() {
    let rows = std::env::args().nth(1).unwrap_or_else(|| "1000".into());
    let iters: u32 = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "3000".into())
        .parse()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("list.lhtml"), LIST).unwrap();
    let cfg = config(dir.path());
    let page: PathBuf = dir.path().join("list.lhtml").canonicalize().unwrap();
    let q = format!("rows={rows}");

    for _ in 0..50 {
        render(&cfg, &page, None, "list.lhtml", &request(&q)).unwrap();
    }

    let profile = std::env::var("NOPROF").is_err();
    let guard = profile.then(|| {
        pprof::ProfilerGuardBuilder::default()
            .frequency(2000)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .unwrap()
    });

    let t = std::time::Instant::now();
    for _ in 0..iters {
        render(&cfg, &page, None, "list.lhtml", &request(&q)).unwrap();
    }
    let us = t.elapsed().as_secs_f64() * 1e6 / iters as f64;

    let backend0 = if cfg!(feature = "luajit") {
        "luajit"
    } else {
        "lua54"
    };
    eprintln!("{backend0} rows={rows}: {us:.1} us/render");
    let Some(guard) = guard else { return };
    let report = guard.report().build().unwrap();
    let backend = if cfg!(feature = "luajit") {
        "luajit"
    } else {
        "lua54"
    };
    let name = format!("/tmp/flame-{backend}-{rows}.svg");
    let f = std::fs::File::create(&name).unwrap();
    report.flamegraph(f).unwrap();
    eprintln!("{backend} rows={rows}: {us:.1} us/render -> {name}");

    // Also print the heaviest leaf frames, so it is readable without the SVG.
    let mut totals: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut all = 0usize;
    for (frames, count) in report.data.iter() {
        all += *count as usize;
        if let Some(sym) = frames.frames.first().and_then(|f| f.first()) {
            *totals.entry(format!("{sym}")).or_default() += *count as usize;
        }
    }
    let mut v: Vec<_> = totals.into_iter().collect();
    v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    eprintln!("  {:>6}  leaf frame", "self%");
    for (sym, c) in v.into_iter().take(18) {
        eprintln!("  {:>5.1}%  {}", 100.0 * c as f64 / all as f64, sym);
    }
}

const LIST: &str = r#"<?lua local rows = tonumber(request.query.rows) or 0 ?><!DOCTYPE html>
<html><head><title>List</title></head><body><table>
<?lua for i = 1, rows do ?><tr><td><?lua= i ?></td><td><?lua= "Item <" .. i .. "> & co" ?></td><td><?lua= string.format("%.2f", i * 1.5) ?></td></tr>
<?lua end ?></table></body></html>
"#;
