use std::{fs, path::Path};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use zipped_file_serving::{Config, app};

fn server(root: &Path, file: &Path) -> Router {
    let mut config = Config::new(root).unwrap();
    config.compression_threads = 1;
    config.profiles_file = file.to_owned();
    app(config).unwrap()
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    value: Value,
    write_header: bool,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if write_header {
        request = request.header("x-requested-with", "zipped-file-serving");
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(value.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| json!({"text": String::from_utf8_lossy(&body)})),
    )
}

fn input(revision: u64, name: &str) -> Value {
    json!({"revision": revision, "name": name, "client_path": "C:\\Client tools\\zipped-file-client.exe", "download_folder": "D:\\Downloaded data"})
}

#[tokio::test]
async fn profiles_persist_across_restarts_and_support_conflict_safe_crud() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join("profiles.json");
    let router = server(root.path(), &file);
    assert_eq!(
        call(&router, "GET", "/api/profiles", Value::Null, false).await,
        (
            StatusCode::OK,
            json!({"version":1,"revision":0,"profiles":[]})
        )
    );
    assert_eq!(
        call(&router, "POST", "/api/profiles", input(0, "Desktop"), false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, created) = call(&router, "POST", "/api/profiles", input(0, "Desktop"), true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["revision"], 1);
    assert_eq!(created["profiles"][0]["name"], "Desktop");
    let id = created["profiles"][0]["id"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok());
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&file).unwrap()).unwrap(),
        created
    );
    drop(router);
    let restarted = server(root.path(), &file);
    assert_eq!(
        call(&restarted, "GET", "/api/profiles", Value::Null, false)
            .await
            .1,
        created
    );
    let uri = format!("/api/profiles/{id}");
    assert_eq!(
        call(&restarted, "PUT", &uri, input(0, "Stale"), true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &restarted,
            "POST",
            "/api/profiles",
            input(1, "desktop"),
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let mut edited = input(1, "Laptop");
    edited["client_path"] = json!("\\\\workstation\\tools\\Client's \u{2019} app.exe");
    edited["download_folder"] = json!("\\\\workstation\\downloads\\");
    let (status, updated) = call(&restarted, "PUT", &uri, edited, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["profiles"][0]["name"], "Laptop");
    assert_eq!(updated["revision"], 2);
    assert_eq!(
        call(
            &restarted,
            "DELETE",
            &format!("{uri}?revision=1"),
            Value::Null,
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &restarted,
            "DELETE",
            &format!("{uri}?revision=2"),
            Value::Null,
            false
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, deleted) = call(
        &restarted,
        "DELETE",
        &format!("{uri}?revision=2"),
        Value::Null,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["profiles"], json!([]));
    assert_eq!(deleted["revision"], 3);
    assert_eq!(
        call(
            &restarted,
            "DELETE",
            &format!("{uri}?revision=3"),
            Value::Null,
            true
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn exactly_99_profiles_are_allowed_and_existing_profiles_remain_editable() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join("profiles.json");
    let router = server(root.path(), &file);
    let mut last_id = String::new();
    for revision in 0..99 {
        let (status, store) = call(
            &router,
            "POST",
            "/api/profiles",
            input(revision, &format!("Profile {revision}")),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            store["profiles"].as_array().unwrap().len(),
            revision as usize + 1
        );
        last_id = store["profiles"].as_array().unwrap().last().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
    }
    let before = fs::read(&file).unwrap();
    assert_eq!(
        call(
            &router,
            "POST",
            "/api/profiles",
            input(99, "Profile 100"),
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(fs::read(&file).unwrap(), before);
    let uri = format!("/api/profiles/{last_id}");
    assert_eq!(
        call(&router, "PUT", &uri, input(99, "Renamed at cap"), true)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &router,
            "DELETE",
            &format!("{uri}?revision=100"),
            Value::Null,
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &router,
            "POST",
            "/api/profiles",
            input(101, "Replacement"),
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, store) = call(&router, "GET", "/api/profiles", Value::Null, false).await;
    assert_eq!(store["profiles"].as_array().unwrap().len(), 99);
}

#[tokio::test]
async fn rejects_invalid_profiles_large_bodies_and_unsafe_storage() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join("profiles.json");
    let router = server(root.path(), &file);
    for (field, value) in [
        ("name", ""),
        ("name", " spaced "),
        ("name", "line\nbreak"),
        ("client_path", "client.exe"),
        ("client_path", "C:\\tools\\script.ps1"),
        ("client_path", "C:\\tools\\..\\client.exe"),
        ("client_path", "C:\\tools\\CON.exe"),
        ("download_folder", "C:relative"),
        ("download_folder", "C:\\a\\\\b"),
        ("download_folder", "\\\\server"),
        ("download_folder", "\\\\server\\"),
        ("download_folder", "C:\\folder\ninjection"),
        ("download_folder", "C:\\data:stream"),
    ] {
        let mut request = input(0, "Invalid");
        request[field] = json!(value);
        assert_eq!(
            call(&router, "POST", "/api/profiles", request, true)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{field}: {value}"
        );
    }
    let mut oversized = input(0, "Large");
    oversized["name"] = json!("x".repeat(40 * 1024));
    assert_eq!(
        call(&router, "POST", "/api/profiles", oversized, true)
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(!file.exists());
    let unsafe_router = server(root.path(), &root.path().join("profiles.json"));
    assert_eq!(
        call(
            &unsafe_router,
            "POST",
            "/api/profiles",
            input(0, "Hidden"),
            true
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(!root.path().join("profiles.json").exists());
}

#[tokio::test]
async fn invalid_or_unwritable_storage_is_not_silently_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join("profiles.json");
    fs::write(&file, b"broken json").unwrap();
    let router = server(root.path(), &file);
    assert_eq!(
        call(&router, "GET", "/api/profiles", Value::Null, false)
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        call(&router, "POST", "/api/profiles", input(0, "New"), true)
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(fs::read(&file).unwrap(), b"broken json");
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    assert_ne!(
        call(&router, "POST", "/api/profiles", input(0, "New"), true)
            .await
            .0,
        StatusCode::OK
    );
    assert!(file.is_dir());
}

#[tokio::test]
async fn concurrent_updates_do_not_lose_profiles() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join("profiles.json");
    let first = server(root.path(), &file);
    let second = server(root.path(), &file);
    let (a, b) = tokio::join!(
        call(&first, "POST", "/api/profiles", input(0, "First"), true),
        call(&second, "POST", "/api/profiles", input(0, "Second"), true)
    );
    assert!(a.0 == StatusCode::OK || b.0 == StatusCode::OK);
    assert!(a.0 != StatusCode::OK || b.0 != StatusCode::OK);
    let rejected = if a.0 == StatusCode::OK { b.0 } else { a.0 };
    assert!(matches!(
        rejected,
        StatusCode::CONFLICT | StatusCode::SERVICE_UNAVAILABLE
    ));
    assert_eq!(
        call(&first, "GET", "/api/profiles", Value::Null, false)
            .await
            .1["profiles"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
