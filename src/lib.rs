mod chunk_api;
pub mod client;
mod compression;
mod download;
mod paths;
mod profiles;
pub mod transfer;

use std::{fs, io, path::PathBuf, sync::Arc, time::UNIX_EPOCH};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, sync::Semaphore};
use tower_http::{set_header::SetResponseHeaderLayer, trace::TraceLayer};

#[derive(Clone)]
pub struct Config {
    pub root: PathBuf,
    pub compression_threads: usize,
    pub max_downloads: usize,
    pub max_uploads: usize,
    pub max_upload_bytes: u64,
    pub max_chunk_bytes: u64,
    pub profiles_file: PathBuf,
}

impl Config {
    pub fn new(directory: impl Into<PathBuf>) -> io::Result<Self> {
        let root = fs::canonicalize(directory.into())?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Serving root must be a directory",
            ));
        }
        Ok(Self {
            root,
            compression_threads: std::thread::available_parallelism()?.get().min(8),
            max_downloads: 2,
            max_uploads: 4,
            max_upload_bytes: 100 * 1024 * 1024 * 1024,
            max_chunk_bytes: transfer::DEFAULT_SPLIT,
            profiles_file: profiles::default_file()?,
        })
    }
}

pub(crate) struct AppState {
    config: Config,
    pool: Arc<rayon::ThreadPool>,
    downloads: Arc<Semaphore>,
    uploads: Arc<Semaphore>,
    sessions: chunk_api::Sessions,
}

pub fn app(config: Config) -> io::Result<Router> {
    transfer::validate_split(config.max_chunk_bytes)?;
    if config.compression_threads == 0
        || config.max_downloads == 0
        || config.max_uploads == 0
        || config.max_upload_bytes == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "All resource limits must be positive",
        ));
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.compression_threads)
        .thread_name(|i| format!("lz4-{i}"))
        .build()
        .map_err(io::Error::other)?;
    let state = Arc::new(AppState {
        downloads: Arc::new(Semaphore::new(config.max_downloads)),
        uploads: Arc::new(Semaphore::new(config.max_uploads)),
        sessions: Default::default(),
        pool: Arc::new(pool),
        config,
    });
    Ok(Router::new()
        .route("/", get(|| async {
            ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], include_str!("web/index.html"))
        }))
        .route("/app.js", get(|| async {
            ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("web/app.js"))
        }))
        .route("/style.css", get(|| async {
            ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("web/style.css"))
        }))
        .route("/commands.js", get(|| async {
            ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("web/commands.js"))
        }))
        .route("/profile-ui.js", get(|| async {
            ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("web/profile-ui.js"))
        }))
        .route("/api/profiles", get(profiles::list).post(profiles::create).layer(DefaultBodyLimit::max(32 * 1024)))
        .route("/api/profiles/{id}", put(profiles::update).delete(profiles::delete).layer(DefaultBodyLimit::max(32 * 1024)))
        .route("/api/list", get(list))
        .route("/api/download", get(download::download))
        .route("/api/upload", put(upload))
        .route("/api/mkdir", post(mkdir))
        .route("/api/transfer/manifest", get(chunk_api::manifest))
        .route("/api/transfer/chunk", get(chunk_api::download_chunk))
        .route("/api/transfer/uploads", post(chunk_api::start_upload).layer(DefaultBodyLimit::max(64 * 1024)))
        .route("/api/transfer/uploads/{id}", put(chunk_api::upload_chunk).delete(chunk_api::cancel_upload))
        .route("/api/transfer/uploads/{id}/complete", post(chunk_api::complete_upload))
        .layer(DefaultBodyLimit::disable())
        .layer(SetResponseHeaderLayer::overriding(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")))
        .layer(SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store")))
        .layer(SetResponseHeaderLayer::overriding(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'")))
        .layer(SetResponseHeaderLayer::overriding(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")))
        .layer(TraceLayer::new_for_http())
        .with_state(state))
}

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn busy() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Transfer limit reached; retry after a current transfer finishes",
        )
    }
}

impl From<io::Error> for ApiError {
    fn from(error: io::Error) -> Self {
        let status = match error.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        tracing::warn!(%error, %status, "Filesystem operation failed");
        let message = match status {
            StatusCode::NOT_FOUND => "File or directory not found",
            StatusCode::FORBIDDEN => "Filesystem access denied",
            StatusCode::CONFLICT => "The destination already exists",
            _ => "Filesystem operation failed; see server log",
        };
        Self::new(status, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }
        tracing::warn!(status = %self.status, error = %self.message, "Request failed");
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        tracing::error!(%error, "Filesystem task failed");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Filesystem task failed; see server log",
        )
    })?
}

