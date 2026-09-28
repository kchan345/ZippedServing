use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::Duration,
};

use reqwest::{
    Url,
    blocking::{Client, Response},
};
use serde::de::DeserializeOwned;

use crate::{
    paths,
    transfer::{self, ChunkAck, Codec, Manifest, UploadInfo, UploadRequest},
};

pub struct Options {
    pub chunk_size: u64,
    pub codec: Codec,
    pub max_bytes: u64,
    pub timeout: Duration,
    pub compression_threads: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            chunk_size: transfer::DEFAULT_SPLIT,
            codec: Codec::Lz4,
            max_bytes: 100 * 1024 * 1024 * 1024,
            timeout: Duration::from_secs(3600),
            compression_threads: 2,
        }
    }
}

fn http(options: &Options) -> io::Result<Client> {
    transfer::validate_split(options.chunk_size)?;
    if options.max_bytes == 0
        || options.compression_threads == 0
        || options.compression_threads > 64
    {
        return Err(transfer::invalid(
            "Limits must be positive; compression threads must be 1-64",
        ));
    }
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(options.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)
}

fn url(value: &str) -> io::Result<Url> {
    let url = Url::parse(value).map_err(io::Error::other)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(transfer::invalid(
            "Use an HTTP(S) URL without embedded credentials or fragment",
        ));
    }
    Ok(url)
}

fn endpoint(base: &Url, suffix: &str) -> io::Result<Url> {
    let path = base.path();
    let prefix = path
        .strip_suffix("/api/download")
        .or_else(|| path.strip_suffix("/api/upload"))
        .ok_or_else(|| {
            transfer::invalid("Chunk transfers require a server /api/download or /api/upload URL")
        })?;
    let mut result = base.clone();
    result.set_path(&format!("{prefix}/api/{suffix}"));
    result.set_query(None);
    Ok(result)
}

fn remote_path(url: &Url) -> String {
    url.query_pairs()
        .find(|(name, _)| name == "path")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

fn checked(response: Result<Response, reqwest::Error>) -> io::Result<Response> {
    let response = response.map_err(io::Error::other)?;
    if !response.status().is_success() {
        let status = response.status();
        let mut detail = String::new();
        response.take(8192).read_to_string(&mut detail)?;
        return Err(io::Error::other(format!("HTTP {status}: {detail}")));
    }
    Ok(response)
}

fn json<T: DeserializeOwned>(response: Response) -> io::Result<T> {
    serde_json::from_reader(response.take(transfer::MAX_MANIFEST)).map_err(io::Error::other)
}

fn safe_relative(value: &str) -> io::Result<PathBuf> {
    if value.is_empty() {
        return Err(transfer::invalid("Empty output path"));
    }
    let parts = paths::components(value).map_err(|_| transfer::invalid("Unsafe output path"))?;
    Ok(parts.iter().collect())
}

pub fn download(
    target_url: &str,
    destination: &Path,
    archive: bool,
    options: &Options,
) -> io::Result<()> {
    let client = http(options)?;
    let target_url = url(target_url)?;
    if destination.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Output directory already exists; choose a new path",
        ));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stage = tempfile::Builder::new()
        .prefix(".zfs-client-")
        .tempdir_in(parent)?;
    if archive || !target_url.path().ends_with("/api/download") {
        let response = checked(client.get(target_url).send())?;
        extract_archive(response, stage.path(), options.max_bytes)?;
    } else {
        download_chunks(&client, &target_url, stage.path(), options)?;
    }
    // The stage is unpublished until all frames, hashes, and files have verified.
    if destination.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Output path appeared during download",
        ));
    }
    fs::rename(stage.path(), destination)?;
    Ok(())
}

