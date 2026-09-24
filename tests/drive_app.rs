//! End-to-end test of the Drive app (apps/drive) running on the real
//! platform: registration, approval flow, sessions, CSRF, folders, multipart
//! upload, authenticated download, share links, and authorization boundaries.

mod common;

use actix_web::dev::{Service, ServiceResponse};
use actix_web::http::StatusCode;
use actix_web::http::header;
use actix_web::test;

use std::time::Duration;

use common::{app_with, body_string, find_between, multipart_body};
use red_crescent::config::Config;

const BOUNDARY: &str = "XDRIVEBOUNDARY";

fn drive_config() -> Config {
    Config {
        timeout_ms: 5000,
        memory_limit_mb: 64,
        // The archive worker runs the export; give it the same headroom the
        // request path gets rather than the 8 MiB test default.
        thread_memory_limit_mb: 64,
        max_upload_size: 1024 * 1024,
        // Matches apps/drive/rc_config.lua: /s/<token> is routed, not a file.
        fallback: Some("app.lhtml".to_string()),
        ..common::test_config("apps/drive")
    }
}

fn session_from(resp: &ServiceResponse) -> Option<String> {
    for value in resp.headers().get_all(header::SET_COOKIE) {
        let s = value.to_str().ok()?;
        if let Some(rest) = s.strip_prefix("session=") {
            let token = rest.split(';').next().unwrap_or("");
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

async fn post_form<S>(app: &S, uri: &str, session: Option<&str>, body: String) -> ServiceResponse
where
    S: Service<actix_http::Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let mut req = test::TestRequest::post()
        .uri(uri)
        .insert_header((header::CONTENT_TYPE, "application/x-www-form-urlencoded"));
    if let Some(s) = session {
        req = req.insert_header((header::COOKIE, format!("session={s}")));
    }
    test::call_service(app, req.set_payload(body).to_request()).await
}

async fn get<S>(app: &S, uri: &str, session: Option<&str>) -> ServiceResponse
where
    S: Service<actix_http::Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let mut req = test::TestRequest::get().uri(uri);
    if let Some(s) = session {
        req = req.insert_header((header::COOKIE, format!("session={s}")));
    }
    test::call_service(app, req.to_request()).await
}

/// Pull an integer field out of a small JSON body, without a parser.
fn json_number(body: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let start = body.find(&needle)? + needle.len();
    body[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Queue an archive for `folder` and wait for the worker to finish it.
async fn ready_archive_job<S>(app: &S, session: &str, folder: &str) -> u64
where
    S: Service<actix_http::Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let body = body_string(
        get(
            app,
            &format!("/archive.lhtml?folder={folder}&json=1"),
            Some(session),
        )
        .await,
    )
    .await;
    let job = json_number(&body, "job").unwrap_or_else(|| panic!("no job id in {body}"));

    for _ in 0..200 {
        let status = body_string(
            get(
                app,
                &format!("/archive.lhtml?job={job}&json=1"),
                Some(session),
            )
            .await,
        )
        .await;
        if status.contains("\"status\":\"ready\"") {
            return job;
        }
        assert!(
            !status.contains("\"status\":\"failed\""),
            "archive job failed: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("archive job {job} never became ready");
}

fn location(resp: &ServiceResponse) -> String {
    resp.headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

async fn csrf_of<S>(app: &S, session: &str) -> String
where
    S: Service<actix_http::Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let page = body_string(get(app, "/", Some(session)).await).await;
    find_between(&page, "name=\"csrf\" value=\"", "\"")
        .expect("csrf token on page")
        .to_string()
}

async fn login<S>(app: &S, username: &str, password: &str) -> Option<String>
where
    S: Service<actix_http::Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let resp = post_form(
        app,
        "/login.lhtml",
        None,
        format!("username={username}&password={password}"),
    )
    .await;
    session_from(&resp)
}

#[actix_web::test]
async fn full_drive_flow() {
    let (app, data_dir) = app_with(drive_config()).await;

    // --- Register the first user: becomes admin, can log in immediately.
    let resp = post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(
        location(&resp).contains("login"),
        "loc: {}",
        location(&resp)
    );

    // Wrong password fails, right one gives a session cookie.
    assert!(login(&app, "admin", "wrongpass99").await.is_none());
    let admin = login(&app, "admin", "adminpass1")
        .await
        .expect("admin session");

    // Without a session, / redirects to the login page.
    let resp = get(&app, "/", None).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(location(&resp).contains("/login.lhtml"));

    // With the session we see the (empty) file browser and a CSRF token.
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    assert!(page.contains("Nothing here yet"), "page: {page}");
    let csrf = csrf_of(&app, &admin).await;
    assert_eq!(csrf.len(), 32);

    // --- Create a folder.
    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=mkdir&folder=&name=Documents"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    let folder_id = find_between(&page, "/?folder=", "\"")
        .expect("folder link")
        .to_string();

    // --- Upload a file into it (multipart).
    let content = b"hello drive e2e \xF0\x9F\x8C\x99";
    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, folder_id.as_bytes()),
            ("file", Some("notes.txt"), content),
        ],
    );
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(
        location(&resp).contains("uploaded"),
        "loc: {}",
        location(&resp)
    );

    // Listing shows it; grab the file id.
    let page = body_string(get(&app, &format!("/?folder={folder_id}"), Some(&admin)).await).await;
    assert!(page.contains("notes.txt"), "page: {page}");
    let file_id = find_between(&page, "/download.lhtml?id=", "\"")
        .expect("download link")
        .to_string();

    // --- Authenticated download returns the exact bytes.
    let resp = get(&app, &format!("/download.lhtml?id={file_id}"), Some(&admin)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let disposition = resp
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(disposition.contains("notes.txt"), "got: {disposition}");
    let bytes = test::read_body(resp).await;
    assert_eq!(&bytes[..], content);

    // --- Create a share link.
    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=share_create&id={file_id}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let page =
        body_string(get(&app, &format!("/shares.lhtml?file={file_id}"), Some(&admin)).await).await;
    let token = find_between(&page, "class=\"sharelink\">", "<")
        .and_then(|link| link.rsplit("/s/").next().map(str::to_string))
        .expect("share token");
    assert_eq!(token.len(), 32, "token: {token}");

    // --- The share works WITHOUT any session.
    let page = body_string(get(&app, &format!("/s/{token}"), None).await).await;
    assert!(page.contains("notes.txt"), "share page: {page}");
    let resp = get(&app, &format!("/s/{token}?dl=1"), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = test::read_body(resp).await;
    assert_eq!(&bytes[..], content);

    // Download counter incremented (checked straight in the DB).
    let conn = rusqlite::Connection::open(data_dir.join("drive/drive.db")).unwrap();
    let downloads: i64 = conn
        .query_row(
            "SELECT downloads FROM shares WHERE token = ?1",
            [&token],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(downloads, 1);

    // A bogus token 404s; an expired share 404s.
    let resp = get(&app, "/s/0000000000000000000000000000dead", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    conn.execute(
        "UPDATE shares SET expires_at = 1 WHERE token = ?1",
        [&token],
    )
    .unwrap();
    let resp = get(&app, &format!("/s/{token}"), None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // --- Second user: pending until the admin approves.
    let resp = post_form(
        &app,
        "/register.lhtml",
        None,
        "username=bob&password=bobpass123&password2=bobpass123".to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(
        location(&resp).contains("approve"),
        "loc: {}",
        location(&resp)
    );
    assert!(
        login(&app, "bob", "bobpass123").await.is_none(),
        "bob must be pending"
    );

    let admin_page = body_string(get(&app, "/admin.lhtml", Some(&admin)).await).await;
    assert!(admin_page.contains("bob"), "admin page: {admin_page}");
    assert!(
        admin_page.contains("pending approval"),
        "admin page: {admin_page}"
    );
    let after_approve = &admin_page[admin_page.find("value=\"approve_user\"").unwrap()..];
    let bob_id = find_between(after_approve, "name=\"id\" value=\"", "\"")
        .expect("bob id on admin page")
        .to_string();
    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=approve_user&id={bob_id}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);

    let bob = login(&app, "bob", "bobpass123")
        .await
        .expect("bob can log in now");

    // --- Authorization boundaries.
    // Bob cannot download or share-manage the admin's file.
    let resp = get(&app, &format!("/download.lhtml?id={file_id}"), Some(&bob)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // Bob is not an admin.
    let resp = get(&app, "/admin.lhtml", Some(&bob)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // A wrong CSRF token is rejected.
    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&bob),
        "csrf=00000000000000000000000000000000&action=mkdir&folder=&name=X".to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // --- Logout kills the session.
    let bob_csrf = csrf_of(&app, &bob).await;
    let resp = post_form(
        &app,
        "/logout.lhtml",
        Some(&bob),
        format!("csrf={bob_csrf}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let resp = get(&app, "/", Some(&bob)).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(location(&resp).contains("/login.lhtml"));
}

#[actix_web::test]
async fn folder_upload_recreates_the_tree() {
    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    let upload = |payload: Vec<u8>| {
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request()
    };

    // A webkitdirectory upload: relative paths as filenames.
    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("proj/readme.md"), b"# readme"),
            ("file", Some("proj/src/main.lua"), b"print('hi')"),
            ("file", Some("proj/src/util.lua"), b"return {}"),
        ],
    );
    let resp = test::call_service(&app, upload(payload)).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(
        location(&resp).contains("uploaded"),
        "loc: {}",
        location(&resp)
    );

    // Root: exactly one folder ("proj"), no loose files.
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    assert_eq!(page.matches("\u{1F4C1}").count(), 1, "root page: {page}");
    assert!(page.contains(">proj</a>"), "root page: {page}");
    let proj_id = find_between(&page, "\u{1F4C1} <a href=\"/?folder=", "\"")
        .expect("proj folder link")
        .to_string();

    // proj/ has readme.md and the src folder.
    let page = body_string(get(&app, &format!("/?folder={proj_id}"), Some(&admin)).await).await;
    assert!(page.contains("readme.md"), "proj page: {page}");
    assert!(page.contains(">src</a>"), "proj page: {page}");
    let src_id = find_between(&page, "\u{1F4C1} <a href=\"/?folder=", "\"")
        .expect("src folder link")
        .to_string();

    // proj/src/ has both files; the bytes survive the round trip.
    let page = body_string(get(&app, &format!("/?folder={src_id}"), Some(&admin)).await).await;
    assert!(page.contains("main.lua"), "src page: {page}");
    assert!(page.contains("util.lua"), "src page: {page}");
    let file_id = find_between(&page, "/download.lhtml?id=", "\"")
        .expect("download link")
        .to_string();
    let resp = get(&app, &format!("/download.lhtml?id={file_id}"), Some(&admin)).await;
    let bytes = test::read_body(resp).await;
    assert_eq!(&bytes[..], b"print('hi')");

    // Uploading more files into the same tree merges instead of duplicating.
    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("proj/src/new.txt"), b"more"),
        ],
    );
    let resp = test::call_service(&app, upload(payload)).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    assert_eq!(
        page.matches("\u{1F4C1}").count(),
        1,
        "proj must not duplicate: {page}"
    );
    let page = body_string(get(&app, &format!("/?folder={src_id}"), Some(&admin)).await).await;
    assert!(page.contains("new.txt"), "src page: {page}");

    // Hostile traversal-looking segments are defused into plain names.
    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("../evil.txt"), b"nope"),
        ],
    );
    let resp = test::call_service(&app, upload(payload)).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    assert!(page.contains(">_</a>"), "sanitized folder missing: {page}");
}

