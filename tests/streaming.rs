//! Streamed responses: `flush()` through the real handler.

mod common;

use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, Instant};

use actix_web::body::{BoxBody, MessageBody};
use actix_web::http::StatusCode;
use actix_web::test;

use common::{app_with, body_string, test_config};

fn serve_dir_with(pages: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in pages {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    dir
}

fn lua_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

async fn next_chunk(body: &mut BoxBody) -> Option<Result<String, String>> {
    std::future::poll_fn(|cx| Pin::new(&mut *body).poll_next(cx))
        .await
        .map(|chunk| {
            chunk
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .map_err(|e| e.to_string())
        })
}

/// The first chunk arrives while the template is still waiting on the test;
/// without streaming this would hang until the render timed out.
#[actix_web::test]
async fn flushed_output_arrives_before_the_render_ends() {
    let signal = tempfile::tempdir().unwrap();
    let signal_file = signal.path().join("go");
    let page = format!(
        r#"<?lua
response.status = 201
response.headers["X-Test"] = "set before flush"
response.set_cookie{{ name = "c", value = "v" }}
print("first")
flush()
while not io.open("{}") do end
print("second")
?>"#,
        lua_path(&signal_file)
    );
    let dir = serve_dir_with(&[("p.lhtml", &page)]);
    let mut config = test_config(dir.path().to_str().unwrap());
    config.timeout_ms = 5000;
    let (app, _) = app_with(config).await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/p.lhtml").to_request()).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let headers = resp.headers();
    assert_eq!(headers.get("x-test").unwrap(), "set before flush");
    assert_eq!(headers.get("x-accel-buffering").unwrap(), "no");
    assert!(
        headers
            .get("set-cookie")
            .is_some_and(|c| c.to_str().unwrap().starts_with("c=v")),
        "cookie missing: {headers:?}"
    );

    let mut body = resp.into_body();
    assert_eq!(next_chunk(&mut body).await, Some(Ok("first".to_string())));
    std::fs::write(&signal_file, "").unwrap();
    assert_eq!(next_chunk(&mut body).await, Some(Ok("second".to_string())));
    assert_eq!(next_chunk(&mut body).await, None);
}

#[actix_web::test]
async fn the_head_is_fixed_after_the_first_flush() {
    let page = r#"<?lua
response.status = 202
flush()
local function try(f)
    local ok, err = pcall(f)
    print(ok and "allowed" or tostring(err):match("[%w_.]+: output has already been flushed"), "\n")
end
try(function() response.status = 500 end)
try(function() response.headers["X-Late"] = "no" end)
try(function() response.set_cookie{ name = "late", value = "no" } end)
try(function() redirect("/elsewhere") end)
?>"#;
    let dir = serve_dir_with(&[("p.lhtml", page)]);
    let (app, _) = app_with(test_config(dir.path().to_str().unwrap())).await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/p.lhtml").to_request()).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert!(resp.headers().get("x-late").is_none());
    assert!(resp.headers().get("location").is_none());
    let body = body_string(resp).await;
    assert_eq!(
        body.lines().map(str::trim).collect::<Vec<_>>(),
        vec![
            "response: output has already been flushed",
            "response.headers: output has already been flushed",
            "set_cookie: output has already been flushed",
            "redirect: output has already been flushed",
        ],
        "body: {body}"
    );
}

/// The client already holds a 200, so the only honest signal left is a body
/// that does not end cleanly.
#[actix_web::test]
async fn an_error_after_a_flush_truncates_the_body() {
    let page = r#"<?lua print("partial") flush() error("boom") ?>"#;
    let dir = serve_dir_with(&[("p.lhtml", page)]);
    let (app, _) = app_with(test_config(dir.path().to_str().unwrap())).await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/p.lhtml").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    assert_eq!(next_chunk(&mut body).await, Some(Ok("partial".to_string())));
    assert!(
        matches!(next_chunk(&mut body).await, Some(Err(_))),
        "the body ended as if complete"
    );
}

#[actix_web::test]
async fn exit_after_a_flush_ends_the_body_cleanly() {
    let page = r#"<?lua print("a") flush() print("b") exit() print("never") ?>"#;
    let dir = serve_dir_with(&[("p.lhtml", page)]);
    let (app, _) = app_with(test_config(dir.path().to_str().unwrap())).await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/p.lhtml").to_request()).await;
    assert_eq!(body_string(resp).await, "ab");
}

/// Once nobody is reading, the next flush stops the template as exit() would.
#[actix_web::test]
async fn a_client_that_leaves_stops_the_template() {
    let marker = tempfile::tempdir().unwrap();
    let marker_file = marker.path().join("stopped");
    let page = format!(
        r#"<?lua
local ok = pcall(function()
    while true do print(("x"):rep(1000)) flush() end
end)
local f = io.open("{}", "w")
f:write(tostring(ok))
f:close()
?>"#,
        lua_path(&marker_file)
    );
    let dir = serve_dir_with(&[("p.lhtml", &page)]);
    let mut config = test_config(dir.path().to_str().unwrap());
    config.timeout_ms = 5000;
    let (app, _) = app_with(config).await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/p.lhtml").to_request()).await;
    let mut body = resp.into_body();
    assert!(matches!(next_chunk(&mut body).await, Some(Ok(_))));
    drop(body);

    let started = Instant::now();
    while !marker_file.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the template kept running"
        );
        actix_web::rt::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(std::fs::read_to_string(&marker_file).unwrap(), "false");
}

/// send_file replaces the output, so there is nothing to flush.
#[actix_web::test]
async fn flush_after_send_file_raises() {
    let page = r#"<?lua
response.send_file(server.data_dir .. "/f.txt")
local ok, err = pcall(flush)
local expected = "send_file() has already replaced the output"
response.headers["X-Error"] = tostring(err):find(expected, 1, true) and "raised" or tostring(ok)
?>"#;
    let dir = serve_dir_with(&[("p.lhtml", page)]);
    let (app, data_dir) = app_with(test_config(dir.path().to_str().unwrap())).await;
    std::fs::write(data_dir.join("f.txt"), "file").unwrap();

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/p.lhtml").to_request()).await;
    assert_eq!(resp.headers().get("x-error").unwrap(), "raised");
    assert_eq!(body_string(resp).await, "file");
}
