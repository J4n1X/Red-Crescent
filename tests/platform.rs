//! Tests for the platform capabilities added for real applications:
//! sqlite, crypto, multipart uploads, send_file, binary output, the server
//! table, and background Lua threads.

mod common;

use std::time::Duration;

use actix_web::dev::{Service, ServiceResponse};
use actix_web::http::StatusCode;
use actix_web::http::header;
use actix_web::test;

use common::{app_with, body_string, find_between, multipart_body, test_config};
use red_crescent::setup_data_dir;

#[actix_web::test]
async fn sqlite_round_trips_types_and_params() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/sqlite_page.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("rowid=1;changes=1;"), "body: {body}");
    assert!(
        body.contains("count=1;title=hello & goodbye;weight=2.5;bloblen=9;"),
        "body: {body}"
    );
    assert!(body.contains("empty=0;"), "body: {body}");
    assert!(body.contains("null_ok=true"), "body: {body}");
}

#[actix_web::test]
async fn crypto_primitives_work() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/crypto_page.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("argon2=true;"), "body: {body}");
    assert!(body.contains("verify_ok=true;"), "body: {body}");
    assert!(body.contains("verify_bad=false;"), "body: {body}");
    assert!(body.contains("verify_garbage=false;"), "body: {body}");
    assert!(
        body.contains("tok64=64;tok16=16;distinct=true;"),
        "body: {body}"
    );
    assert!(
        body.contains("sha=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad;"),
        "body: {body}"
    );
    assert!(body.contains("cte=true,false,false"), "body: {body}");
}

#[actix_web::test]
async fn send_file_serves_exact_bytes_with_disposition() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/sendfile_page.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let disposition = resp
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .expect("Content-Disposition")
        .to_str()
        .unwrap()
        .to_string();
    assert!(disposition.contains("attachment"), "got: {disposition}");
    assert!(disposition.contains("report.bin"), "got: {disposition}");
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .expect("cookie survives send_file")
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.contains("dl=yes"), "got: {set_cookie}");

    let bytes = test::read_body(resp).await;
    assert_eq!(&bytes[..], b"BIN\xFF\xFE\x00DATA");
}

#[actix_web::test]
async fn send_file_is_confined_to_data_and_serve_dirs() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/sendfile_escape.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("blocked=true"));
}

#[actix_web::test]
async fn send_file_refuses_server_side_source() {
    // The static file server never serves .lua or dotfiles; send_file must
    // hold the same line, or an app passing a user-controlled path would
    // hand out its own modules and rc_config.lua.
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let serve_dir = std::path::Path::new("tests/fixtures")
        .canonicalize()
        .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/sendfile_source.lhtml?serve_dir={}",
                serve_dir.display()
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("lua_source=true;"), "body: {body}");
    assert!(body.contains("dotfile=true"), "body: {body}");

    // ...while an ordinary file in the serve directory is still sendable,
    // so the rule did not over-block.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/sendfile_allowed.lhtml?serve_dir={}",
                serve_dir.display()
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("color: red"));
}

#[actix_web::test]
async fn binary_output_is_not_mangled() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/binary_page.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = test::read_body(resp).await;
    // The fixture file ends with a newline after the closing tag.
    assert_eq!(&bytes[..], b"\x00\x01\xFF\xFEraw\n");
}

#[actix_web::test]
async fn server_table_exposes_data_dir() {
    let (app, data_dir) = app_with(test_config("tests/fixtures")).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/server_page.lhtml")
            .to_request(),
    )
    .await;
    let body = body_string(resp).await;
    assert!(
        body.contains(&format!("data_dir={}", data_dir.display())),
        "body: {body}"
    );
}

