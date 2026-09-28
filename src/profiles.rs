use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
    sync::Arc,
};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::{ApiError, AppState, blocking, paths, require_write_header};

const MAX_PROFILES: usize = 99;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

pub(crate) fn default_file() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "No user configuration directory is available",
            )
        })?;
    Ok(base.join("ZippedServing").join("profiles.json"))
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    id: String,
    name: String,
    client_path: String,
    download_folder: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Store {
    version: u32,
    pub(crate) revision: u64,
    profiles: Vec<Profile>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            profiles: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    revision: u64,
    name: String,
    client_path: String,
    download_folder: String,
}

#[derive(Deserialize)]
pub(crate) struct Revision {
    revision: u64,
}

fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.trim().is_empty()
        || name != name.trim()
        || name.len() > 100
        || name.chars().any(char::is_control)
    {
        return Err(
            "Profile name must be 1-100 UTF-8 bytes, without surrounding whitespace or control characters",
        );
    }
    Ok(())
}

fn validate_path(path: &str, executable: bool) -> Result<(), &'static str> {
    if path.len() > 4096 || path.chars().any(char::is_control) || path.contains('/') {
        return Err(
            "Use a Windows path of at most 4096 UTF-8 bytes, without control characters or forward slashes",
        );
    }
    let bytes = path.as_bytes();
    let rest = if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":\\" {
        &path[3..]
    } else if let Some(rest) = path.strip_prefix("\\\\") {
        if rest.trim_end_matches('\\').split('\\').count() < if executable { 3 } else { 2 } {
            return Err("UNC paths must include a server and share");
        }
        rest
    } else {
        return Err("Use an absolute Windows drive path or UNC path on the client machine");
    };
    let rest = if executable {
        rest
    } else {
        rest.trim_end_matches('\\')
    };
    if !rest.is_empty() {
        for part in rest.split('\\') {
            if part.is_empty() {
                return Err("Path contains an empty component");
            }
            paths::components(part)
                .map_err(|_| "Path contains an invalid or reserved Windows filename")?;
        }
    }
    if executable && (!path.to_ascii_lowercase().ends_with(".exe") || rest.is_empty()) {
        return Err("Client binary path must name an .exe file");
    }
    Ok(())
}

fn validate_profile(profile: &Profile) -> Result<(), &'static str> {
    validate_name(&profile.name)?;
    validate_path(&profile.client_path, true)?;
    validate_path(&profile.download_folder, false)
}

fn invalid_store(message: impl Into<String>) -> ApiError {
    let message = message.into();
    tracing::error!(%message, "Profile store is invalid; refusing to overwrite");
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Profile storage is invalid; see server log. Existing data was not overwritten.",
    )
}

fn read_store(file: &std::path::Path) -> Result<Store, ApiError> {
    let input = match File::open(file) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    input.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid_store("Profile store exceeds 1 MiB"));
    }
    let store: Store =
        serde_json::from_slice(&bytes).map_err(|error| invalid_store(error.to_string()))?;
    if store.version != 1 || store.profiles.len() > MAX_PROFILES {
        return Err(invalid_store(
            "Unsupported version or more than 99 profiles",
        ));
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for profile in &store.profiles {
        validate_profile(profile).map_err(invalid_store)?;
        if uuid::Uuid::parse_str(&profile.id).is_err()
            || !ids.insert(&profile.id)
            || !names.insert(profile.name.to_lowercase())
        {
            return Err(invalid_store("Invalid or duplicate profile IDs/names"));
        }
    }
    Ok(store)
}

fn transaction(
    state: &AppState,
    change: impl FnOnce(&mut Store) -> Result<bool, ApiError>,
) -> Result<Json<Store>, ApiError> {
    let file = &state.config.profiles_file;
    let parent = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    fs::create_dir_all(parent)?;
    let parent = fs::canonicalize(parent)?;
    if parent.starts_with(&state.config.root) {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Profile storage must be outside the served directory; configure --profiles-file",
        ));
    }
    let name = file
        .file_name()
        .ok_or_else(|| invalid_store("Profile storage must name a file"))?;
    let file = parent.join(name);
    let lock_path = parent.join(format!("{}.lock", name.to_string_lossy()));
    for path in [&file, &lock_path] {
        match fs::symlink_metadata(path) {
            Ok(metadata) if paths::is_link(&metadata) => {
                return Err(invalid_store(
                    "Profile storage must not be a link or reparse point",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    FileExt::try_lock_exclusive(&lock).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock
            || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Profile storage is busy; retry",
            )
        } else {
            error.into()
        }
    })?;
    let mut store = read_store(&file)?;
    if change(&mut store)? {
        store.revision = store
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_store("Profile revision overflow"))?;
        let bytes = serde_json::to_vec_pretty(&store).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid_store("Serialized profile store exceeds 1 MiB"));
        }
        let mut temp = tempfile::Builder::new()
            .prefix(".zfs-profiles-")
            .tempfile_in(parent)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(file)
            .map_err(|error| ApiError::from(error.error))?;
    }
    Ok(Json(store))
}

fn check_revision(store: &Store, revision: u64) -> Result<(), ApiError> {
    if store.revision != revision {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Profiles changed in another browser. Reload profiles before saving.",
        ));
    }
    Ok(())
}

pub(crate) async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Store>, ApiError> {
    blocking(move || transaction(&state, |_| Ok(false))).await
}

pub(crate) async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Store>, ApiError> {
    require_write_header(&headers)?;
    save(state, None, input).await
}

pub(crate) async fn update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Store>, ApiError> {
    require_write_header(&headers)?;
    save(state, Some(id), input).await
}

async fn save(
    state: Arc<AppState>,
    id: Option<String>,
    input: Input,
) -> Result<Json<Store>, ApiError> {
    blocking(move || {
        transaction(&state, |store| {
            check_revision(store, input.revision)?;
            let profile = Profile {
                id: id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: input.name,
                client_path: input.client_path,
                download_folder: input.download_folder,
            };
            validate_profile(&profile)
                .map_err(|message| ApiError::new(StatusCode::BAD_REQUEST, message))?;
            if store
                .profiles
                .iter()
                .any(|p| p.id != profile.id && p.name.to_lowercase() == profile.name.to_lowercase())
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "A profile with this name already exists",
                ));
            }
            if let Some(id) = id {
                let existing = store
                    .profiles
                    .iter_mut()
                    .find(|p| p.id == id)
                    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Profile not found"))?;
                *existing = profile;
            } else {
                if store.profiles.len() >= MAX_PROFILES {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "Maximum of 99 profiles reached. Edit or delete a profile first.",
                    ));
                }
                store.profiles.push(profile);
            }
            Ok(true)
        })
    })
    .await
}

pub(crate) async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<Revision>,
    headers: HeaderMap,
) -> Result<Json<Store>, ApiError> {
    require_write_header(&headers)?;
    blocking(move || {
        transaction(&state, |store| {
            check_revision(store, query.revision)?;
            let index = store
                .profiles
                .iter()
                .position(|profile| profile.id == id)
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Profile not found"))?;
            store.profiles.remove(index);
            Ok(true)
        })
    })
    .await
}