fn download_chunks(
    client: &Client,
    base: &Url,
    destination: &Path,
    options: &Options,
) -> io::Result<()> {
    let manifest: Manifest = json(checked(
        client
            .get(endpoint(base, "transfer/manifest")?)
            .query(&[
                ("path", remote_path(base)),
                ("chunk_size", options.chunk_size.to_string()),
                ("codec", options.codec.name().to_owned()),
            ])
            .send(),
    )?)?;
    transfer::validate_split(manifest.chunk_size)?;
    if manifest.protocol != transfer::PROTOCOL
        || manifest.hash != transfer::HASH
        || manifest.codec != options.codec
        || manifest.chunk_size > options.chunk_size
        || manifest.entries.len() > transfer::MAX_ENTRIES
        || manifest.entries.is_empty()
    {
        return Err(transfer::invalid(
            "Invalid or unsupported transfer negotiation",
        ));
    }
    eprintln!(
        "Negotiated {} MiB chunks, {}, {}.",
        manifest.chunk_size / (1024 * 1024),
        manifest.codec.name(),
        manifest.hash
    );
    let mut total = 0_u64;
    let mut seen = HashSet::new();
    for entry in &manifest.entries {
        safe_relative(&entry.path)?;
        if !seen.insert(entry.path.to_lowercase()) {
            return Err(transfer::invalid("Duplicate manifest path"));
        }
        total = total
            .checked_add(entry.size)
            .ok_or_else(|| transfer::invalid("Manifest size overflow"))?;
        if total > options.max_bytes || (entry.directory && entry.size != 0) {
            return Err(transfer::invalid(
                "Manifest exceeds output limit or has invalid directory size",
            ));
        }
    }
    for entry in manifest.entries {
        let output = destination.join(safe_relative(&entry.path)?);
        if entry.directory {
            fs::create_dir_all(output)?;
            continue;
        }
        fs::create_dir_all(
            output
                .parent()
                .ok_or_else(|| transfer::invalid("Missing output parent"))?,
        )?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)?;
        let mut offset = 0;
        while offset < entry.size {
            let length = manifest.chunk_size.min(entry.size - offset);
            let mut last_error = None;
            for attempt in 0..3 {
                let result = (|| {
                    file.set_len(offset)?;
                    file.seek(SeekFrom::Start(offset))?;
                    let response = checked(
                        client
                            .get(endpoint(base, "transfer/chunk")?)
                            .query(&[
                                ("path", entry.source.clone()),
                                ("stamp", entry.stamp.clone()),
                                ("offset", offset.to_string()),
                                ("chunk_size", manifest.chunk_size.to_string()),
                                ("codec", manifest.codec.name().to_owned()),
                            ])
                            .send(),
                    )?;
                    transfer::decode_chunk(response, &mut file, manifest.codec, offset, length)
                })();
                match result {
                    Ok(_) => {
                        last_error = None;
                        break;
                    }
                    Err(error) => {
                        eprintln!(
                            "Chunk {} at {offset}, attempt {} failed: {error}",
                            entry.path,
                            attempt + 1
                        );
                        last_error = Some(error);
                    }
                }
            }
            if let Some(error) = last_error {
                return Err(error);
            }
            offset += length;
        }
        file.sync_all()?;
        eprintln!("Verified {} ({} bytes).", entry.path, entry.size);
    }
    Ok(())
}

struct Budget<R> {
    input: R,
    remaining: u64,
}

impl<R: Read> Read for Budget<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            if self.input.read(&mut [0])? == 0 {
                return Ok(0);
            }
            return Err(transfer::invalid("Archive exceeds decompressed byte limit"));
        }
        let length = buffer
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let count = self.input.read(&mut buffer[..length])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

pub fn extract_archive(mut input: impl Read, destination: &Path, max_bytes: u64) -> io::Result<()> {
    let mut magic = [0; 4];
    input.read_exact(&mut magic)?;
    let input =
        BufReader::with_capacity(transfer::BUFFER_SIZE, io::Cursor::new(magic).chain(input));
    if magic.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        let mut decoder = lz4::Decoder::new(input)?;
        extract_tar(&mut decoder, destination, max_bytes)?;
        let (mut input, result) = decoder.finish();
        result?;
        if input.read(&mut [0])? != 0 {
            return Err(transfer::invalid("Trailing LZ4 data"));
        }
    } else if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let mut decoder = zstd::stream::read::Decoder::with_buffer(input)?.single_frame();
        decoder.window_log_max(23)?;
        extract_tar(&mut decoder, destination, max_bytes)?;
        if decoder.finish().read(&mut [0])? != 0 {
            return Err(transfer::invalid("Trailing zstd data"));
        }
    } else {
        return Err(transfer::invalid(
            "Expected an LZ4 or zstd frame containing tar",
        ));
    }
    Ok(())
}