#[actix_web::test]
async fn multipart_upload_spools_files_and_collects_fields() {
    let (app, data_dir) = app_with(test_config("tests/fixtures")).await;
    let boundary = "XTESTBOUNDARY";
    let payload = multipart_body(
        boundary,
        &[
            ("note", None, b"hello upload"),
            ("file", Some("photo.bin"), b"\x89PNGfakebytes"),
        ],
    );

    // Without keep=1 the spool file must be gone after the request.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/upload_page.lhtml")
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            ))
            .set_payload(payload.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("nfiles=1;note=hello upload;"), "body: {body}");
    assert!(body.contains("f1=file,photo.bin,13;"), "body: {body}");
    let spool_entries = std::fs::read_dir(data_dir.join(".spool")).unwrap().count();
    assert_eq!(spool_entries, 0, "unclaimed upload must be swept");

    // With keep=1 the template renames the file into the data dir.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/upload_page.lhtml?keep=1")
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("kept=1;"));
    let kept = std::fs::read(data_dir.join("kept.bin")).expect("kept file exists");
    assert_eq!(&kept[..], b"\x89PNGfakebytes");
    let spool_entries = std::fs::read_dir(data_dir.join(".spool")).unwrap().count();
    assert_eq!(spool_entries, 0);
}

#[actix_web::test]
async fn upload_filenames_keep_relative_paths() {
    // Folder uploads (webkitdirectory) send relative paths as filenames;
    // they must reach Lua untouched.
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let boundary = "XTESTBOUNDARY";
    let payload = multipart_body(boundary, &[("file", Some("dir/sub/x.bin"), b"abc")]);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/upload_page.lhtml")
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;
    let body = body_string(resp).await;
    assert!(body.contains("f1=file,dir/sub/x.bin,3;"), "body: {body}");
}

#[actix_web::test]
async fn too_many_upload_files_are_rejected_and_swept() {
    // The test config caps a multipart request at 8 file parts.
    let (app, data_dir) = app_with(test_config("tests/fixtures")).await;
    let boundary = "XTESTBOUNDARY";
    let parts: Vec<(&str, Option<&str>, &[u8])> = (0..10)
        .map(|_| ("file", Some("f.bin"), b"x".as_slice()))
        .collect();
    let payload = multipart_body(boundary, &parts);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/upload_page.lhtml")
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let spool_entries = std::fs::read_dir(data_dir.join(".spool")).unwrap().count();
    assert_eq!(spool_entries, 0, "rejected upload must be swept");
}

#[actix_web::test]
async fn oversized_plain_body_reports_its_limit() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/body_echo.lhtml")
            .insert_header((header::CONTENT_TYPE, "text/plain"))
            .set_payload("x".repeat(10_000))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body_string(resp).await.contains("4096"));
}

#[actix_web::test]
async fn oversized_multipart_upload_is_rejected_and_swept() {
    let (app, data_dir) = app_with(test_config("tests/fixtures")).await;
    let boundary = "XTESTBOUNDARY";
    let big = vec![b'x'; 128 * 1024]; // over the 64 KiB test cap
    let payload = multipart_body(boundary, &[("file", Some("big.bin"), &big)]);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/upload_page.lhtml")
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // The page must name the limit: a silent rejection is indistinguishable
    // from a connection failure once a browser is involved.
    let body = body_string(resp).await;
    assert!(
        body.contains("65536"),
        "413 page must state the limit: {body}"
    );
    let spool_entries = std::fs::read_dir(data_dir.join(".spool")).unwrap().count();
    assert_eq!(spool_entries, 0, "rejected upload must be swept");
}

#[actix_web::test]
async fn data_dir_inside_serve_dir_is_refused() {
    let serve_dir = std::path::Path::new("tests/fixtures")
        .canonicalize()
        .unwrap();
    let inside = serve_dir.join("data");
    let result = setup_data_dir(&inside, &serve_dir);
    assert!(result.is_err());
    std::fs::remove_dir_all(&inside).ok();
}

