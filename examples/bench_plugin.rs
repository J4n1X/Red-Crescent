//! Built-in `sqlite` against the plugin in plugins/examples/sqlite, same pages,
//! same database, same connection reuse:
//!
//! ```text
//! cargo build --release -p sqlite
//! cargo run --release --example bench_plugin [iters]
//! ```
//!
//! `RC_BENCH_CASE=<text>` runs only the cases whose label contains it, for
//! profiling. The two run in alternating rounds, so drift on the machine hits both
//! equally; each cell is the median round. Compare only within one run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use red_crescent::plugins::PluginSetting;
use red_crescent::runtime::*;
use red_crescent::template::TemplateCache;
use red_crescent::threads::ThreadRegistry;

const ROUNDS: usize = 7;

/// Run once per side, each on its own file: two copies of SQLite in one
/// process must never open the same database (POSIX locks are per process).
const SETUP: &str = r#"<?lua
local db = DB.open("DB.db")
db:execute("DROP TABLE IF EXISTS items")
db:execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, price REAL, note TEXT)")
db:execute("BEGIN")
for i = 1, 1000 do
    db:execute("INSERT INTO items (name, price, note) VALUES (?, ?, ?)",
        { "Item " .. i, i * 1.5, i % 3 == 0 and DB.NULL or "note " .. i })
end
db:execute("COMMIT")
?>"#;

/// (label, body); `DB` is replaced by `sqlite` or `sqlite3`, also in the
/// file name.
const CASES: &[(&str, &str)] = &[
    (
        "open + 1 row",
        r#"<?lua local db = DB.open("DB.db")
local r = db:query("SELECT * FROM items WHERE id = ?", { 42 })[1] ?><?lua= r.name ?>"#,
    ),
    (
        "100 rows",
        r#"<?lua local db = DB.open("DB.db")
for _, r in ipairs(db:query("SELECT * FROM items WHERE id <= ?", { 100 })) do ?><?lua= r.name ?> <?lua end ?>"#,
    ),
    (
        "1000 rows",
        r#"<?lua local db = DB.open("DB.db")
local n = 0
for _, r in ipairs(db:query("SELECT * FROM items")) do n = n + r.price end ?><?lua= n ?>"#,
    ),
    (
        "100 bare execs",
        r#"<?lua local db = DB.open("DB.db")
for i = 1, 100 do db:execute("UPDATE items SET price = 1 WHERE id = 0") end ?>ok"#,
    ),
    (
        "100 executes",
        r#"<?lua local db = DB.open("DB.db")
db:execute("BEGIN")
for i = 1, 100 do db:execute("UPDATE items SET price = ? WHERE id = ?", { i * 2.5, i }) end
db:execute("ROLLBACK") ?>ok"#,
    ),
];

fn config(serve_dir: &Path, plugin: &Path) -> RenderConfig {
    let data_dir = serve_dir.join("../data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_dir = data_dir.canonicalize().unwrap();
    let mut settings = BTreeMap::new();
    settings.insert(
        "sqlite3".to_string(),
        PluginSetting {
            path: plugin.to_path_buf(),
            options: None,
        },
    );
    let plugins = red_crescent::plugins::load(&settings, serve_dir, &data_dir)
        .unwrap_or_else(|e| panic!("{e}\nbuild it first: cargo build --release -p sqlite"));
    RenderConfig {
        id: ConfigId::new(),
        serve_dir: serve_dir.to_path_buf(),
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
        // The plugin's default, so both sides reuse connections alike.
        sqlite_idle_connections: 2,
        thread_limits: ThreadLimits {
            timeout: None,
            memory_bytes: 1 << 20,
        },
        c_module_path: None,
        pool: true,
        plugins,
        pool_max_requests: 100_000_000,
    }
}

fn request(name: &str) -> RequestData {
    RequestData {
        method: "GET".into(),
        path: format!("/{name}"),
        query_string: String::new(),
        headers: Vec::new(),
        cookies: Vec::new(),
        content_type: None,
        body: RequestBody::Raw(Vec::new()),
        remote_addr: None,
    }
}

fn render_once(cfg: &RenderConfig, dir: &Path, name: &str) -> Vec<u8> {
    render(cfg, &dir.join(name), None, name, &request(name), None)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .body
}

/// Microseconds per render over `iters` renders.
fn time(cfg: &RenderConfig, dir: &Path, name: &str, iters: u32) -> f64 {
    let path = dir.join(name);
    let req = request(name);
    let start = Instant::now();
    for _ in 0..iters {
        render(cfg, &path, None, name, &req, None).expect("render failed");
    }
    start.elapsed().as_secs_f64() * 1e6 / iters as f64
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let budget: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_000);
    let plugin: PathBuf = std::env::var_os("RC_SQLITE_PLUGIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/release/sqlite"));

    let root = std::env::temp_dir().join(format!("rc-bench-plugin-{}", std::process::id()));
    let dir = root.join("serve");
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let cfg = config(&dir, &plugin);
    for module in ["sqlite", "sqlite3"] {
        let name = format!("setup_{module}.lhtml");
        std::fs::write(dir.join(&name), SETUP.replace("DB", module)).unwrap();
        render_once(&cfg, &dir, &name);
    }

    println!(
        "{:<14} {:>11} {:>11} {:>8}",
        "page", "built-in", "plugin", "plugin/"
    );
    println!(
        "{:<14} {:>11} {:>11} {:>8}",
        "", "us/render", "us/render", "built-in"
    );
    println!("{}", "-".repeat(47));

    let only = std::env::var("RC_BENCH_CASE").unwrap_or_default();
    for (i, (label, body)) in CASES.iter().enumerate() {
        if !label.contains(only.as_str()) {
            continue;
        }
        let names = [format!("b{i}.lhtml"), format!("p{i}.lhtml")];
        for (name, module) in names.iter().zip(["sqlite", "sqlite3"]) {
            std::fs::write(dir.join(name), body.replace("DB", module)).unwrap();
        }
        let outputs: Vec<Vec<u8>> = names.iter().map(|n| render_once(&cfg, &dir, n)).collect();
        assert_eq!(outputs[0], outputs[1], "{label}: the two disagree");

        // Sized so a case takes about the same time as "open + 1 row".
        let probe = time(&cfg, &dir, &names[0], 50);
        let iters = ((budget as f64 * 20.0 / probe) as u32).clamp(20, budget * 4);
        for n in &names {
            time(&cfg, &dir, n, iters / 4 + 1);
        }
        // RC_BENCH_SIDE=0 or 1 times one side only, for profiling it.
        let sides: Vec<usize> = match std::env::var("RC_BENCH_SIDE").as_deref() {
            Ok("0") => vec![0],
            Ok("1") => vec![1],
            _ => vec![0, 1],
        };
        let mut rounds = [Vec::new(), Vec::new()];
        for _ in 0..ROUNDS {
            for &side in &sides {
                let n = &names[side];
                rounds[side].push(time(&cfg, &dir, n, iters));
            }
        }
        let [built_in, plugin] = rounds.map(median);
        println!(
            "{:<14} {:>11.1} {:>11.1} {:>7.2}x",
            label,
            built_in,
            plugin,
            plugin / built_in
        );
    }

    println!();
    println!("Median of {ROUNDS} alternating rounds. Compare only within one run.");
    let _ = std::fs::remove_dir_all(&root);
}