fn extract_tar(input: impl Read, destination: &Path, max_bytes: u64) -> io::Result<()> {
    let reader = TarGuard {
        input: Budget {
            input,
            remaining: max_bytes,
        },
        header: [0; 512],
        position: 512,
        remaining: 0,
        headers: 0,
    };
    let mut archive = tar::Archive::new(reader);
    let mut count = 0;
    for entry in archive.entries()? {
        count += 1;
        if count > transfer::MAX_ENTRIES {
            return Err(transfer::invalid("Too many archive entries"));
        }
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(transfer::invalid(
                "Archive links and special files are not allowed",
            ));
        }
        let name = entry.path_bytes();
        let name = std::str::from_utf8(&name).map_err(io::Error::other)?;
        let relative = safe_relative(if kind.is_dir() {
            name.trim_end_matches('/')
        } else {
            name
        })?;
        let output = destination.join(relative);
        if kind.is_dir() {
            fs::create_dir_all(output)?;
        } else {
            fs::create_dir_all(
                output
                    .parent()
                    .ok_or_else(|| transfer::invalid("Missing file parent"))?,
            )?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            io::copy(&mut entry, &mut file)?;
            file.sync_all()?;
        }
    }
    // Tar can stop before the compression footer. Always finish decoding and validate it.
    let mut reader = archive.into_inner();
    let mut buffer = vec![0; transfer::BUFFER_SIZE];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if buffer[..count].iter().any(|byte| *byte != 0) {
            return Err(transfer::invalid("Nonzero data after end of tar archive"));
        }
    }
    Ok(())
}

struct TarGuard<R> {
    input: R,
    header: [u8; 512],
    position: usize,
    remaining: u64,
    headers: usize,
}

impl<R: Read> Read for TarGuard<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.position == 512 && self.remaining > 0 {
            let length = buffer
                .len()
                .min(self.remaining.min(usize::MAX as u64) as usize);
            let count = self.input.read(&mut buffer[..length])?;
            if count == 0 {
                return Err(transfer::invalid("Truncated tar entry"));
            }
            self.remaining -= count as u64;
            return Ok(count);
        }
        if self.position == 512 {
            if self.input.read(&mut self.header[..1])? == 0 {
                return Ok(0);
            }
            self.input.read_exact(&mut self.header[1..])?;
            self.position = 0;
            if self.header.iter().any(|byte| *byte != 0) {
                self.headers += 1;
                if self.headers > transfer::MAX_ENTRIES {
                    return Err(transfer::invalid("Too many tar headers"));
                }
                let header = tar::Header::from_byte_slice(&self.header);
                let size = header.size()?;
                // Tar buffers extension records internally; cap those, not regular file data.
                if matches!(header.entry_type().as_byte(), b'L' | b'K' | b'x' | b'g')
                    && size > 1024 * 1024
                {
                    return Err(transfer::invalid("Tar metadata record exceeds 1 MiB"));
                }
                self.remaining = size
                    .checked_add(511)
                    .ok_or_else(|| transfer::invalid("Tar size overflow"))?
                    / 512
                    * 512;
            }
        }
        let count = buffer.len().min(512 - self.position);
        buffer[..count].copy_from_slice(&self.header[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

struct UploadWriter(mpsc::SyncSender<io::Result<Vec<u8>>>);

impl Write for UploadWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        for part in data.chunks(transfer::BUFFER_SIZE) {
            self.0
                .send(Ok(part.to_vec()))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Upload cancelled"))?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct UploadReader {
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    current: io::Cursor<Vec<u8>>,
}

impl Read for UploadReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.current.read(buffer)?;
            if count != 0 {
                return Ok(count);
            }
            match self.receiver.recv() {
                Ok(chunk) => self.current = io::Cursor::new(chunk?),
                Err(_) => return Ok(0),
            }
        }
    }
}

pub fn upload(local: &Path, target_url: &str, options: &Options) -> io::Result<()> {
    let client = http(options)?;
    let base = url(target_url)?;
    endpoint(&base, "transfer/uploads")?;
    let destination = remote_path(&base);
    safe_relative(&destination)?;
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(options.compression_threads)
            .build()
            .map_err(io::Error::other)?,
    );
    let mut pending = vec![(local.to_path_buf(), destination)];
    let mut total = 0_u64;
    let mut count = 0;
    while let Some((path, remote)) = pending.pop() {
        count += 1;
        if count + pending.len() > transfer::MAX_ENTRIES {
            return Err(transfer::invalid("Too many upload entries"));
        }
        let metadata = fs::symlink_metadata(&path)?;
        if paths::is_link(&metadata) {
            return Err(transfer::invalid(
                "Uploading links/reparse points is not allowed",
            ));
        }
        if metadata.is_dir() {
            checked(
                client
                    .post(endpoint(&base, "mkdir")?)
                    .query(&[("path", &remote)])
                    .header("X-Requested-With", "zipped-file-serving")
                    .send(),
            )?;
            for item in fs::read_dir(path)? {
                let item = item?;
                let name = item
                    .file_name()
                    .into_string()
                    .map_err(|_| transfer::invalid("Non-Unicode filename"))?;
                safe_relative(&name)?;
                pending.push((item.path(), format!("{remote}/{name}")));
                if count + pending.len() > transfer::MAX_ENTRIES {
                    return Err(transfer::invalid("Too many upload entries"));
                }
            }
        } else if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or_else(|| transfer::invalid("Upload size overflow"))?;
            if total > options.max_bytes {
                return Err(transfer::invalid("Upload exceeds byte limit"));
            }
            upload_file(&client, &base, &path, &remote, options, pool.clone())?;
        } else {
            return Err(transfer::invalid(
                "Only regular files and directories can be uploaded",
            ));
        }
    }
    Ok(())
}

