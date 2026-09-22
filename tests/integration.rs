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

/// The second request to a template runs cached bytecode rather than freshly
/// parsed source. Both paths must produce the same bytes, for the page itself
/// and for anything it includes.
#[actix_web::test]
async fn cached_templates_render_identically_on_every_request() {
    let app = default_app().await;
    for uri in ["/hello.lhtml?name=Janick", "/include_page.lhtml"] {
        let mut seen: Option<String> = None;
        for attempt in 1..=3 {
            let resp =
                test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
            assert_eq!(resp.status(), StatusCode::OK, "{uri} attempt {attempt}");
            let body = body_string(resp).await;
            match &seen {
                None => seen = Some(body),
                Some(first) => assert_eq!(first, &body, "{uri} changed on attempt {attempt}"),
            }
        }
    }
}

/// A template that raises must keep failing the same way once it is cached —
/// the dump must not swallow the error or lose the 500.
#[actix_web::test]
async fn a_failing_template_stays_failing_when_cached() {
    let app = default_app().await;
    for attempt in 1..=2 {
        let resp = test::call_service(
            &app,
            test::TestRequest::get().uri("/err.lhtml").to_request(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "attempt {attempt}"
        );
    }
}

/// `require` resolves `?/init.lua` when `?.lua` does not exist, the same order
/// `package.path` declares.
#[actix_web::test]
async fn require_falls_back_to_init_lua() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/require_init.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("pkg-init"));
}

/// A missing module still names every path it looked at, so the error stays as
/// useful as the stock searcher's.
#[actix_web::test]
async fn require_reports_the_paths_it_tried() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/require_missing.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("not found"), "{body}");
    assert!(body.contains("nope/missing.lua"), "{body}");
    assert!(body.contains("nope/missing/init.lua"), "{body}");
}

/// Modules are cached process-wide, so the second request loads bytecode rather
/// than re-reading the file. It must behave exactly like the first.
#[actix_web::test]
async fn cached_modules_load_identically_on_every_request() {
    let app = default_app().await;
    let mut seen: Option<String> = None;
    for attempt in 1..=3 {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/require_page.lhtml")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "attempt {attempt}");
        let body = body_string(resp).await;
        assert!(body.contains("Hello, Bob!"), "attempt {attempt}: {body}");
        match &seen {
            None => seen = Some(body),
            Some(first) => assert_eq!(first, &body, "changed on attempt {attempt}"),
        }
    }
}

/// A symlink inside the serve directory pointing out of it must not be loadable
/// as a module. The stock searcher would follow it; this one canonicalizes and
/// checks the result, the same rule `include()` applies.
#[actix_web::test]
async fn require_refuses_a_module_symlinked_out_of_the_serve_dir() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        outside.path().join("escape.lua"),
        "return { got = 'outside' }",
    )
    .unwrap();

    let serve = tempfile::tempdir().unwrap();
    std::fs::write(
        serve.path().join("index.lhtml"),
        "<?lua local ok = pcall(require, 'escape') ?><?lua= ok and 'LOADED' or 'refused' ?>",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("escape.lua"),
        serve.path().join("escape.lua"),
    )
    .unwrap();

    let app = app_with(common::test_config(serve.path().to_str().unwrap()))
        .await
        .0;
    let resp = test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(
        body.contains("refused"),
        "symlink escaped the serve dir: {body}"
    );
}

/// `<?lua= ?>` escapes by default and `<?lua== ?>` does not. The explicit
/// `html_escape()` still works for values assembled inside Lua.
#[actix_web::test]
async fn inline_expressions_escape_unless_the_raw_form_is_used() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/escaping.lhtml?v=%3Cb%3E%26%27%22")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(
        body.contains("esc:[&lt;b&gt;&amp;&#x27;&quot;]"),
        "default form must escape: {body}"
    );
    assert!(
        body.contains(r#"raw:[<b>&'"]"#),
        "raw form must not escape: {body}"
    );
    assert!(body.contains("num:[42]"), "numbers pass through: {body}");
    assert!(body.contains("nil:[]"), "nil prints nothing: {body}");
    // Escaping in Lua then emitting raw is the pattern for markup built by
    // hand, and must escape exactly once.
    assert!(
        body.contains("lua:[&lt;b&gt;&amp;&#x27;&quot;]"),
        "html_escape + raw form must escape once: {body}"
    );
    // And the migration hazard, pinned deliberately: the old
    // `<?lua= html_escape(x) ?>` shape now escapes twice.
    assert!(
        body.contains("dbl:[&amp;lt;b&amp;gt;"),
        "html_escape under the default form double-escapes: {body}"
    );
}

/// A value needing no escaping must come back byte-identical, which is the
/// short-circuit path that skips an allocation.
#[actix_web::test]
async fn values_needing_no_escaping_pass_through_untouched() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/escaping.lhtml?v=report.pdf")
            .to_request(),
    )
    .await;
    let body = body_string(resp).await;
    for expected in ["esc:[report.pdf]", "raw:[report.pdf]", "lua:[report.pdf]"] {
        assert!(body.contains(expected), "missing {expected}: {body}");
    }
}

/// Output larger than the buffer threshold must come back whole and in order.
/// On a JIT backend `_out` accumulates in Lua and drains periodically, so this
/// is where a lost or misordered flush would show up; on a direct backend it
/// simply passes through.
#[actix_web::test]
async fn large_interleaved_output_survives_buffering() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/big_output.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;

    let expected: String = (1..=400).map(|i| format!("L{i}|RP{i};\n")).collect();
    assert!(
        body.contains(expected.trim_end()),
        "interleaved output not intact; got {} bytes starting {:?}",
        body.len(),
        &body[..body.len().min(80)]
    );
    assert!(
        body.trim_end().ends_with("END"),
        "tail lost: {:?}",
        &body[body.len().saturating_sub(40)..]
    );
}

/// Pins how every value type reaches the page through `<?lua= ?>`, which a raw
/// C function renders: strings escaped, numbers formatted by Rust so the two
/// backends agree, nil dropped, tables as JSON.
#[actix_web::test]
async fn out_expr_renders_every_value_type() {
    let app = default_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/out_expr_types.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // LuaJIT has no integer subtype, so past 2^53 it only has the double.
    let bigint = if cfg!(feature = "luajit") {
        "9007199254740992"
    } else {
        "9007199254740993"
    };
    assert_eq!(
        body_string(resp).await,
        format!(
            "nil:[]\ntrue:[true]\nfalse:[false]\nint:[42]\nfloat:[2.5]\nwhole:[5]\n\
             neg:[-0.125]\nstr:[a&lt;b&gt;&amp;&#x27;&quot;c]\nmulti:[x1true]\n\
             table:[[1,2]]\nfunc:[&lt;function&gt;]\nraw:[<b>]\nbigint:[{bigint}]\n"
        )
    );
}
