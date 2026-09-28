use std::{fs, io::Read};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use zipped_file_serving::{Config, app};

fn server(root: &std::path::Path) -> Router {
    let mut config = Config::new(root).unwrap();
    config.compression_threads = 2;
    config.max_upload_bytes = 1024 * 1024;
    app(config).unwrap()
}

async fn request(router: &Router, method: &str, uri: &str, body: Body) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-requested-with", "zipped-file-serving")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn bytes(response: axum::response::Response) -> Vec<u8> {
    response.into_body().collect().await.unwrap().to_bytes().to_vec()
}

fn decode(bytes: &[u8]) -> Vec<u8> {
    let mut decoder = lz4::Decoder::new(bytes).unwrap();
    let mut output = Vec::new();
    decoder.read_to_end(&mut output).unwrap();
    decoder.finish().1.unwrap();
    output
}

#[tokio::test]
async fn browse_upload_and_download_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    let router = server(root.path());
    for asset in ["/", "/app.js", "/style.css"] {
        let response = request(&router, "GET", asset, Body::empty()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert!(!bytes(response).await.is_empty());
    }
    assert_eq!(
        request(&router, "POST", "/api/mkdir?path=sub", Body::empty())
            .await
            .status(),
        StatusCode::CREATED
    );
    let content = b"hello uploaded world".repeat(4096);
    assert_eq!(
        request(
            &router,
            "PUT",
            "/api/upload?path=sub%2Fhello.txt",
            Body::from(content.clone()),
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    assert_eq!(fs::read(root.path().join("sub/hello.txt")).unwrap(), content);
    let listing: Value = serde_json::from_slice(
        &bytes(request(&router, "GET", "/api/list?path=sub", Body::empty()).await).await,
    )
    .unwrap();
    assert_eq!(listing["entries"][0]["name"], "hello.txt");
    for format in ["raw", "lz4"] {
        let response = request(
            &router,
            "GET",
            &format!("/api/download?path=sub%2Fhello.txt&format={format}"),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()["content-disposition"].to_str().unwrap().contains("attachment"));
        let downloaded = bytes(response).await;
        assert_eq!(
            if format == "raw" { downloaded } else { decode(&downloaded) },
            content
        );
    }
    assert_eq!(
        request(&router, "PUT", "/api/upload?path=sub%2Fhello.txt", Body::from("replace"))
            .await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(fs::read(root.path().join("sub/hello.txt")).unwrap(), content);
}

#[tokio::test]
async fn directory_archive_preserves_nested_files_and_empty_directories() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("folder/nested/empty")).unwrap();
    fs::write(root.path().join("folder/nested/data.bin"), [0, 1, 2, 255]).unwrap();
    fs::write(root.path().join("folder/zero"), []).unwrap();
    fs::write(root.path().join("folder/.zfs-upload-active.part"), "hidden").unwrap();
    let router = server(root.path());
    let response = request(&router, "GET", "/api/download?path=folder", Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-disposition"].to_str().unwrap().contains(".tar.lz4"));
    let decoded = decode(&bytes(response).await);
    let mut archive = tar::Archive::new(decoded.as_slice());
    let output = tempfile::tempdir().unwrap();
    archive.unpack(output.path()).unwrap();
    assert_eq!(fs::read(output.path().join("folder/nested/data.bin")).unwrap(), [0, 1, 2, 255]);
    assert!(output.path().join("folder/nested/empty").is_dir());
    assert_eq!(fs::metadata(output.path().join("folder/zero")).unwrap().len(), 0);
    assert!(!output.path().join("folder/.zfs-upload-active.part").exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(root.path().join("folder")).unwrap().count(), 3);
}

#[tokio::test]
async fn rejects_invalid_paths_missing_files_and_oversized_uploads() {
    let root = tempfile::tempdir().unwrap();
    let router = server(root.path());
    for path in ["..%2Fsecret", "C%3A%2Fsecret", "file%3Astream", "CON", "%5C%5Chost"] {
        assert_eq!(
            request(&router, "GET", &format!("/api/list?path={path}"), Body::empty()).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    assert_eq!(
        request(&router, "GET", "/api/download?path=missing", Body::empty()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&router, "GET", "/api/download?format=raw", Body::empty()).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&router, "GET", "/api/download?format=zstd", Body::empty()).await.status(),
        StatusCode::BAD_REQUEST
    );
    let response = router.clone().oneshot(
        Request::builder().method("PUT").uri("/api/upload?path=blocked")
            .body(Body::from("data")).unwrap()
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        request(&router, "PUT", "/api/upload?path=large", Body::from(vec![0; 1024 * 1024 + 1]))
            .await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(!root.path().join("large").exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn interrupted_upload_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let router = server(root.path());
    let stream = futures_util::stream::iter([
        Ok(bytes::Bytes::from_static(b"partial")),
        Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "test disconnect")),
    ]);
    assert_eq!(
        request(&router, "PUT", "/api/upload?path=partial", Body::from_stream(stream))
            .await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_junctions_are_not_served_or_archived() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "outside root").unwrap();
    let junction = root.path().join("junction");
    let status = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(outside.path())
        .status()
        .unwrap();
    assert!(status.success());
    let router = server(root.path());
    assert_eq!(
        request(&router, "GET", "/api/download?path=junction%2Fsecret.txt", Body::empty())
            .await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&router, "PUT", "/api/upload?path=junction%2Fnew.txt", Body::from("outside"))
            .await.status(),
        StatusCode::FORBIDDEN
    );
    let decoded = decode(&bytes(request(&router, "GET", "/api/download", Body::empty()).await).await);
    let mut archive = tar::Archive::new(decoded.as_slice());
    assert_eq!(archive.entries().unwrap().count(), 1);
    fs::remove_dir(junction).unwrap();
}
