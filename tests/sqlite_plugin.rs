//! The sqlite plugin in plugins/examples/sqlite, held to the built-in's own
//! fixtures. It loads as `sqlite3`, since the built-in keeps `sqlite`.

mod common;

use actix_web::http::StatusCode;
use actix_web::test as atest;

use common::{app_with, body_string, plugin_path, test_config};
use red_crescent::plugins::PluginSetting;

/// A serve directory with `pages`, each with `sqlite.` rewritten to
/// `sqlite3.`, and a config that loads the plugin with `options`.
fn setup(
    pages: &[(&str, &str)],
    options: Option<serde_json::Value>,
) -> (tempfile::TempDir, red_crescent::config::Config) {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in pages {
        std::fs::write(dir.path().join(name), body.replace("sqlite.", "sqlite3.")).unwrap();
    }
    let mut config = test_config(dir.path().to_str().unwrap());
    config.plugins.insert(
        "sqlite3".to_string(),
        PluginSetting {
            path: plugin_path("sqlite"),
            options,
        },
    );
    (dir, config)
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("tests/fixtures/{name}")).unwrap()
}

async fn get<S>(app: &S, uri: &str) -> String
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = atest::call_service(app, atest::TestRequest::get().uri(uri).to_request()).await;
    let status = resp.status();
    let body = body_string(resp).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

/// tests/platform.rs `sqlite_round_trips_types_and_params`, word for word.
#[actix_web::test]
async fn round_trips_types_and_params() {
    let page = fixture("sqlite_page.lhtml");
    let (_dir, config) = setup(&[("p.lhtml", &page)], None);
    let (app, _) = app_with(config).await;
    let body = get(&app, "/p.lhtml").await;
    assert!(body.contains("rowid=1;changes=1;"), "body: {body}");
    assert!(
        body.contains("count=1;title=hello & goodbye;weight=2.5;bloblen=9;"),
        "body: {body}"
    );
    assert!(body.contains("empty=0;"), "body: {body}");
    assert!(body.contains("null_ok=true"), "body: {body}");
}

/// tests/platform.rs `an_open_transaction_does_not_survive_into_the_next_request`.
#[actix_web::test]
async fn an_open_transaction_does_not_survive_into_the_next_request() {
    let page = fixture("txn_leak.lhtml");
    let (_dir, config) = setup(&[("t.lhtml", &page)], None);
    let (app, _) = app_with(config).await;
    for _ in 0..15 {
        let body = get(&app, "/t.lhtml?mode=leak").await;
        assert!(body.contains("left-open"), "body: {body}");
    }
    for _ in 0..15 {
        let body = get(&app, "/t.lhtml?mode=read").await;
        assert!(body.contains("count=0"), "uncommitted row survived: {body}");
    }
}

#[actix_web::test]
async fn reuse_can_be_turned_off() {
    let page = fixture("txn_leak.lhtml");
    let options = serde_json::json!({ "idle_connections": 0 });
    let (_dir, config) = setup(&[("t.lhtml", &page)], Some(options));
    let (app, _) = app_with(config).await;
    for _ in 0..5 {
        let body = get(&app, "/t.lhtml?mode=leak").await;
        assert!(body.contains("left-open"), "body: {body}");
    }
    let body = get(&app, "/t.lhtml?mode=read").await;
    assert!(body.contains("count=0"), "body: {body}");
}

/// The same null as the built-in and `json`, so a template can switch over.
/// Not the same files at once, though: two copies of SQLite in one process
/// must never open one database, since POSIX locks are per process.
#[actix_web::test]
async fn shares_the_platform_null() {
    let page = r#"<?lua
local db = sqlite3.open("n.db")
db:execute("CREATE TABLE t (v TEXT)")
db:execute("INSERT INTO t VALUES (?)", { json.decode("[null]")[1] })
print(sqlite3.NULL == sqlite.NULL, db:query("SELECT v FROM t")[1].v == nil,
      tostring(db):match("^sqlite3%.Connection") ~= nil)
?>"#;
    let (_dir, config) = setup(&[], None);
    std::fs::write(config.serve_dir.join("p.lhtml"), page).unwrap();
    let (app, _) = app_with(config).await;
    assert_eq!(get(&app, "/p.lhtml").await, "true true true");
}

#[actix_web::test]
async fn errors_name_the_call() {
    let page = r#"<?lua
local function err(f, ...)
    return (tostring(select(2, pcall(f, ...))):match("^runtime error: ([^\n]*)"))
end
local db = sqlite.open("e.db")
local closed = sqlite.open("e.db")
closed:close()
print(table.concat({
    err(sqlite.open, "../outside.db"),
    err(db.query, db, "SELEKT 1"),
    err(db.execute, db, "SELECT ?", { {} }),
    err(closed.query, closed, "SELECT 1"),
}, "|"))
?>"#;
    let (_dir, config) = setup(&[("p.lhtml", page)], None);
    let (app, _) = app_with(config).await;
    let body = get(&app, "/p.lhtml").await;
    let lines: Vec<&str> = body.split('|').collect();
    assert_eq!(
        lines[0],
        "sqlite3.open: path must be relative and inside the data directory: ../outside.db"
    );
    assert!(
        lines[1].starts_with("sqlite3:Connection.query: near \"SELEKT\""),
        "{}",
        lines[1]
    );
    assert_eq!(
        lines[2],
        "sqlite3:Connection.execute: unsupported sqlite parameter type: table"
    );
    assert_eq!(
        lines[3],
        "sqlite3:Connection.query: attempt to use a closed sqlite connection"
    );
}