/// Poll `check` every 50ms until it returns true or ~5s pass.
fn wait_until(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn count_rows(db: &std::path::Path, sql: &str) -> i64 {
    let Ok(conn) = rusqlite::Connection::open(db) else {
        return -1;
    };
    conn.query_row(sql, [], |r| r.get(0)).unwrap_or(-1)
}

#[actix_web::test]
async fn thread_spawn_is_idempotent_and_names_free_on_exit() {
    let (app, data_dir) = app_with(test_config("tests/fixtures")).await;
    let uri = "/thread_page.lhtml?name=worker&script=jobs/thread_worker.lua";

    // First spawn starts the thread; a second call while it runs is a no-op.
    let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
    assert!(body_string(resp).await.contains("true|true"));
    let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
    assert!(body_string(resp).await.contains("false|true"));

    // The worker writes three ticks with sleeps in between, then exits.
    let db = data_dir.join("threadtest/worker.db");
    assert!(
        wait_until(|| count_rows(&db, "SELECT COUNT(*) FROM ticks") == 3),
        "worker never finished its ticks"
    );

    // Once it has exited, the name is free and spawn works again. Poll by
    // re-spawning: the first "true" response is the proof of release.
    let mut respawned = false;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        if body_string(resp).await.contains("true|") {
            respawned = true;
            break;
        }
    }
    assert!(respawned, "worker thread never released its name");
}

#[actix_web::test]
async fn sleep_resets_the_execution_deadline() {
    // The survivor's total awake time (~750ms) exceeds the 400ms deadline,
    // but each awake stretch is ~250ms — sleep() resets the clock. Threads
    // have no deadline by default, so this test configures one.
    let mut config = test_config("tests/fixtures");
    config.thread_timeout_ms = Some(400);
    let (app, data_dir) = app_with(config).await;
    let uri = "/thread_page.lhtml?name=survivor&script=jobs/thread_survivor.lua";
    let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
    assert!(body_string(resp).await.contains("true|"));

    let db = data_dir.join("threadtest/survivor.db");
    assert!(
        wait_until(|| count_rows(&db, "SELECT COUNT(*) FROM marks") == 1),
        "survivor was killed before finishing — deadline reset on sleep() is broken"
    );
}

#[actix_web::test]
async fn thread_that_never_sleeps_is_killed() {
    let mut config = test_config("tests/fixtures");
    config.thread_timeout_ms = Some(400);
    let (app, _) = app_with(config).await;
    let spawn_uri = "/thread_page.lhtml?name=hog&script=jobs/thread_hog.lua";
    let resp = test::call_service(&app, test::TestRequest::get().uri(spawn_uri).to_request()).await;
    assert!(body_string(resp).await.contains("true|true"));

    // The 400ms awake limit kills it; killing frees the name, so a re-spawn
    // succeeding is the observable proof of death.
    let mut respawned = false;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        let resp =
            test::call_service(&app, test::TestRequest::get().uri(spawn_uri).to_request()).await;
        if body_string(resp).await.contains("true|") {
            respawned = true;
            break;
        }
    }
    assert!(respawned, "hog thread was never killed by the timeout");
}

#[actix_web::test]
async fn spawning_a_missing_or_non_lua_script_fails() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    for uri in [
        "/thread_page.lhtml?name=x&script=missing.lua",
        "/thread_page.lhtml?name=x&script=hello.lhtml",
    ] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "uri: {uri}"
        );
    }
}

// --- native C modules -------------------------------------------------

#[actix_web::test]
async fn c_modules_are_disabled_by_default() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/cmodules.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("cpath=[]"), "cpath should be empty: {body}");
    assert!(body.contains("loadlib=blocked"), "body: {body}");
}