#[derive(Deserialize, Default)]
pub(crate) struct PathQuery {
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    kind: &'static str,
    size: u64,
    modified: Option<u64>,
}

#[derive(Serialize)]
struct Listing {
    path: String,
    entries: Vec<Entry>,
    max_upload_bytes: u64,
}

async fn list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PathQuery>,
) -> Result<Json<Listing>, ApiError> {
    blocking(move || {
        let path = paths::resolve(&state.config.root, &query.path)?;
        if !path.is_dir() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "The selected path is not a directory",
            ));
        }
        let mut entries = Vec::new();
        for item in fs::read_dir(path)? {
            let item = item?;
            let name = item.file_name().into_string().map_err(|_| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Directory contains a filename that is not valid Unicode",
                )
            })?;
            if name.to_ascii_lowercase().starts_with(paths::UPLOAD_PREFIX) {
                continue;
            }
            let metadata = fs::symlink_metadata(item.path())?;
            let kind = if paths::is_link(&metadata) || paths::components(&name).is_err() {
                "blocked"
            } else if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else {
                "blocked"
            };
            entries.push(Entry {
                name,
                kind,
                size: metadata.len(),
                modified: metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
            });
        }
        entries.sort_by(|a, b| {
            (a.kind != "directory")
                .cmp(&(b.kind != "directory"))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(Json(Listing {
            path: query.path,
            entries,
            max_upload_bytes: state.config.max_upload_bytes,
        }))
    })
    .await
}

fn require_write_header(headers: &HeaderMap) -> Result<(), ApiError> {
    if headers
        .get("x-requested-with")
        .and_then(|v| v.to_str().ok())
        != Some("zipped-file-serving")
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Writes require X-Requested-With: zipped-file-serving",
        ));
    }
    Ok(())
}

async fn mkdir(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    require_write_header(&headers)?;
    blocking(move || {
        let destination = paths::destination(&state.config.root, &query.path)?;
        fs::create_dir(destination)?;
        Ok(StatusCode::CREATED)
    })
    .await
}

async fn upload(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    require_write_header(&headers)?;
    let _permit = state
        .uploads
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::busy())?;
    let length = headers
        .get(header::CONTENT_LENGTH)
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid Content-Length"))
        })
        .transpose()?;
    if length.is_some_and(|n| n > state.config.max_upload_bytes) {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "File exceeds the upload size limit",
        ));
    }
    let setup_state = state.clone();
    let (destination, temp) = blocking(move || {
        let destination = paths::destination(&setup_state.config.root, &query.path)?;
        let parent = destination
            .parent()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Destination has no parent"))?;
        let temp = tempfile::Builder::new()
            .prefix(paths::UPLOAD_PREFIX)
            .suffix(".part")
            .tempfile_in(parent)?;
        Ok((destination, temp))
    })
    .await?;
    let (file, temp_path) = temp.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = body.into_data_stream();
    let mut received = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            tracing::warn!(%error, "Upload body interrupted");
            ApiError::new(StatusCode::BAD_REQUEST, "Upload body interrupted")
        })?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "Upload size overflow"))?;
        if received > state.config.max_upload_bytes {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "File exceeds the upload size limit",
            ));
        }
        file.write_all(&chunk).await?;
    }
    if length.is_some_and(|expected| expected != received) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Incomplete upload"));
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    blocking(move || {
        temp_path
            .persist_noclobber(destination)
            .map_err(|error| ApiError::from(error.error))?;
        Ok(StatusCode::CREATED)
    })
    .await
}