fn upload_file(
    client: &Client,
    base: &Url,
    local: &Path,
    remote: &str,
    options: &Options,
    pool: Arc<rayon::ThreadPool>,
) -> io::Result<()> {
    let original = fs::metadata(local)?;
    let size = original.len();
    let info: UploadInfo = json(checked(
        client
            .post(endpoint(base, "transfer/uploads")?)
            .header("X-Requested-With", "zipped-file-serving")
            .json(&UploadRequest {
                path: remote.into(),
                size,
                chunk_size: options.chunk_size,
                codec: options.codec,
            })
            .send(),
    )?)?;
    if uuid::Uuid::parse_str(&info.id).is_err() {
        return Err(transfer::invalid("Invalid upload session ID"));
    }
    let session_url = endpoint(base, &format!("transfer/uploads/{}", info.id))?;
    let result = (|| {
        transfer::validate_split(info.chunk_size)?;
        if info.protocol != transfer::PROTOCOL
            || info.hash != transfer::HASH
            || info.chunk_size > options.chunk_size
            || info.codec != options.codec
            || info.offset != 0
        {
            return Err(transfer::invalid("Invalid upload negotiation"));
        }
        eprintln!(
            "Uploading {remote}: {} MiB chunks, {}, {}.",
            info.chunk_size / (1024 * 1024),
            info.codec.name(),
            info.hash
        );
        let mut offset = 0;
        while offset < size {
            let length = info.chunk_size.min(size - offset);
            let mut file = File::open(local)?;
            file.seek(SeekFrom::Start(offset))?;
            let (send, receive) = mpsc::sync_channel(2);
            let pool = pool.clone();
            let codec = info.codec;
            let worker = std::thread::spawn(move || {
                let result = transfer::encode_chunk(
                    file,
                    UploadWriter(send.clone()),
                    codec,
                    offset,
                    length,
                    pool,
                );
                if let Err(error) = &result {
                    let _ = send.send(Err(io::Error::new(error.kind(), error.to_string())));
                }
                result
            });
            let response = checked(
                client
                    .put(session_url.clone())
                    .query(&[("offset", offset)])
                    .header("X-Requested-With", "zipped-file-serving")
                    .body(reqwest::blocking::Body::new(UploadReader {
                        receiver: receive,
                        current: io::Cursor::new(Vec::new()),
                    }))
                    .send(),
            );
            let encoded_hash = worker
                .join()
                .map_err(|_| io::Error::other("Compression worker panicked"))??;
            let ack: ChunkAck = json(response?)?;
            if ack.next_offset != offset + length || ack.hash != encoded_hash {
                return Err(transfer::invalid(
                    "Server chunk acknowledgement failed integrity verification",
                ));
            }
            offset += length;
        }
        let after = fs::metadata(local)?;
        if after.len() != size || after.modified()? != original.modified()? {
            return Err(transfer::invalid("Source changed during upload"));
        }
        checked(
            client
                .post(endpoint(
                    base,
                    &format!("transfer/uploads/{}/complete", info.id),
                )?)
                .header("X-Requested-With", "zipped-file-serving")
                .send(),
        )?;
        eprintln!("Verified and published {remote} ({size} bytes).");
        Ok(())
    })();
    if result.is_err() {
        if let Err(error) = checked(
            client
                .delete(session_url)
                .header("X-Requested-With", "zipped-file-serving")
                .send(),
        ) {
            eprintln!(
                "Could not cancel upload session {}: {error}; inactive sessions expire after 15 minutes.",
                info.id
            );
        }
    }
    result
}
