use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use serde::Deserialize;
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::{StreamReader, SyncIoBridge};

use crate::{
    ApiError, AppState, blocking,
    download::{ChannelWriter, DownloadStream},
    paths, require_write_header,
    transfer::{self, ChunkAck, Codec, Manifest, ManifestEntry, UploadInfo, UploadRequest},
};

pub(crate) type Sessions = Mutex<HashMap<String, Arc<AsyncMutex<Upload>>>>;

pub(crate) struct Upload {
    temp: Option<tempfile::NamedTempFile>,
    destination: std::path::PathBuf,
    size: u64,
    offset: u64,
    chunk_size: u64,
    codec: Codec,
    touched: Instant,
    _permit: OwnedSemaphorePermit,
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}

fn negotiate(state: &AppState, requested: u64) -> Result<u64, ApiError> {
    transfer::validate_split(requested).map_err(|e| bad(e.to_string()))?;
    Ok(requested.min(state.config.max_chunk_bytes))
}

fn stamp(metadata: &fs::Metadata) -> io::Result<String> {
    let time = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    Ok(format!("{}-{}", metadata.len(), time.as_nanos()))
}

#[derive(Deserialize)]
pub(crate) struct ManifestQuery {
    #[serde(default)]
    path: String,
    chunk_size: u64,
    codec: Codec,
}

pub(crate) async fn manifest(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ManifestQuery>,
) -> Result<Response, ApiError> {
    let chunk_size = negotiate(&state, query.chunk_size)?;
    blocking(move || {
        let root = paths::resolve(&state.config.root, &query.path)?;
        let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("download").to_owned();
        paths::components(&name)?;
        let mut entries = Vec::new();
        let mut pending = vec![(root, name, query.path)];
        while let Some((path, relative, source)) = pending.pop() {
            let metadata = fs::symlink_metadata(&path)?;
            if paths::is_link(&metadata) || (!metadata.is_dir() && !metadata.is_file()) {
                tracing::info!(path = %path.display(), "Omitting link or special file from manifest");
                continue;
            }
            let directory = metadata.is_dir();
            entries.push(ManifestEntry {
                path: relative.clone(), source: source.clone(), directory,
                size: if directory { 0 } else { metadata.len() },
                stamp: if directory { String::new() } else { stamp(&metadata)? },
            });
            if entries.len() + pending.len() > transfer::MAX_ENTRIES {
                return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "Manifest exceeds 100000 entries; transfer smaller subdirectories"));
            }
            if directory {
                for entry in fs::read_dir(path)? {
                    let entry = entry?;
                    let name = entry.file_name().into_string().map_err(|_| bad("Filename is not Unicode"))?;
                    if name.to_ascii_lowercase().starts_with(paths::UPLOAD_PREFIX) { continue; }
                    paths::components(&name)?;
                    pending.push((
                        entry.path(), format!("{relative}/{name}"),
                        if source.is_empty() { name } else { format!("{source}/{name}") },
                    ));
                    if entries.len() + pending.len() > transfer::MAX_ENTRIES {
                        return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "Manifest exceeds 100000 entries"));
                    }
                }
            }
        }
        let manifest = Manifest {
            protocol: transfer::PROTOCOL.into(), hash: transfer::HASH.into(),
            chunk_size, codec: query.codec, entries,
        };
        let body = serde_json::to_vec(&manifest).map_err(io::Error::other)?;
        if body.len() as u64 > transfer::MAX_MANIFEST {
            return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "Manifest exceeds 16 MiB"));
        }
        Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
    }).await
}

#[derive(Deserialize)]
pub(crate) struct ChunkQuery {
    path: String,
    stamp: String,
    offset: u64,
    chunk_size: u64,
    codec: Codec,
}

pub(crate) async fn download_chunk(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ChunkQuery>,
) -> Result<Response, ApiError> {
    let chunk_size = negotiate(&state, query.chunk_size)?;
    if chunk_size != query.chunk_size || query.offset % chunk_size != 0 {
        return Err(bad("Chunk size or offset differs from negotiation"));
    }
    let permit = Arc::new(
        state
            .downloads
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::busy())?,
    );
    let setup = state.clone();
    let expected_stamp = query.stamp.clone();
    let (mut file, length) = blocking(move || {
        let path = paths::resolve(&setup.config.root, &query.path)?;
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(bad("Chunk source must be a regular file"));
        }
        if stamp(&metadata)? != query.stamp {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Source changed; request a new manifest",
            ));
        }
        if query.offset >= metadata.len() {
            return Err(bad("Offset is outside the file"));
        }
        file.seek(SeekFrom::Start(query.offset))?;
        Ok((file, chunk_size.min(metadata.len() - query.offset)))
    })
    .await?;
    let (send, receive) = mpsc::channel(8);
    let body_permit = permit.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut writer = ChannelWriter {
            sender: send.clone(),
            buffer: Vec::with_capacity(transfer::BUFFER_SIZE),
        };
        let result = (|| {
            transfer::encode_chunk(
                &mut file,
                &mut writer,
                query.codec,
                query.offset,
                length,
                state.pool.clone(),
            )?;
            if stamp(&file.metadata()?)? != expected_stamp {
                return Err(transfer::invalid("Source changed during chunk read"));
            }
            writer.flush()
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "Chunk download failed");
            let _ = send.blocking_send(Err(error));
        }
    });
    Ok((
        [(header::CONTENT_TYPE, "application/vnd.zfs.chunk")],
        Body::from_stream(DownloadStream {
            receiver: ReceiverStream::new(receive),
            _permit: body_permit,
        }),
    )
        .into_response())
}