#[actix_web::test]
async fn c_module_dirs_enable_native_loading() {
    let dir = common::unique_data_dir().join("cmodules");
    std::fs::create_dir_all(&dir).unwrap();

    let mut config = test_config("tests/fixtures");
    config.c_module_dirs = vec![dir.clone()];
    let (app, _) = app_with(config).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/cmodules.lhtml").to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;

    let expected = format!("{}/?.so", dir.canonicalize().unwrap().display());
    assert!(
        body.contains(&expected),
        "cpath should list {expected}: {body}"
    );
    // The state is in mlua's unsafe mode, so the real loadlib is present.
    assert!(body.contains("loadlib=callable"), "body: {body}");

    std::fs::remove_dir_all(&dir).ok();
}

#[actix_web::test]
async fn c_module_dir_inside_serve_dir_is_refused() {
    let serve_dir = std::path::PathBuf::from("tests/fixtures")
        .canonicalize()
        .unwrap();
    let inside = serve_dir.join("assets");
    let err = red_crescent::resolve_c_module_dirs(&[inside], &serve_dir).unwrap_err();
    assert!(
        err.contains("must not be inside the serve directory"),
        "error: {err}"
    );
}

#[actix_web::test]
async fn missing_c_module_dir_is_a_startup_error() {
    let serve_dir = std::path::PathBuf::from("tests/fixtures")
        .canonicalize()
        .unwrap();
    let err = red_crescent::resolve_c_module_dirs(
        &[std::path::PathBuf::from("/nonexistent/lua/modules")],
        &serve_dir,
    )
    .unwrap_err();
    assert!(err.contains("is not usable"), "error: {err}");
}

#[actix_web::test]
async fn no_c_module_dirs_means_disabled() {
    let serve_dir = std::path::PathBuf::from("tests/fixtures")
        .canonicalize()
        .unwrap();
    assert!(
        red_crescent::resolve_c_module_dirs(&[], &serve_dir)
            .unwrap()
            .is_none()
    );
}

/// Loading a real, system-built `.so` only works because the binary exports
/// its statically linked Lua symbols (`-rdynamic`, set in `build.rs` for the
/// `c-modules` feature). Without that this fails with "undefined symbol:
/// lua_gettop", so the test guards a linker flag nothing else would catch.
///
/// Needs the feature to be meaningful: `cargo test --features c-modules`.
#[cfg(feature = "c-modules")]
#[actix_web::test]
async fn a_real_system_c_module_loads() {
    let module_dir = std::path::Path::new("/usr/lib/x86_64-linux-gnu/lua/5.4");
    if !module_dir.join("lpeg.so").exists() {
        eprintln!(
            "skipping: lpeg.so not installed at {}",
            module_dir.display()
        );
        return;
    }

    let mut config = test_config("tests/fixtures");
    config.c_module_dirs = vec![module_dir.to_path_buf()];
    let (app, _) = app_with(config).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/require_c.lhtml")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("loaded=LPeg"), "lpeg should load: {body}");
    assert!(body.contains("matched=5"), "lpeg should work: {body}");
}

// --- separate thread limits -------------------------------------------

#[actix_web::test]
async fn a_thread_without_a_deadline_finishes_long_work() {
    // No thread deadline is the default. The script works for ~600ms in one
    // unbroken stretch, well past the 400ms per-request budget, and must live
    // to write its mark — that is the whole point of splitting the two.
    let (app, data_dir) = app_with(test_config("tests/fixtures")).await;
    let uri = "/thread_page.lhtml?name=longrun&script=jobs/thread_longrun.lua";
    let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
    assert!(body_string(resp).await.contains("true|"));

    let db = data_dir.join("threadtest/longrun.db");
    assert!(
        wait_until(|| count_rows(&db, "SELECT COUNT(*) FROM marks") == 1),
        "thread was killed even though no deadline is configured"
    );
}

// --- request.remote_addr ----------------------------------------------

#[actix_web::test]
async fn remote_addr_reports_the_socket_peer() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let peer: std::net::SocketAddr = "203.0.113.7:51234".parse().unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/remote_addr.lhtml")
            .peer_addr(peer)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    // IP only, no port: this is what an app keys a rate limit on.
    assert!(body.contains("addr=[203.0.113.7]"), "body: {body}");
}

