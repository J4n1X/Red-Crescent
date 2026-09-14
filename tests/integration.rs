mod common;

use std::time::{Duration, Instant};

use actix_web::dev::{Service, ServiceResponse};
use actix_web::http::StatusCode;
use actix_web::http::header;
use actix_web::test;

use common::{app_with, body_string};
use red_crescent::config::Config;

fn test_config() -> Config {
    common::test_config("tests/fixtures")
}

async fn default_app()
-> impl Service<actix_http::Request, Response = ServiceResponse, Error = actix_web::Error> {
    app_with(test_config()).await.0
}

#[actix_web::test]
async fn renders_a_simple_template() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/hello.lhtml?name=Janick")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp.headers().get(header::CONTENT_TYPE).unwrap();
    assert!(content_type.to_str().unwrap().starts_with("text/html"));
    assert!(body_string(resp).await.contains("Hello Janick"));
}

#[actix_web::test]
async fn control_flow_spans_blocks() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/controlflow.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("<i>1</i><i>2</i><i>3</i>"));
}

#[actix_web::test]
async fn close_marker_inside_lua_string_does_not_end_block() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/string_close.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("?>|done"));
}

#[actix_web::test]
async fn form_post_populates_body_table() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/body_echo.lhtml")
            .insert_header((header::CONTENT_TYPE, "application/x-www-form-urlencoded"))
            .set_payload("msg=hi+there")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("POST:hi there:msg=hi+there"), "body: {body}");
}

#[actix_web::test]
async fn put_requests_are_served() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/body_echo.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("PUT:none"));
}

#[actix_web::test]
async fn json_post_round_trips() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/json_echo.lhtml")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"user":{"name":"Ada","age":36}}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp.headers().get(header::CONTENT_TYPE).unwrap().clone();
    assert_eq!(content_type.to_str().unwrap(), "application/json");
    let body = body_string(resp).await;
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON body");
    assert_eq!(parsed["user"]["name"], "Ada");
    assert_eq!(parsed["ok"], true);
}

#[actix_web::test]
async fn repeated_query_params_are_exposed() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/multi_query.lhtml?a=1&a=2")
            .to_request(),
    )
    .await;
    let body = body_string(resp).await;
    assert!(body.contains("2|2|1"), "body: {body}");
}

#[actix_web::test]
async fn header_lookup_is_case_insensitive() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/headers.lhtml")
            .insert_header(("x-custom-header", "custom-value"))
            .to_request(),
    )
    .await;
    assert!(body_string(resp).await.contains("custom-value"));
}

#[actix_web::test]
async fn cookies_round_trip() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/cookies.lhtml")
            .insert_header((header::COOKIE, "session=tok123"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .expect("Set-Cookie header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.contains("sid=abc123"), "got: {set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "got: {set_cookie}");
    assert!(body_string(resp).await.contains("session=tok123"));
}

#[actix_web::test]
async fn redirect_helper_sets_location_and_status() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/redirect.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap(),
        "/hello.lhtml"
    );
    assert!(!body_string(resp).await.contains("never rendered"));
}

#[actix_web::test]
async fn exit_stops_rendering() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/exit.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("before"));
    assert!(!body.contains("after"));
}

#[actix_web::test]
async fn include_shares_globals_and_output() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/include_page.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("<header>H</header>"), "body: {body}");
    assert!(body.contains("body:Fixture Site"), "body: {body}");
}

#[actix_web::test]
async fn recursive_include_hits_depth_limit() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/include_loop.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[actix_web::test]
async fn require_loads_modules_from_serve_dir() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/require_page.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("Hello, Bob!"));
}

#[actix_web::test]
async fn infinite_loop_is_stopped_by_timeout() {
    let app = default_app().await;
    let started = Instant::now();
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/timeout.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "timeout took {:?}",
        started.elapsed()
    );
}

#[actix_web::test]
async fn pcall_cannot_swallow_the_timeout() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/pcall_timeout.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[actix_web::test]
async fn runaway_allocation_is_stopped_by_memory_limit() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/memory.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[actix_web::test]
async fn custom_status_codes_are_honored() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/status.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::IM_A_TEAPOT);
}