/// Folder download. Requires an archiver on the host; the feature is
/// discovery-gated, so without one there is nothing to assert.
#[actix_web::test]
async fn folder_download_produces_a_real_archive() {
    if std::process::Command::new("sh")
        .args(["-c", "command -v zip >/dev/null 2>&1"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: no `zip` on this host");
        return;
    }

    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    // A tree with a subfolder and a name that would be hostile unquoted.
    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("bundle/readme.md"), b"# hello"),
            ("file", Some("bundle/deep/data.bin"), b"\x00\x01\xFFbytes"),
            (
                "file",
                Some("bundle/we'ird; rm -rf $(pwd) name.txt"),
                b"still here",
            ),
        ],
    );
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);

    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    let bundle_id = find_between(&page, "\u{1F4C1} <a href=\"/?folder=", "\"")
        .expect("bundle folder")
        .to_string();

    // The UI offers the download because an archiver was discovered.
    assert!(
        page.contains("/archive.lhtml?folder="),
        "no archive link on page: {page}"
    );

    // The request only queues the job; jobs/archive.lua does the building.
    let job = ready_archive_job(&app, &admin, &bundle_id).await;
    let resp = get(
        &app,
        &format!("/archive.lhtml?job={job}&fetch=1"),
        Some(&admin),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let disposition = resp
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(disposition.contains("bundle.zip"), "got: {disposition}");
    let bytes = test::read_body(resp).await;
    assert_eq!(
        &bytes[..2],
        b"PK",
        "not a zip: {:?}",
        &bytes[..4.min(bytes.len())]
    );

    // Unpack it for real and compare the tree.
    let dir = std::env::temp_dir().join(format!("rc-zip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let zip_path = dir.join("out.zip");
    std::fs::write(&zip_path, &bytes).unwrap();
    let status = std::process::Command::new("unzip")
        .args(["-qq", "-o"])
        .arg(&zip_path)
        .arg("-d")
        .arg(&dir)
        .status()
        .expect("unzip runs");
    assert!(status.success(), "unzip failed");

    assert_eq!(
        std::fs::read_to_string(dir.join("readme.md")).unwrap(),
        "# hello"
    );
    assert_eq!(
        std::fs::read(dir.join("deep/data.bin")).unwrap(),
        b"\x00\x01\xFFbytes"
    );
    // The shell-hostile filename survived verbatim, and the shell did not run it.
    assert_eq!(
        std::fs::read_to_string(dir.join("we'ird; rm -rf $(pwd) name.txt")).unwrap(),
        "still here"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[actix_web::test]
async fn folders_can_be_shared_publicly() {
    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    // Two separate trees: one gets shared, the other must stay invisible.
    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("shared/top.txt"), b"top level"),
            ("file", Some("shared/inner/deep.txt"), b"deep file"),
            ("file", Some("private/secret.txt"), b"not yours"),
        ],
    );
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);

    // Find both top-level folders.
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    let mut ids = Vec::new();
    let mut rest = page.as_str();
    while let Some(idx) = rest.find("\u{1F4C1} <a href=\"/?folder=") {
        rest = &rest[idx + "\u{1F4C1} <a href=\"/?folder=".len()..];
        let end = rest.find('"').unwrap();
        let id = rest[..end].to_string();
        let name_start = rest.find('>').unwrap() + 1;
        let name_end = rest[name_start..].find('<').unwrap() + name_start;
        ids.push((id, rest[name_start..name_end].to_string()));
    }
    let shared_id = ids
        .iter()
        .find(|(_, n)| n == "shared")
        .map(|(i, _)| i.clone())
        .expect("shared folder");
    let private_id = ids
        .iter()
        .find(|(_, n)| n == "private")
        .map(|(i, _)| i.clone())
        .expect("private folder");

    // Share the folder.
    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=share_create&kind=folder&id={shared_id}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let page = body_string(
        get(
            &app,
            &format!("/shares.lhtml?folder={shared_id}"),
            Some(&admin),
        )
        .await,
    )
    .await;
    let token = find_between(&page, "class=\"sharelink\">", "<")
        .and_then(|link| link.rsplit("/s/").next().map(str::to_string))
        .expect("share token");

    // Anonymous browsing of the shared tree.
    let page = body_string(get(&app, &format!("/s/{token}"), None).await).await;
    assert!(page.contains("top.txt"), "share page: {page}");
    assert!(page.contains("inner"), "share page: {page}");
    assert!(
        !page.contains("secret.txt"),
        "share leaked another tree: {page}"
    );

    // Descend into the subfolder and download a file, all without a session.
    let inner_id = find_between(&page, "\u{1F4C1} <a href=\"/s/", "\"")
        .and_then(|s| s.split("?f=").nth(1).map(str::to_string))
        .expect("inner folder link");
    let page = body_string(get(&app, &format!("/s/{token}?f={inner_id}"), None).await).await;
    assert!(page.contains("deep.txt"), "inner page: {page}");
    let file_id = find_between(&page, "?dl=", "\"")
        .expect("file link")
        .to_string();
    let resp = get(&app, &format!("/s/{token}?dl={file_id}"), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(&test::read_body(resp).await[..], b"deep file");

    // The share is a boundary: a folder outside it is not reachable through
    // the token, even by guessing its id.
    let resp = get(&app, &format!("/s/{token}?f={private_id}"), None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_string(resp).await;
    assert!(body.contains("not part of this share"), "body: {body}");

    // Revoking kills public access.
    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=share_revoke&kind=folder&id={shared_id}&token={token}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let resp = get(&app, &format!("/s/{token}"), None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn repeated_failed_logins_are_throttled_by_the_app() {
    // nginx also rate limits these endpoints, but the app must not depend on
    // a proxy being in front. The test client sends no X-Real-IP, so every
    // request shares one throttle key — which is the case being tested.
    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;

    // The correct password works before any failures.
    assert!(login(&app, "admin", "adminpass1").await.is_some());

    for _ in 0..12 {
        assert!(
            login(&app, "admin", "wrongpassword").await.is_none(),
            "a wrong password must never open a session"
        );
    }

    // Past the threshold even the *correct* password is refused, which is the
    // whole point: an attacker cannot keep guessing.
    assert!(
        login(&app, "admin", "adminpass1").await.is_none(),
        "throttling must apply once the failure threshold is passed"
    );
    let page = body_string(
        post_form(
            &app,
            "/login.lhtml",
            None,
            "username=admin&password=adminpass1".to_string(),
        )
        .await,
    )
    .await;
    assert!(page.contains("Too many failed attempts"), "page: {page}");
}

#[actix_web::test]
async fn storage_quota_is_enforced() {
    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    // Admin sets their own quota to the smallest the form allows.
    let admin_page = body_string(get(&app, "/admin.lhtml", Some(&admin)).await).await;
    let after = &admin_page[admin_page.find("value=\"set_quota\"").unwrap()..];
    let user_id = find_between(after, "name=\"id\" value=\"", "\"").expect("user id");
    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        // ~1 KiB, small enough that the next upload cannot fit.
        format!("csrf={csrf}&action=set_quota&kind=user&id={user_id}&quota_gib=0.000001"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);

    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("big.bin"), &vec![b'x'; 64 * 1024]),
        ],
    );
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let loc = location(&resp);
    assert!(loc.contains("err="), "over-quota upload must fail: {loc}");

    // And nothing was stored.
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    assert!(!page.contains("big.bin"), "page: {page}");
    assert!(page.contains("Nothing here yet"), "page: {page}");
}

#[actix_web::test]
async fn archive_requests_share_one_job_and_stay_private() {
    if std::process::Command::new("sh")
        .args(["-c", "command -v zip >/dev/null 2>&1"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: no `zip` on this host");
        return;
    }

    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("note.txt"), b"content"),
        ],
    );
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;

    // Two clicks in a row must not queue two exports of the same folder.
    let first = body_string(get(&app, "/archive.lhtml?folder=&json=1", Some(&admin)).await).await;
    let second = body_string(get(&app, "/archive.lhtml?folder=&json=1", Some(&admin)).await).await;
    let a = json_number(&first, "job").expect("first job id");
    let b = json_number(&second, "job").expect("second job id");
    assert_eq!(a, b, "a second request queued another export: {second}");

    // A job id nobody owns is a 404, not a leak — get_job filters on owner,
    // so "never existed" and "someone else's" are the same answer.
    let resp = get(&app, "/archive.lhtml?job=999999", Some(&admin)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Unauthenticated callers do not get to queue work at all.
    let resp = get(&app, "/archive.lhtml?folder=", None).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(
        location(&resp).contains("/login.lhtml"),
        "{}",
        location(&resp)
    );
}

