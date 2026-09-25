//! Native plugins, through the real handler, with the probe plugin in
//! plugins/tests/probe.

mod common;

use std::path::PathBuf;
use std::sync::OnceLock;

use actix_web::http::StatusCode;
use actix_web::test as atest;

use common::{app_with, body_string, test_config};
use red_crescent::plugins::PluginSetting;
use red_crescent::runtime::{RequestBody, RequestData, render};

/// Builds the probe once per test binary. Its own target directory, so this
/// never waits on the lock the running `cargo test` holds.
fn probe_path() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target = root.join("target/plugin-tests");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet", "-p", "rc-probe", "--target-dir"])
            .arg(&target)
            .current_dir(&root)
            .status()
            .expect("cargo runs");
        assert!(status.success(), "building the probe plugin failed");
        target.join("debug/rc_probe")
    })
    .clone()
}

fn serve_dir_with(pages: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in pages {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    dir
}

fn config_with_probe(dir: &tempfile::TempDir) -> red_crescent::config::Config {
    let mut config = test_config(dir.path().to_str().unwrap());
    config.plugins.insert(
        "probe".to_string(),
        PluginSetting {
            path: probe_path(),
            options: Some(serde_json::json!({ "greeting": "hi" })),
        },
    );
    config
}

/// Renders `page` with the probe loaded and returns the body.
async fn render_page(page: &str) -> String {
    let dir = serve_dir_with(&[("p.lhtml", page)]);
    let (app, _) = app_with(config_with_probe(&dir)).await;
    let resp =
        atest::call_service(&app, atest::TestRequest::get().uri("/p.lhtml").to_request()).await;
    let status = resp.status();
    let body = body_string(resp).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    body
}

#[actix_web::test]
async fn values_round_trip() {
    let body = render_page(
        r##"<?lua
local t, f = {}, function() end
local a, b, c, d, e, g, h = probe.echo(nil, true, 42, 1.5, "x\0y", t, f)
print(a == nil, b, c, d, e == "x\0y", g == t, h == f, select("#", probe.echo()))
?>"##,
    )
    .await;
    assert_eq!(body, "true true 42 1.5 true true true 0");
}

#[actix_web::test]
async fn errors_carry_the_function_name() {
    let body = render_page(
        r#"<?lua
print(tostring(select(2, pcall(probe.fail, "boom"))))
print(tostring(select(2, pcall(probe.bogus))))
?>"#,
    )
    .await;
    assert!(body.contains("probe.fail: boom"), "body: {body}");
    assert!(
        body.contains("probe.bogus: returned RC_PROPAGATE with no failed host call"),
        "body: {body}"
    );
}

#[actix_web::test]
async fn a_plugin_calls_back_into_lua() {
    let body = render_page(
        r#"<?lua
print(probe.apply(function(a, b) return a + b, "r" end, 2, 3))
print(tostring(select(2, pcall(probe.apply, function() error("inner") end))))
?>"#,
    )
    .await;
    assert!(body.starts_with("5 r"), "body: {body}");
    assert!(body.contains("inner"), "body: {body}");
}

/// The error comes back through the plugin as RC_PROPAGATE and is re-raised
/// as itself, so exit() still ends the page cleanly.
#[actix_web::test]
async fn exit_inside_a_callback_is_still_exit() {
    let body = render_page(
        r#"<?lua probe.apply(function() print("before") exit() end) print("after") ?>"#,
    )
    .await;
    assert_eq!(body, "before");
}

#[actix_web::test]
async fn yielding_across_a_plugin_is_refused() {
    let body = render_page(
        r#"<?lua
local co = coroutine.wrap(function()
    probe.apply(function() coroutine.yield(1) end)
end)
local ok, err = pcall(co)
print(ok, tostring(err))
?>"#,
    )
    .await;
    assert!(body.starts_with("false"), "body: {body}");
    assert!(body.contains("yield across"), "body: {body}");
}

#[actix_web::test]
async fn tables_by_reference() {
    let body = render_page(
        r#"<?lua
local seen = {}
probe.each({ a = 1, b = 2 }, function(k, v) seen[k] = v end)
local made = probe.make(3)
print(probe.sum({ 1, 2, 3.5 }), #made, made[3], seen.a, seen.b)
?>"#,
    )
    .await;
    assert_eq!(body, "6.5 3 3 1 2");
}

#[actix_web::test]
async fn write_goes_into_the_page_unescaped() {
    let body = render_page(r#"a<?lua probe.write("<b>") ?>c"#).await;
    assert_eq!(body, "a<b>c");
}

#[actix_web::test]
async fn options_arrive_as_json_and_require_finds_the_table() {
    let body = render_page(r#"<?lua print(probe.options(), require("probe") == probe) ?>"#).await;
    assert_eq!(body, r#"{"greeting":"hi"} true"#);
}

#[actix_web::test]
async fn handles_have_methods_and_are_destroyed() {
    let dir = serve_dir_with(&[
        (
            "make.lhtml",
            r#"<?lua
local c = probe.counter(5)
c:inc() c:inc()
print(c:get(), tostring(c):match("^probe%.Counter") ~= nil, probe.echo(c) == c, probe.destroyed())
probe.counter(1)
?>"#,
        ),
        ("count.lhtml", r#"<?lua print(probe.destroyed()) ?>"#),
    ]);
    let (app, _) = app_with(config_with_probe(&dir)).await;
    let get = |uri: &'static str| atest::TestRequest::get().uri(uri).to_request();

    let made = body_string(atest::call_service(&app, get("/make.lhtml")).await).await;
    let parts: Vec<&str> = made.split(' ').collect();
    assert_eq!(parts[..3], ["7", "true", "true"], "body: {made}");
    let before: i64 = parts[3].parse().unwrap();

    // Both counters are garbage once the request ends; creating them flagged
    // it for a full collection, so they are gone by the next one.
    let after = body_string(atest::call_service(&app, get("/count.lhtml")).await).await;
    assert!(
        after.parse::<i64>().unwrap() >= before + 2,
        "destroyed before {before}, after {after}"
    );
}

/// Renders pages on this thread, one after another, so they share one
/// pooled state: the case the per-request rules exist for.
fn render_in_sequence(pages: &[(&str, &str)], order: &[&str]) -> Vec<String> {
    let dir = serve_dir_with(pages);
    let config = config_with_probe(&dir);
    let serve_dir = dir.path().canonicalize().unwrap();
    let (data_dir, _) = red_crescent::setup_data_dir(&config.data_dir, &serve_dir).unwrap();
    let cfg = common::render_config(&config, &serve_dir, &data_dir);
    order
        .iter()
        .map(|name| {
            let request = RequestData {
                method: "GET".into(),
                path: format!("/{name}"),
                query_string: String::new(),
                headers: Vec::new(),
                cookies: Vec::new(),
                content_type: None,
                body: RequestBody::Raw(Vec::new()),
                remote_addr: None,
            };
            let rendered = render(&cfg, &serve_dir.join(name), None, name, &request, None)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            String::from_utf8(rendered.body).unwrap()
        })
        .collect()
}

/// A retained reference lasts the request, and not a moment longer: the next
/// request on the same pooled state must not reach the previous one's values.
#[test]
fn a_retained_reference_ends_with_its_request() {
    let bodies = render_in_sequence(
        &[
            (
                "keep.lhtml",
                r#"<?lua probe.keep(function() return "kept" end) print(probe.call_kept()) ?>"#,
            ),
            (
                "later.lhtml",
                r#"<?lua print(tostring(select(2, pcall(probe.call_kept)))) ?>"#,
            ),
        ],
        &["keep.lhtml", "later.lhtml"],
    );
    assert_eq!(bodies[0], "kept");
    assert!(
        bodies[1].contains("a stale or foreign reference"),
        "body: {}",
        bodies[1]
    );
}

/// The plugin table is copied per request like `string`, so pooling holds.
#[test]
fn a_request_cannot_break_the_plugin_for_the_next() {
    let bodies = render_in_sequence(
        &[(
            "p.lhtml",
            r#"<?lua print(type(probe.noop)) probe.noop = nil ?>"#,
        )],
        &["p.lhtml", "p.lhtml", "p.lhtml"],
    );
    assert_eq!(bodies, ["function", "function", "function"]);
}

#[test]
fn loading_refuses_bad_setups() {
    let serve = tempfile::tempdir().unwrap();
    let serve_dir = serve.path().canonicalize().unwrap();
    let data_dir = std::env::temp_dir();
    let load = |name: &str, path: PathBuf| {
        let mut settings = std::collections::BTreeMap::new();
        settings.insert(
            name.to_string(),
            PluginSetting {
                path,
                options: None,
            },
        );
        match red_crescent::plugins::load(&settings, &serve_dir, &data_dir) {
            Ok(_) => String::from("loaded"),
            Err(e) => e,
        }
    };

    let missing = load("probe", serve_dir.join("nowhere/nothing"));
    assert!(missing.contains("no library at"), "{missing}");

    let clash = load("json", probe_path());
    assert!(clash.contains("clashes with a built-in"), "{clash}");

    let bad_name = load("not-a-name", probe_path());
    assert!(bad_name.contains("Lua identifier"), "{bad_name}");

    let file = std::fs::read_dir(probe_path().parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("rc_probe.") && !n.ends_with(".d"))
        })
        .unwrap();
    let inside = serve_dir.join(file.file_name().unwrap());
    std::fs::copy(&file, &inside).unwrap();
    let served = load("probe", inside);
    assert!(served.contains("inside the serve directory"), "{served}");
}