#[actix_web::test]
async fn remote_addr_is_nil_when_there_is_no_peer() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/remote_addr.lhtml")
            .to_request(),
    )
    .await;
    let body = body_string(resp).await;
    // nil, not a placeholder string, so `or` picks up an app's fallback.
    assert!(body.contains("addr=[nil]"), "body: {body}");
}

// --- thread arguments, results and join --------------------------------

/// Fetch a page and return its body, for the thread tests below.
async fn thread_body(
    app: &impl Service<actix_http::Request, Response = ServiceResponse, Error = actix_web::Error>,
    uri: &str,
) -> String {
    let resp = test::call_service(app, test::TestRequest::get().uri(uri).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    body_string(resp).await
}

#[actix_web::test]
async fn thread_args_and_result_round_trip() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let body = thread_body(
        &app,
        "/thread_join.lhtml?name=echo&script=jobs/thread_echo.lua&label=hello&n=21",
    )
    .await;
    assert!(
        body.contains("spawned=true status=finished"),
        "body: {body}"
    );
    // Both directions crossed the state boundary as JSON.
    assert!(body.contains("got=hello"), "args did not arrive: {body}");
    assert!(body.contains("doubled=42"), "result did not return: {body}");
}

#[actix_web::test]
async fn a_failed_thread_reports_its_error() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let body = thread_body(
        &app,
        "/thread_join.lhtml?name=boom&script=jobs/thread_boom.lua",
    )
    .await;
    assert!(body.contains("status=failed"), "body: {body}");
    assert!(body.contains("deliberate failure"), "body: {body}");
}

#[actix_web::test]
async fn a_thread_cannot_join_itself() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let body = thread_body(
        &app,
        "/thread_join.lhtml?name=selfjoin&script=jobs/thread_selfjoin.lua",
    )
    .await;
    // The pcall inside the thread caught the refusal rather than deadlocking.
    assert!(body.contains("ok=false"), "self-join was allowed: {body}");
    assert!(body.contains("cannot join itself"), "body: {body}");
}

#[actix_web::test]
async fn racing_spawns_share_one_run() {
    // Only one claim can win, and the loser is handed the winner's id so both
    // callers can watch the same run.
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let body = thread_body(&app, "/thread_twice.lhtml").await;
    assert!(
        body.contains("first=true second=false same_id=true"),
        "body: {body}"
    );
}

#[actix_web::test]
async fn run_ids_survive_name_reuse() {
    // The hazard that makes ids necessary: the same name, used twice in a row,
    // must not let the first run's status be confused with the second's.
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let body = thread_body(&app, "/thread_gen.lhtml").await;
    assert!(body.contains("distinct=true"), "ids were reused: {body}");
    assert!(body.contains("first=finished/one"), "body: {body}");
    assert!(body.contains("second=finished/two"), "body: {body}");
    // The older run still answers for itself after the name moved on.
    assert!(body.contains("first_still=finished"), "body: {body}");
}

// --- killing a thread ---------------------------------------------------

async fn spawned_id(
    app: &impl Service<actix_http::Request, Response = ServiceResponse, Error = actix_web::Error>,
    name: &str,
    script: &str,
) -> String {
    let body = thread_body(
        app,
        &format!("/thread_kill.lhtml?action=spawn&name={name}&script={script}"),
    )
    .await;
    find_between(&body, "id=", "\n")
        .unwrap_or(body.trim().trim_start_matches("id="))
        .trim()
        .to_string()
}