#[actix_web::test]
async fn a_changed_folder_is_never_served_a_stale_archive() {
    if std::process::Command::new("sh")
        .args(["-c", "command -v zip >/dev/null 2>&1"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: no `zip` on this host");
        return;
    }

    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    let upload = |name: &'static str, body: &'static [u8], csrf: String| {
        let payload = multipart_body(
            BOUNDARY,
            &[
                ("csrf", None, csrf.as_bytes()),
                ("action", None, b"upload"),
                ("folder", None, b""),
                ("file", Some(name), body),
            ],
        );
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request()
    };

    test::call_service(&app, upload("a.txt", b"one", csrf.clone())).await;
    let first = ready_archive_job(&app, &admin, "").await;

    // Nothing moved, so the built archive is still exactly right.
    let again = body_string(get(&app, "/archive.lhtml?folder=&json=1", Some(&admin)).await).await;
    assert_eq!(
        json_number(&again, "job"),
        Some(first),
        "an unchanged folder rebuilt instead of reusing: {again}"
    );

    // A new file changes the tree, so the old archive must not be handed out.
    test::call_service(&app, upload("b.txt", b"two", csrf.clone())).await;
    let after_add =
        body_string(get(&app, "/archive.lhtml?folder=&json=1", Some(&admin)).await).await;
    let second = json_number(&after_add, "job").expect("job id");
    assert_ne!(second, first, "stale archive served after an upload");

    // A rename touches neither file count nor total size — only a fingerprint
    // that includes names catches it.
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    // Rows no longer carry forms, so the id comes from the manage link.
    let file_id = find_between(&page, "/manage.lhtml?file=", "\"").expect("a file id");
    post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=rename_file&id={file_id}&folder=&name=renamed.txt"),
    )
    .await;
    let after_rename =
        body_string(get(&app, "/archive.lhtml?folder=&json=1", Some(&admin)).await).await;
    let third = json_number(&after_rename, "job").expect("job id");
    assert_ne!(third, second, "stale archive served after a rename");
}

