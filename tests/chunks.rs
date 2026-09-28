use std::{fs, io::{self, Read}, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use tower::ServiceExt;
use zipped_file_serving::{
    Config, app,
    transfer::{self, ChunkAck, Codec, Manifest, UploadInfo},
};

async fn call(router: &Router, method: &str, uri: &str, body: Vec<u8>) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .header("x-requested-with", "zipped-file-serving")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn bytes(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn negotiated_chunks_roundtrip_with_integrity_rollback_and_no_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::new(root.path()).unwrap();
    config.max_chunk_bytes = transfer::MIN_SPLIT;
    config.compression_threads = 2;
    let router = app(config).unwrap();
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap(),
    );
    let data = vec![b'x'; 2 * transfer::MIN_SPLIT as usize + 17];
    fs::write(root.path().join("source"), &data).unwrap();
    for codec in [Codec::Lz4, Codec::Zstd] {
        let response = call(
            &router,
            "GET",
            &format!(
                "/api/transfer/manifest?path=source&chunk_size={}&codec={}",
                transfer::DEFAULT_SPLIT,
                codec.name()
            ),
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let manifest: Manifest = serde_json::from_slice(&bytes(response).await).unwrap();
        assert_eq!(manifest.chunk_size, transfer::MIN_SPLIT);
        assert_eq!(manifest.hash, transfer::HASH);
        let entry = &manifest.entries[0];
        let request = serde_json::to_vec(&transfer::UploadRequest {
            path: format!("uploaded-{}", codec.name()),
            size: data.len() as u64,
            chunk_size: transfer::DEFAULT_SPLIT,
            codec,
        })
        .unwrap();
        let response = call(&router, "POST", "/api/transfer/uploads", request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let info: UploadInfo = serde_json::from_slice(&bytes(response).await).unwrap();
        assert_eq!(info.chunk_size, transfer::MIN_SPLIT);
        let session = format!("/api/transfer/uploads/{}", info.id);
        assert_eq!(
            call(&router, "POST", &format!("{session}/complete"), vec![])
                .await
                .status(),
            StatusCode::CONFLICT
        );
        let mut reconstructed = Vec::new();
        for offset in (0..data.len()).step_by(info.chunk_size as usize) {
            let length = info.chunk_size.min(data.len() as u64 - offset as u64);
            let response = call(&router, "GET", &format!("/api/transfer/chunk?path=source&stamp={}&offset={offset}&chunk_size={}&codec={}", entry.stamp, info.chunk_size, codec.name()), vec![]).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = bytes(response).await;
            let downloaded_hash = transfer::decode_chunk(
                body.as_slice(),
                &mut reconstructed,
                codec,
                offset as u64,
                length,
            )
            .unwrap();
            let mut encoded = Vec::new();
            let encoded_hash = transfer::encode_chunk(
                &data[offset..],
                &mut encoded,
                codec,
                offset as u64,
                length,
                pool.clone(),
            )
            .unwrap();
            assert_eq!(encoded_hash, downloaded_hash);
            let mut corrupt = encoded.clone();
            *corrupt.last_mut().unwrap() ^= 1;
            assert_eq!(
                call(
                    &router,
                    "PUT",
                    &format!("{session}?offset={offset}"),
                    corrupt
                )
                .await
                .status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
            assert!(
                !root
                    .path()
                    .join(format!("uploaded-{}", codec.name()))
                    .exists()
            );
            let response = call(
                &router,
                "PUT",
                &format!("{session}?offset={offset}"),
                encoded.clone(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let ack: ChunkAck = serde_json::from_slice(&bytes(response).await).unwrap();
            assert_eq!(ack.next_offset, offset as u64 + length);
            assert_eq!(ack.hash, encoded_hash);
            assert_eq!(
                call(
                    &router,
                    "PUT",
                    &format!("{session}?offset={offset}"),
                    encoded
                )
                .await
                .status(),
                StatusCode::CONFLICT
            );
        }
        assert_eq!(reconstructed, data);
        assert_eq!(
            call(&router, "POST", &format!("{session}/complete"), vec![])
                .await
                .status(),
            StatusCode::CREATED
        );
        assert_eq!(
            fs::read(root.path().join(format!("uploaded-{}", codec.name()))).unwrap(),
            data
        );
        assert_eq!(
            call(&router, "DELETE", &session, vec![]).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
}

#[tokio::test]
async fn invalid_negotiation_changed_sources_and_cancelled_sessions() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("source"), "data").unwrap();
    let router = app(Config::new(root.path()).unwrap()).unwrap();
    for size in [0, transfer::MIN_SPLIT - 1, transfer::MAX_SPLIT + 1] {
        assert_eq!(
            call(
                &router,
                "GET",
                &format!("/api/transfer/manifest?chunk_size={size}&codec=lz4"),
                vec![]
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(
            &router,
            "GET",
            "/api/transfer/chunk?path=source&stamp=stale&offset=0&chunk_size=1048576&codec=lz4",
            vec![]
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let info: UploadInfo = serde_json::from_slice(
        &bytes(
            call(
                &router,
                "POST",
                "/api/transfer/uploads",
                serde_json::to_vec(&transfer::UploadRequest {
                    path: "new".into(),
                    size: 42,
                    chunk_size: transfer::MIN_SPLIT,
                    codec: Codec::Lz4,
                })
                .unwrap(),
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert_eq!(
        call(
            &router,
            "DELETE",
            &format!("/api/transfer/uploads/{}", info.id),
            vec![]
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn streaming_tar_decoders_enforce_integrity_and_output_limits() {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(7);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, "folder/file.txt", &b"content"[..])
        .unwrap();
    let data = tar.into_inner().unwrap();
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap(),
    );
    for codec in [Codec::Lz4, Codec::Zstd] {
        let mut encoder = transfer::Encoder::new(Vec::new(), codec, pool.clone()).unwrap();
        io::copy(&mut data.as_slice(), &mut encoder).unwrap();
        let encoded = encoder.finish().unwrap();
        struct Fragmented<'a>(&'a [u8]);
        impl Read for Fragmented<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let length = bytes.len().min(1);
                self.0.read(&mut bytes[..length])
            }
        }
        let fragmented_output = tempfile::tempdir().unwrap();
        zipped_file_serving::client::extract_archive(Fragmented(encoded.as_slice()), fragmented_output.path(), 1024 * 1024).unwrap();
        let output = tempfile::tempdir().unwrap();
        zipped_file_serving::client::extract_archive(
            encoded.as_slice(),
            output.path(),
            1024 * 1024,
        )
        .unwrap();
        assert_eq!(
            fs::read(output.path().join("folder/file.txt")).unwrap(),
            b"content"
        );
        let output = tempfile::tempdir().unwrap();
        assert!(
            zipped_file_serving::client::extract_archive(encoded.as_slice(), output.path(), 10)
                .is_err()
        );
        let output = tempfile::tempdir().unwrap();
        assert!(
            zipped_file_serving::client::extract_archive(
                &encoded[..encoded.len() - 1],
                output.path(),
                1024 * 1024
            )
            .is_err()
        );
    }
}

#[test]
fn extraction_rejects_parent_paths_links_and_duplicate_files() {
    for unsafe_kind in ["parent", "link", "duplicate"] {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(0);
        if unsafe_kind == "parent" {
            header.as_mut_bytes()[..9].copy_from_slice(b"../escape");
        } else {
            header.set_path("file").unwrap();
        }

        #[test]
        fn extraction_rejects_large_metadata_before_buffering_it() {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::GNULongName);
            header.set_size(256 * 1024 * 1024);
            header.set_cksum();
            let encoded = zstd::stream::encode_all(&header.as_bytes()[..], 1).unwrap();
            let output = tempfile::tempdir().unwrap();
            let error = zipped_file_serving::client::extract_archive(encoded.as_slice(), output.path(), 1024 * 1024 * 1024).unwrap_err();
            assert!(error.to_string().contains("metadata record exceeds"), "{error}");
        }
        if unsafe_kind == "link" {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("../outside").unwrap();
        }
        header.set_cksum();
        tar.append(&header, io::empty()).unwrap();
        if unsafe_kind == "duplicate" {
            tar.append(&header, io::empty()).unwrap();
        }
        let data = tar.into_inner().unwrap();
        let compressed = zstd::stream::encode_all(data.as_slice(), 1).unwrap();
        let output = tempfile::tempdir().unwrap();
        assert!(
            zipped_file_serving::client::extract_archive(
                compressed.as_slice(),
                output.path(),
                1024 * 1024
            )
            .is_err(),
            "{unsafe_kind}"
        );
    }
}
