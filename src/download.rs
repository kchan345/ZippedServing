use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures_util::Stream;
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::{ApiError, AppState, blocking, compression::ParallelLz4, paths};

const CHUNK_SIZE: usize = 256 * 1024;
const CHANNEL_DEPTH: usize = 8;

#[derive(Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum Format {
    #[default]
    Lz4,
    Raw,
}

#[derive(Deserialize)]
pub(crate) struct DownloadQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    format: Format,
}

struct ChannelWriter {
    sender: mpsc::Sender<io::Result<Bytes>>,
    buffer: Vec<u8>,
}

impl ChannelWriter {
    fn send(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::replace(&mut self.buffer, Vec::with_capacity(CHUNK_SIZE));
        self.sender
            .blocking_send(Ok(Bytes::from(bytes)))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Download cancelled"))
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        if self.sender.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Download cancelled",
            ));
        }
        let total = bytes.len();
        while !bytes.is_empty() {
            let count = bytes.len().min(CHUNK_SIZE - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if self.buffer.len() == CHUNK_SIZE {
                self.send()?;
            }
        }
        Ok(total)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

enum Source {
    File(File),
    Directory(PathBuf),
}

struct DownloadStream {
    receiver: ReceiverStream<io::Result<Bytes>>,
    // Hold the slot until both the HTTP body and producer have finished.
    _permit: Arc<OwnedSemaphorePermit>,
}

impl Stream for DownloadStream {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }
}

pub(crate) async fn download(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    let permit = Arc::new(
        state
            .downloads
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::busy())?,
    );
    let setup_state = state.clone();
    let (source, name) = blocking(move || {
        let path = paths::resolve(&setup_state.config.root, &query.path)?;
        let metadata = fs::symlink_metadata(&path)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("download")
            .to_owned();
        let source = if metadata.is_dir() {
            if matches!(query.format, Format::Raw) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Directory downloads require LZ4 compression",
                ));
            }
            Source::Directory(path)
        } else if metadata.is_file() {
            Source::File(File::open(path)?)
        } else {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Only regular files and directories can be downloaded",
            ));
        };
        Ok((source, name))
    })
    .await?;
    let suffix = match (&source, &query.format) {
        (Source::Directory(_), _) => ".tar.lz4",
        (_, Format::Lz4) => ".lz4",
        (_, Format::Raw) => "",
    };
    let filename = format!("{name}{suffix}");
    let disposition = content_disposition(&filename);
    let (send, receive) = mpsc::channel(CHANNEL_DEPTH);
    let body_permit = permit.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let output = ChannelWriter {
            sender: send.clone(),
            buffer: Vec::with_capacity(CHUNK_SIZE),
        };
        let result = produce(
            source,
            &name,
            query.format,
            output,
            state.pool.clone(),
            &send,
        );
        if let Err(error) = result {
            if send.is_closed() {
                tracing::info!("Client cancelled download");
            } else {
                tracing::error!(%error, "Download failed after response started");
                let _ = send.blocking_send(Err(error));
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, disposition)
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(DownloadStream {
            receiver: ReceiverStream::new(receive),
            _permit: body_permit,
        }))
        .map_err(|error| {
            tracing::error!(%error, "Failed to build download response");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build download response",
            )
        })
}

fn produce(
    source: Source,
    name: &str,
    format: Format,
    mut output: ChannelWriter,
    pool: Arc<rayon::ThreadPool>,
    sender: &mpsc::Sender<io::Result<Bytes>>,
) -> io::Result<()> {
    if matches!(format, Format::Raw) {
        if let Source::File(mut file) = source {
            copy(&mut file, &mut output)?;
        }
        return output.flush();
    }
    let mut encoder = ParallelLz4::new(output, pool)?;
    match source {
        Source::File(mut file) => {
            copy(&mut file, &mut encoder)?;
        }
        Source::Directory(path) => {
            let mut archive = tar::Builder::new(encoder);
            archive.follow_symlinks(false);
            append_tree(&mut archive, &path, name, sender)?;
            archive.finish()?;
            encoder = archive.into_inner()?;
        }
    }
    encoder.finish()?;
    Ok(())
}

fn copy(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<u64> {
    let mut buffer = vec![0; 1024 * 1024];
    let mut total = 0;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(total);
        }
        writer.write_all(&buffer[..count])?;
        total += count as u64;
    }
}

fn append_tree<W: Write>(
    archive: &mut tar::Builder<W>,
    root: &Path,
    name: &str,
    sender: &mpsc::Sender<io::Result<Bytes>>,
) -> io::Result<()> {
    archive.append_dir(name, root)?;
    // Keep one ReadDir per depth, not a list of every path in a large tree.
    let mut stack = vec![fs::read_dir(root)?];
    while let Some(directory) = stack.last_mut() {
        if sender.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Download cancelled",
            ));
        }
        let Some(entry) = directory.next() else {
            stack.pop();
            continue;
        };
        let entry = entry?;
        let entry_name = entry.file_name();
        let entry_name = entry_name.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Filename is not valid Unicode")
        })?;
        if entry_name
            .to_ascii_lowercase()
            .starts_with(paths::UPLOAD_PREFIX)
        {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if paths::is_link(&metadata) || (!metadata.is_file() && !metadata.is_dir()) {
            tracing::debug!(path = %entry.path().display(), "Skipping link, reparse point, or special file");
            continue;
        }
        paths::components(entry_name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Archive contains an unsupported filename",
            )
        })?;
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(io::Error::other)?;
        let archived_path = Path::new(name).join(relative);
        if metadata.is_dir() {
            archive.append_dir(archived_path, &path)?;
            stack.push(fs::read_dir(&path)?);
        } else {
            archive.append_file(archived_path, &mut File::open(path)?)?;
        }
    }
    Ok(())
}

fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .as_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"._-".contains(b) {
                (*b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filename_header_is_ascii_and_escaped() {
        let value = content_disposition("report \"x\"\r\n.txt");
        assert!(!value.contains('\r'));
        assert!(!value.contains('\n'));
        assert!(value.contains("%22x%22%0D%0A"));
    }
}