#[actix_web::test]
async fn an_abandoned_build_is_restarted_on_the_next_poll() {
    if std::process::Command::new("sh")
        .args(["-c", "command -v zip >/dev/null 2>&1"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: no `zip` on this host");
        return;
    }

    let (app, data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("note.txt"), b"content"),
        ],
    );
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;

    let job = ready_archive_job(&app, &admin, "").await;

    // Forge exactly what a restart leaves behind: a row claiming to be under
    // construction, with no thread anywhere that is building it. Nothing polls
    // for such rows, so if the poll did not notice, it would sit there forever.
    let db = data_dir.join("drive/drive.db");
    let conn = rusqlite::Connection::open(&db).expect("open drive db");
    conn.execute(
        "UPDATE archive_jobs SET status = 'building', path = NULL, finished_at = NULL WHERE id = ?",
        [job as i64],
    )
    .expect("forge an abandoned build");
    drop(conn);

    // Polling is the only thing that can rescue it.
    let mut recovered = false;
    for _ in 0..200 {
        let body = body_string(
            get(
                &app,
                &format!("/archive.lhtml?job={job}&json=1"),
                Some(&admin),
            )
            .await,
        )
        .await;
        if body.contains("\"status\":\"ready\"") {
            recovered = true;
            break;
        }
        assert!(
            !body.contains("\"status\":\"failed\""),
            "abandoned job failed instead of restarting: {body}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(recovered, "an abandoned build was never restarted");
}

#[actix_web::test]
async fn concurrent_exports_queue_rather_than_fail() {
    if std::process::Command::new("sh")
        .args(["-c", "command -v zip >/dev/null 2>&1"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: no `zip` on this host");
        return;
    }

    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    // Three folders, so three genuinely different exports are possible. The
    // concurrency cap is lower than this on purpose.
    for name in ["one", "two", "three"] {
        post_form(
            &app,
            "/actions.lhtml",
            Some(&admin),
            format!("csrf={csrf}&action=mkdir&folder=&name={name}"),
        )
        .await;
    }
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    let mut folders: Vec<String> = Vec::new();
    for part in page.split("href=\"/?folder=").skip(1) {
        if let Some(id) = part.split('"').next()
            && !folders.contains(&id.to_string())
        {
            folders.push(id.to_string());
        }
    }
    assert_eq!(folders.len(), 3, "expected three folders: {folders:?}");

    for folder in &folders {
        let payload = multipart_body(
            BOUNDARY,
            &[
                ("csrf", None, csrf.as_bytes()),
                ("action", None, b"upload"),
                ("folder", None, folder.as_bytes()),
                ("file", Some("payload.bin"), b"some bytes to compress"),
            ],
        );
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/actions.lhtml")
                .insert_header((header::COOKIE, format!("session={admin}")))
                .insert_header((
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                ))
                .set_payload(payload)
                .to_request(),
        )
        .await;
    }

    // Fire all three before any of them can finish. Going over the cap must
    // delay an export, never refuse one.
    let mut jobs = Vec::new();
    for folder in &folders {
        let body = body_string(
            get(
                &app,
                &format!("/archive.lhtml?folder={folder}&json=1"),
                Some(&admin),
            )
            .await,
        )
        .await;
        assert!(
            !body.contains("\"status\":\"failed\""),
            "an export was refused instead of queued: {body}"
        );
        jobs.push(json_number(&body, "job").unwrap_or_else(|| panic!("no job id in {body}")));
    }
    assert_eq!(jobs.len(), 3);

    // Every one of them still gets built; the queued ones start as slots free.
    for job in jobs {
        let mut ready = false;
        for _ in 0..400 {
            let body = body_string(
                get(
                    &app,
                    &format!("/archive.lhtml?job={job}&json=1"),
                    Some(&admin),
                )
                .await,
            )
            .await;
            if body.contains("\"status\":\"ready\"") {
                ready = true;
                break;
            }
            assert!(
                !body.contains("\"status\":\"failed\""),
                "job {job} failed: {body}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(ready, "job {job} never completed");
    }
}

/// The front controller owns every path that is not a file: it routes
/// `/s/<token>` and answers the rest in Drive's own chrome.
#[actix_web::test]
async fn unrouted_paths_get_drives_own_404() {
    let (app, _) = app_with(drive_config()).await;

    let resp = get(&app, "/no/such/page", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let page = body_string(resp).await;
    assert!(page.contains("Back to your files"), "page: {page}");
    assert!(page.contains("/no/such/page"), "page: {page}");
    // Rendered by Drive, so it carries the site chrome rather than the
    // server's bare error page.
    assert!(page.contains("🌙 Drive"), "page: {page}");

    // A share-shaped path with no share behind it is the share page's 404,
    // not the router's.
    let resp = get(&app, "/s/0000000000000000000000000000dead", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        body_string(resp).await.contains("Link not found"),
        "expected the share page's own 404"
    );

    // The template is still a file, so it still resolves — but it is not a
    // way to reach a share any more.
    let resp = get(&app, "/s.lhtml", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn the_shares_overview_lists_and_revokes_links() {
    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    // Nothing shared yet.
    let page = body_string(get(&app, "/shares.lhtml", Some(&admin)).await).await;
    assert!(page.contains("not shared anything yet"), "page: {page}");

    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("shared.txt"), b"contents"),
        ],
    );
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;

    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    let file_id = find_between(&page, "shares.lhtml?file=", "\"").expect("a share link");
    post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=share_create&kind=file&id={file_id}&expires="),
    )
    .await;

    // The overview names the shared item and offers the token.
    let page = body_string(get(&app, "/shares.lhtml", Some(&admin)).await).await;
    assert!(page.contains("shared.txt"), "page: {page}");
    assert!(page.contains("sharetoken"), "page: {page}");
    let token = find_between(&page, "name=\"token\" value=\"", "\"")
        .expect("token on the overview")
        .to_string();

    // The link works, then the overview revokes it by token alone.
    let resp = get(&app, &format!("/s/{token}"), None).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=share_revoke&from=all&token={token}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(
        location(&resp).starts_with("/shares.lhtml?"),
        "{}",
        location(&resp)
    );

    let resp = get(&app, &format!("/s/{token}"), None).await;
    assert_ne!(resp.status(), StatusCode::OK, "revoked link still resolves");

    let page = body_string(get(&app, "/shares.lhtml", Some(&admin)).await).await;
    assert!(page.contains("not shared anything yet"), "page: {page}");
}

#[actix_web::test]
async fn the_listing_emits_one_move_select_regardless_of_row_count() {
    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    for name in ["alpha", "beta", "gamma"] {
        post_form(
            &app,
            "/actions.lhtml",
            Some(&admin),
            format!("csrf={csrf}&action=mkdir&folder=&name={name}"),
        )
        .await;
    }
    for file in ["one.txt", "two.txt", "three.txt", "four.txt"] {
        let payload = multipart_body(
            BOUNDARY,
            &[
                ("csrf", None, csrf.as_bytes()),
                ("action", None, b"upload"),
                ("folder", None, b""),
                ("file", Some(file), b"data"),
            ],
        );
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/actions.lhtml")
                .insert_header((header::COOKIE, format!("session={admin}")))
                .insert_header((
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                ))
                .set_payload(payload)
                .to_request(),
        )
        .await;
    }

    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    // The folder <select> used to be repeated per row, making the markup
    // quadratic in folders. It now lives in one dialog for the whole page.
    assert_eq!(
        page.matches(r#"<select name="dest""#).count(),
        1,
        "the move select is emitted more than once"
    );
    assert_eq!(page.matches(r#"<dialog id="manage""#).count(), 1);
    // One per row, folders included now that they can be renamed and moved.
    assert_eq!(
        page.matches("data-manage").count(),
        7,
        "3 folders + 4 files"
    );

    // Without JavaScript the same actions are reachable as a page.
    let file_id = find_between(&page, "/manage.lhtml?file=", "\"").expect("a manage link");
    let page =
        body_string(get(&app, &format!("/manage.lhtml?file={file_id}"), Some(&admin)).await).await;
    for action in ["rename_file", "move_file", "delete_file"] {
        assert!(
            page.contains(action),
            "manage page missing {action}: {page}"
        );
    }

    // And it is still owner-scoped.
    let resp = get(&app, "/manage.lhtml?file=999999", Some(&admin)).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
}

#[actix_web::test]
async fn large_folders_are_listed_a_page_at_a_time() {
    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    // One page is 200 rows: 205 folders and 3 files cross both the page and
    // the folders-to-files boundary.
    for i in 0..205 {
        post_form(
            &app,
            "/actions.lhtml",
            Some(&admin),
            format!("csrf={csrf}&action=mkdir&folder=&name=f{i:03}"),
        )
        .await;
    }
    let payload = multipart_body(
        BOUNDARY,
        &[
            ("csrf", None, csrf.as_bytes()),
            ("action", None, b"upload"),
            ("folder", None, b""),
            ("file", Some("a.txt"), b"a"),
            ("file", Some("b.txt"), b"b"),
            ("file", Some("c.txt"), b"c"),
        ],
    );
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/actions.lhtml")
            .insert_header((header::COOKIE, format!("session={admin}")))
            .insert_header((
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            ))
            .set_payload(payload)
            .to_request(),
    )
    .await;

    let names = |html: &str| -> Vec<String> {
        html.split("data-name=\"")
            .skip(1)
            .map(|s| s[..s.find('"').unwrap()].to_string())
            .collect()
    };

    let first = body_string(get(&app, "/", Some(&admin)).await).await;
    assert_eq!(names(&first).len(), 200);
    assert_eq!(names(&first)[199], "f199");
    let more = find_between(&first, "data-more=\"", "\"")
        .expect("a next-page link")
        .replace("&amp;", "&");
    let page_href = find_between(&first, "<a href=\"/?folder=&amp;ak=", "\"")
        .expect("a no-script next-page link")
        .replace("&amp;", "&");

    let second = body_string(get(&app, &more, Some(&admin)).await).await;
    assert!(!second.contains("<html"), "the fragment is bare rows");
    assert!(!second.contains("data-more"), "the last page links nowhere");
    let rest = names(&second);
    assert_eq!(
        rest,
        [
            "f200", "f201", "f202", "f203", "f204", "a.txt", "b.txt", "c.txt"
        ]
    );

    // Without script the same cursor is a whole page.
    let page =
        body_string(get(&app, &format!("/?folder=&ak={page_href}"), Some(&admin)).await).await;
    assert!(page.contains("<html"));
    assert_eq!(names(&page), rest);

    // Logged out, the fragment redirects to the login page like any other.
    let resp = get(&app, &more, None).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
}

#[actix_web::test]
async fn deleting_a_huge_folder_tree_finishes_and_spares_its_siblings() {
    // On a real disk: under tmpfs a commit costs nothing, and the row-by-row
    // delete this guards against would pass.
    let disk_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "drive-delete-{}",
        common::unique_data_dir()
            .file_name()
            .unwrap()
            .to_string_lossy()
    ));
    let (app, data_dir) = app_with(Config {
        data_dir: disk_dir.clone(),
        ..drive_config()
    })
    .await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    let db = rusqlite::Connection::open(data_dir.join("drive/drive.db")).unwrap();
    let folder_id = |name: &str| -> i64 {
        db.query_row("SELECT id FROM folders WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .unwrap()
    };
    let mkdir = |parent: String, name: &str| {
        post_form(
            &app,
            "/actions.lhtml",
            Some(&admin),
            format!("csrf={csrf}&action=mkdir&folder={parent}&name={name}"),
        )
    };
    mkdir(String::new(), "top").await;
    mkdir(String::new(), "other").await;
    mkdir(folder_id("top").to_string(), "sub").await;
    mkdir(folder_id("sub").to_string(), "deeper").await;

    // Row by row this took minutes and died at the execution limit half done.
    let files_dir = data_dir.join("drive/files");
    let insert = |folder: i64, count: usize, prefix: &str| {
        let tx = db.unchecked_transaction().unwrap();
        for i in 0..count {
            let stored = format!("{prefix}{i}");
            std::fs::write(files_dir.join(&stored), b"x").unwrap();
            tx.execute(
                "INSERT INTO files (owner_id, folder_id, name, stored_name, size, created_at)
                 VALUES (1, ?1, ?2, ?2, 1, 0)",
                rusqlite::params![folder, stored],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    };
    insert(folder_id("top"), 20000, "top-");
    insert(folder_id("deeper"), 3, "deep-");
    insert(folder_id("other"), 2, "other-");

    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!(
            "csrf={csrf}&action=delete_folder&id={}&folder=",
            folder_id("top")
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(location(&resp).contains("msg="), "{}", location(&resp));

    let names: Vec<String> = db
        .prepare("SELECT name FROM folders ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(names, ["other"]);
    let files: i64 = db
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(files, 2);
    let mut blobs: Vec<String> = std::fs::read_dir(&files_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    blobs.sort();
    assert_eq!(blobs, ["other-0", "other-1"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&disk_dir);
}

#[actix_web::test]
async fn folders_can_be_renamed_and_moved_but_never_into_themselves() {
    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    let mkdir = |parent: &str, name: &str, csrf: &str| {
        format!("csrf={csrf}&action=mkdir&folder={parent}&name={name}")
    };
    post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        mkdir("", "outer", &csrf),
    )
    .await;
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    let outer = find_between(&page, "/manage.lhtml?folder=", "\"")
        .expect("outer folder")
        .to_string();
    post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        mkdir(&outer, "inner", &csrf),
    )
    .await;
    let page = body_string(get(&app, &format!("/?folder={outer}"), Some(&admin)).await).await;
    let inner = find_between(&page, "/manage.lhtml?folder=", "\"")
        .expect("inner folder")
        .to_string();

    // Rename works.
    post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=rename_folder&id={outer}&folder=&name=renamed"),
    )
    .await;
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    assert!(page.contains("renamed"), "rename did not take: {page}");

    // Into itself, and into its own descendant, are both refused — either
    // would cut the branch loose in a cycle no listing can reach.
    for dest in [&outer, &inner] {
        let resp = post_form(
            &app,
            "/actions.lhtml",
            Some(&admin),
            format!("csrf={csrf}&action=move_folder&id={outer}&folder=&dest={dest}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert!(
            location(&resp).contains("cannot+be+moved") || location(&resp).contains("itself"),
            "move into {dest} was not refused: {}",
            location(&resp)
        );
    }

    // The manage page never offers a destination that would make a cycle.
    let page =
        body_string(get(&app, &format!("/manage.lhtml?folder={outer}"), Some(&admin)).await).await;
    assert!(
        !page.contains(&format!("option value=\"{outer}\"")),
        "a folder was offered as its own destination"
    );
    assert!(
        !page.contains(&format!("option value=\"{inner}\"")),
        "a descendant was offered as a destination"
    );

    // A legitimate move still works.
    post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        mkdir("", "target", &csrf),
    )
    .await;
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    let target = page
        .split("/manage.lhtml?folder=")
        .skip(1)
        .map(|p| p.split('"').next().unwrap_or("").to_string())
        .find(|id| id != &outer)
        .expect("target folder");
    let resp = post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        format!("csrf={csrf}&action=move_folder&id={outer}&folder=&dest={target}"),
    )
    .await;
    assert!(location(&resp).contains("Moved"), "{}", location(&resp));
}

#[actix_web::test]
async fn move_destinations_show_full_paths_and_exclude_own_subtree() {
    let (app, _data_dir) = app_with(drive_config()).await;
    post_form(
        &app,
        "/register.lhtml",
        None,
        "username=admin&password=adminpass1&password2=adminpass1".to_string(),
    )
    .await;
    let admin = login(&app, "admin", "adminpass1").await.expect("session");
    let csrf = csrf_of(&app, &admin).await;

    let mkdir =
        |parent: &str, name: &str| format!("csrf={csrf}&action=mkdir&folder={parent}&name={name}");
    let id_of = |page: &str, folder_query: &str| -> String {
        find_between(page, folder_query, "\"")
            .unwrap_or_default()
            .to_string()
    };

    post_form(&app, "/actions.lhtml", Some(&admin), mkdir("", "docs")).await;
    post_form(&app, "/actions.lhtml", Some(&admin), mkdir("", "archive")).await;
    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    // Rows are alphabetical, so archive comes first.
    let archive = id_of(&page, "/manage.lhtml?folder=");
    let docs = page
        .split("/manage.lhtml?folder=")
        .skip(1)
        .map(|p| p.split('"').next().unwrap_or("").to_string())
        .find(|id| id != &archive)
        .expect("docs folder");

    // The same folder name in two places: a flat list could not tell them apart.
    post_form(&app, "/actions.lhtml", Some(&admin), mkdir(&docs, "photos")).await;
    post_form(
        &app,
        "/actions.lhtml",
        Some(&admin),
        mkdir(&archive, "photos"),
    )
    .await;

    let page = body_string(get(&app, "/", Some(&admin)).await).await;
    assert!(page.contains(">/docs/photos<"), "no path label: {page}");
    assert!(page.contains(">/archive/photos<"), "no path label: {page}");
    assert!(
        page.contains(r#"data-path="/docs""#),
        "folder rows must carry their path for the dialog filter"
    );

    // Managing /docs must not offer /docs or anything beneath it.
    let page =
        body_string(get(&app, &format!("/manage.lhtml?folder={docs}"), Some(&admin)).await).await;
    assert!(
        page.contains(">/archive<"),
        "unrelated folder missing: {page}"
    );
    assert!(
        page.contains(">/archive/photos<"),
        "unrelated folder missing"
    );
    assert!(!page.contains(">/docs<"), "offered itself as a destination");
    assert!(
        !page.contains(">/docs/photos<"),
        "offered its own descendant"
    );
}