#[actix_web::test]
async fn a_spinning_thread_can_be_killed() {
    // No deadline is configured, so nothing but the kill can stop this loop.
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let id = spawned_id(&app, "spin", "jobs/thread_hog.lua").await;

    let killed = thread_body(&app, &format!("/thread_kill.lhtml?action=kill&id={id}")).await;
    assert!(killed.contains("killed=true"), "body: {killed}");

    let joined = thread_body(
        &app,
        &format!("/thread_kill.lhtml?action=join&id={id}&wait=5"),
    )
    .await;
    // Cancelled, not failed: a deliberate stop is not a fault.
    assert!(joined.contains("status=cancelled"), "body: {joined}");

    // The name went with it, so the slot is reusable.
    let running = thread_body(&app, "/thread_kill.lhtml?action=running&name=spin").await;
    assert!(running.contains("running=false"), "body: {running}");
}

#[actix_web::test]
async fn killing_a_sleeping_thread_does_not_wait_out_the_nap() {
    // The script sleeps 30s. If sleep were one long nap the join would time
    // out and report "running"; slicing it is what makes the kill land.
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let id = spawned_id(&app, "napper", "jobs/thread_napper.lua").await;

    let killed = thread_body(&app, &format!("/thread_kill.lhtml?action=kill&id={id}")).await;
    assert!(killed.contains("killed=true"), "body: {killed}");

    let joined = thread_body(
        &app,
        &format!("/thread_kill.lhtml?action=join&id={id}&wait=3"),
    )
    .await;
    assert!(
        joined.contains("status=cancelled"),
        "kill did not interrupt the sleep: {joined}"
    );
}

#[actix_web::test]
async fn killing_a_finished_run_reports_false() {
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    let body = thread_body(
        &app,
        "/thread_join.lhtml?name=done&script=jobs/thread_echo.lua&label=x&n=1",
    )
    .await;
    assert!(body.contains("status=finished"), "body: {body}");

    // Nothing to stop, and an id nobody owns is the same answer.
    let killed = thread_body(&app, "/thread_kill.lhtml?action=kill&id=999999").await;
    assert!(killed.contains("killed=false"), "body: {killed}");
}

#[actix_web::test]
async fn the_thread_cap_counts_only_live_runs() {
    // 64 is a ceiling on threads running at once, not on threads ever started.
    // A disposable-task design — one thread per unit of work, then gone —
    // spawns far more than that over a lifetime, so an ended run has to give
    // its slot back immediately.
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    for i in 0..100 {
        let body = thread_body(
            &app,
            &format!("/thread_join.lhtml?name=burst{i}&script=jobs/thread_echo.lua&label=x&n={i}"),
        )
        .await;
        assert!(
            body.contains("status=finished"),
            "run {i} did not finish — thread limit reached? {body}"
        );
    }
}

// --- sqlite connection reuse -------------------------------------------

#[actix_web::test]
async fn an_aborted_transaction_does_not_poison_a_reused_connection() {
    // Connections are parked for the next request, so one that ended inside a
    // transaction would otherwise hand over its uncommitted writes and an open
    // write lock. Repeated because which pool thread serves a request -- and
    // therefore which parked connection is reused -- is not deterministic.
    let (app, _) = app_with(test_config("tests/fixtures")).await;
    for _ in 0..15 {
        let body = thread_body(&app, "/txn_leak.lhtml?mode=leak").await;
        assert!(body.contains("left-open"), "body: {body}");
    }
    for _ in 0..15 {
        let body = thread_body(&app, "/txn_leak.lhtml?mode=read").await;
        assert!(
            body.contains("count=0"),
            "uncommitted row survived into a later request: {body}"
        );
    }
}

#[actix_web::test]
async fn sqlite_reuse_can_be_turned_off() {
    let mut config = test_config("tests/fixtures");
    config.sqlite_idle_connections = 0;
    let (app, _) = app_with(config).await;
    for _ in 0..5 {
        let body = thread_body(&app, "/txn_leak.lhtml?mode=leak").await;
        assert!(body.contains("left-open"), "body: {body}");
    }
    let body = thread_body(&app, "/txn_leak.lhtml?mode=read").await;
    assert!(body.contains("count=0"), "body: {body}");
}