#[actix_web::test]
async fn root_serves_the_index_template() {
    let app = default_app().await;
    let resp = test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("INDEX PAGE"));
}

#[actix_web::test]
async fn static_files_are_served_with_content_type() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/assets/style.css")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp.headers().get(header::CONTENT_TYPE).unwrap();
    assert!(content_type.to_str().unwrap().starts_with("text/css"));
    assert!(body_string(resp).await.contains("color: red"));
}

#[actix_web::test]
async fn static_serving_can_be_disabled() {
    let (app, _) = app_with(Config {
        static_files: false,
        ..test_config()
    })
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/assets/style.css")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn lua_source_files_are_never_served() {
    let app = default_app().await;
    for uri in ["/secret.lua", "/libs/helper.lua"] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "uri: {uri}");
    }
}

#[actix_web::test]
async fn path_traversal_is_blocked() {
    let app = default_app().await;
    for uri in [
        "/../Cargo.toml",
        "/%2e%2e/Cargo.toml",
        "/partials/%2e%2e/%2e%2e/Cargo.toml",
    ] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        assert!(
            resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
            "uri {uri} returned {}",
            resp.status()
        );
    }
}

#[actix_web::test]
async fn missing_files_return_404() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/nope.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn a_fallback_template_catches_unresolved_paths() {
    let (app, _) = app_with(Config {
        fallback: Some("front.lhtml".to_string()),
        ..test_config()
    })
    .await;

    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/api/ping").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert_eq!(
        body_string(resp).await.trim(),
        r#"{"method":"GET","ok":true}"#
    );

    // The front controller owns the status too, so it can still answer 404.
    let resp =
        test::call_service(&app, test::TestRequest::get().uri("/api/nope").to_request()).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(body_string(resp).await.contains("no route for /api/nope"));
}

#[actix_web::test]
async fn a_fallback_does_not_shadow_real_files_or_escapes() {
    let (app, _) = app_with(Config {
        fallback: Some("front.lhtml".to_string()),
        ..test_config()
    })
    .await;

    // A real template still wins.
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/hello.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("Hello"));

    // A real static file still wins.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/assets/style.css")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Traversal is refused before the fallback is considered.
    for uri in ["/../Cargo.toml", "/partials/%2e%2e/%2e%2e/Cargo.toml"] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "uri: {uri}");
    }
}

#[actix_web::test]
async fn a_fallback_receives_post_bodies() {
    let (app, _) = app_with(Config {
        fallback: Some("front.lhtml".to_string()),
        ..test_config()
    })
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post().uri("/api/ping").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains(r#""method":"POST""#));
}

#[actix_web::test]
async fn oversized_bodies_are_rejected() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/body_echo.lhtml")
            .insert_header((header::CONTENT_TYPE, "application/x-www-form-urlencoded"))
            .set_payload("x".repeat(10_000))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[actix_web::test]
async fn prod_error_pages_hide_details_dev_pages_show_them() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/err.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_string(resp).await;
    assert!(!body.contains("boom"), "prod page leaked details: {body}");

    let (dev_app, _) = app_with(Config {
        dev: true,
        ..test_config()
    })
    .await;
    let resp = test::call_service(
        &dev_app,
        test::TestRequest::get().uri("/err.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_string(resp).await;
    assert!(body.contains("boom"), "dev page missing details: {body}");
    // Line numbers must map back to the .lhtml source (error is on line 3).
    assert!(
        body.contains("err.lhtml:3"),
        "dev page missing file:line: {body}"
    );
}

#[actix_web::test]
async fn template_edits_are_picked_up_when_cache_validates_mtime() {
    // The cache is keyed on mtime+len; touching the file with new content and
    // a different length must invalidate it.
    let dir = std::path::Path::new("tests/fixtures");
    let path = dir.join("cache_probe.lhtml");
    std::fs::write(&path, "v1").unwrap();
    let app = default_app().await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/cache_probe.lhtml")
            .to_request(),
    )
    .await;
    assert!(body_string(resp).await.contains("v1"));

    std::fs::write(&path, "version-two").unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/cache_probe.lhtml")
            .to_request(),
    )
    .await;
    let body = body_string(resp).await;
    std::fs::remove_file(&path).ok();
    assert!(body.contains("version-two"), "stale cache: {body}");
}