fn sessions(
    state: &AppState,
) -> Result<std::sync::MutexGuard<'_, HashMap<String, Arc<AsyncMutex<Upload>>>>, ApiError> {
    let mut sessions = state.sessions.lock().map_err(|error| {
        tracing::error!(%error, "Upload session lock poisoned");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Upload session state unavailable",
        )
    })?;
    sessions.retain(|id, session| {
        let Ok(session) = session.try_lock() else {
            return true;
        };
        let alive = session.touched.elapsed() < Duration::from_secs(15 * 60);
        if !alive {
            tracing::warn!(%id, "Expired inactive upload session");
        }
        alive
    });
    Ok(sessions)
}

fn session(state: &AppState, id: &str) -> Result<Arc<AsyncMutex<Upload>>, ApiError> {
    sessions(state)?
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Upload session missing or expired"))
}

pub(crate) async fn start_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<UploadRequest>,
) -> Result<Json<UploadInfo>, ApiError> {
    require_write_header(&headers)?;
    let chunk_size = negotiate(&state, request.chunk_size)?;
    if request.size > state.config.max_upload_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "File exceeds upload size limit",
        ));
    }
    drop(sessions(&state)?);
    let permit = state
        .uploads
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::busy())?;
    blocking(move || {
        let destination = paths::destination(&state.config.root, &request.path)?;
        let temp = tempfile::Builder::new()
            .prefix(paths::UPLOAD_PREFIX)
            .suffix(".part")
            .tempfile_in(
                destination
                    .parent()
                    .ok_or_else(|| bad("Destination has no parent"))?,
            )?;
        let id = uuid::Uuid::new_v4().to_string();
        sessions(&state)?.insert(
            id.clone(),
            Arc::new(AsyncMutex::new(Upload {
                temp: Some(temp),
                destination,
                size: request.size,
                offset: 0,
                chunk_size,
                codec: request.codec,
                touched: Instant::now(),
                _permit: permit,
            })),
        );
        Ok(Json(UploadInfo {
            id,
            protocol: transfer::PROTOCOL.into(),
            hash: transfer::HASH.into(),
            chunk_size,
            codec: request.codec,
            offset: 0,
        }))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct OffsetQuery {
    offset: u64,
}

pub(crate) async fn upload_chunk(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<OffsetQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<ChunkAck>, ApiError> {
    require_write_header(&headers)?;
    let mut upload = session(&state, &id)?
        .try_lock_owned()
        .map_err(|_| ApiError::busy())?;
    if upload.offset != query.offset || upload.offset >= upload.size || upload.temp.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Unexpected upload offset or completed session",
        ));
    }
    let reader = SyncIoBridge::new(StreamReader::new(
        body.into_data_stream().map_err(io::Error::other),
    ));
    blocking(move || {
        let offset = upload.offset;
        let length = upload.chunk_size.min(upload.size - offset);
        let codec = upload.codec;
        let file = upload
            .temp
            .as_mut()
            .ok_or_else(|| bad("Session is already complete"))?
            .as_file_mut();
        file.seek(SeekFrom::Start(offset))?;
        let result = transfer::decode_chunk(reader, &mut *file, codec, offset, length);
        let hash = match result {
            Ok(hash) => {
                file.flush()?;
                hash
            }
            Err(error) => {
                file.set_len(offset)?;
                file.seek(SeekFrom::Start(offset))?;
                upload.touched = Instant::now();
                tracing::warn!(%error, "Rejected upload chunk; rolled back to verified offset");
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    error.to_string(),
                ));
            }
        };
        upload.offset += length;
        upload.touched = Instant::now();
        Ok(Json(ChunkAck {
            next_offset: upload.offset,
            hash,
        }))
    })
    .await
}

pub(crate) async fn complete_upload(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    require_write_header(&headers)?;
    let mut upload = session(&state, &id)?
        .try_lock_owned()
        .map_err(|_| ApiError::busy())?;
    blocking(move || {
        if upload.offset != upload.size {
            return Err(ApiError::new(StatusCode::CONFLICT, "Upload is incomplete"));
        }
        let temp = upload
            .temp
            .take()
            .ok_or_else(|| bad("Session is already complete"))?;
        temp.as_file().sync_all()?;
        if let Err(error) = temp.persist_noclobber(&upload.destination) {
            upload.temp = Some(error.file);
            return Err(error.error.into());
        }
        sessions(&state)?.remove(&id);
        Ok(StatusCode::CREATED)
    })
    .await
}

pub(crate) async fn cancel_upload(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    require_write_header(&headers)?;
    let _guard = session(&state, &id)?
        .try_lock_owned()
        .map_err(|_| ApiError::busy())?;
    sessions(&state)?.remove(&id);
    Ok(StatusCode::NO_CONTENT)
}
