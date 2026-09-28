use std::{
    io::{self, BufReader, Read, Write},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::Xxh3;

use crate::compression::ParallelLz4;

pub const DEFAULT_SPLIT: u64 = 256 * 1024 * 1024;
pub const MIN_SPLIT: u64 = 1024 * 1024;
pub const MAX_SPLIT: u64 = 1024 * 1024 * 1024;
pub const BUFFER_SIZE: usize = 256 * 1024;
pub const PROTOCOL: &str = "zfs-chunks-v1";
pub const HASH: &str = "xxh3-128";
pub const MAX_ENTRIES: usize = 100_000;
pub const MAX_MANIFEST: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    #[default]
    Lz4,
    Zstd,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Self::Lz4 => "lz4",
            Self::Zstd => "zstd",
        }
    }
}

pub fn validate_split(size: u64) -> io::Result<()> {
    if !(MIN_SPLIT..=MAX_SPLIT).contains(&size) {
        return Err(invalid("Split size must be between 1 MiB and 1 GiB"));
    }
    Ok(())
}

pub fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub enum Encoder<W: Write> {
    Lz4(ParallelLz4<W>),
    Zstd(zstd::stream::write::Encoder<'static, W>),
}

impl<W: Write> Encoder<W> {
    pub fn new(output: W, codec: Codec, pool: Arc<rayon::ThreadPool>) -> io::Result<Self> {
        match codec {
            Codec::Lz4 => Ok(Self::Lz4(ParallelLz4::new(output, pool)?)),
            Codec::Zstd => {
                let mut encoder = zstd::stream::write::Encoder::new(output, 1)?;
                encoder.window_log(23)?;
                encoder.include_checksum(true)?;
                Ok(Self::Zstd(encoder))
            }
        }
    }

    pub fn finish(self) -> io::Result<W> {
        let mut output = match self {
            Self::Lz4(encoder) => encoder.finish(),
            Self::Zstd(encoder) => encoder.finish(),
        }?;
        output.flush()?;
        Ok(output)
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Lz4(encoder) => encoder.write(bytes),
            Self::Zstd(encoder) => encoder.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Lz4(encoder) => encoder.flush(),
            Self::Zstd(encoder) => encoder.flush(),
        }
    }
}

struct Records<W>(W);

impl<W: Write> Write for Records<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for record in bytes.chunks(BUFFER_SIZE) {
            self.0.write_all(&(record.len() as u32).to_le_bytes())?;
            self.0.write_all(record)?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

struct RecordReader<R> {
    input: R,
    remaining: usize,
    budget: u64,
    ended: bool,
}

impl<R: Read> Read for RecordReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() || self.ended {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut length = [0; 4];
            self.input.read_exact(&mut length)?;
            self.remaining = u32::from_le_bytes(length) as usize;
            if self.remaining == 0 {
                self.ended = true;
                return Ok(0);
            }
            if self.remaining > BUFFER_SIZE || self.remaining as u64 > self.budget {
                return Err(invalid("Oversized chunk record or compressed payload"));
            }
            self.budget -= self.remaining as u64;
        }
        let count = bytes.len().min(self.remaining);
        self.input.read_exact(&mut bytes[..count])?;
        self.remaining -= count;
        Ok(count)
    }
}

pub fn encode_chunk(
    mut input: impl Read,
    mut output: impl Write,
    codec: Codec,
    offset: u64,
    length: u64,
    pool: Arc<rayon::ThreadPool>,
) -> io::Result<String> {
    if length > MAX_SPLIT {
        return Err(invalid("Chunk exceeds protocol maximum"));
    }
    output.write_all(b"ZFC1")?;
    output.write_all(&[match codec {
        Codec::Lz4 => 1,
        Codec::Zstd => 2,
    }])?;
    output.write_all(&offset.to_le_bytes())?;
    output.write_all(&length.to_le_bytes())?;
    let mut encoder = Encoder::new(Records(&mut output), codec, pool)?;
    let mut hash = Xxh3::new();
    let mut remaining = length;
    let mut buffer = vec![0; BUFFER_SIZE];
    while remaining > 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..count])?;
        hash.update(&buffer[..count]);
        encoder.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    encoder.finish()?;
    output.write_all(&0_u32.to_le_bytes())?;
    let digest = hash.digest128();
    output.write_all(&digest.to_be_bytes())?;
    output.flush()?;
    Ok(format!("{digest:032x}"))
}

pub fn decode_chunk(
    mut input: impl Read,
    mut output: impl Write,
    codec: Codec,
    offset: u64,
    length: u64,
) -> io::Result<String> {
    let mut header = [0; 21];
    input.read_exact(&mut header)?;
    let codec_id = match codec {
        Codec::Lz4 => 1,
        Codec::Zstd => 2,
    };
    if &header[..4] != b"ZFC1"
        || header[4] != codec_id
        || u64::from_le_bytes(header[5..13].try_into().unwrap()) != offset
        || u64::from_le_bytes(header[13..21].try_into().unwrap()) != length
        || length > MAX_SPLIT
    {
        return Err(invalid(
            "Chunk header differs from negotiated codec, offset, or length",
        ));
    }
    let mut records = RecordReader {
        input,
        remaining: 0,
        budget: length + length / 8 + 1024 * 1024,
        ended: false,
    };
    let digest = match codec {
        Codec::Lz4 => {
            let mut decoder = lz4::Decoder::new(&mut records)?;
            let hash = copy_verified(&mut decoder, &mut output, length)?;
            decoder.finish().1?;
            hash
        }
        Codec::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::with_buffer(BufReader::with_capacity(
                BUFFER_SIZE,
                &mut records,
            ))?
            .single_frame();
            decoder.window_log_max(23)?;
            let hash = copy_verified(&mut decoder, &mut output, length)?;
            if !decoder.finish().buffer().is_empty() {
                return Err(invalid("Trailing data inside compressed chunk"));
            }
            hash
        }
    };
    if records.read(&mut [0])? != 0 {
        return Err(invalid("Trailing compressed chunk data"));
    }
    let mut expected = [0; 16];
    records.input.read_exact(&mut expected)?;
    if digest != u128::from_be_bytes(expected) {
        return Err(invalid("XXH3-128 integrity mismatch"));
    }
    if records.input.read(&mut [0])? != 0 {
        return Err(invalid("Trailing bytes after chunk integrity footer"));
    }
    Ok(format!("{digest:032x}"))
}

fn copy_verified(input: &mut impl Read, output: &mut impl Write, length: u64) -> io::Result<u128> {
    let mut buffer = vec![0; BUFFER_SIZE];
    let mut hash = Xxh3::new();
    let mut received = 0_u64;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received += count as u64;
        if received > length {
            return Err(invalid("Decoded chunk exceeds negotiated length"));
        }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    if received != length {
        return Err(invalid("Decoded chunk is incomplete"));
    }
    Ok(hash.digest128())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub protocol: String,
    pub hash: String,
    pub chunk_size: u64,
    pub codec: Codec,
    pub entries: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ManifestEntry {
    pub path: String,
    pub source: String,
    pub directory: bool,
    pub size: u64,
    pub stamp: String,
}

#[derive(Deserialize, Serialize)]
pub struct UploadRequest {
    pub path: String,
    pub size: u64,
    pub chunk_size: u64,
    pub codec: Codec,
}

#[derive(Deserialize, Serialize)]
pub struct UploadInfo {
    pub id: String,
    pub protocol: String,
    pub hash: String,
    pub chunk_size: u64,
    pub codec: Codec,
    pub offset: u64,
}

#[derive(Deserialize, Serialize)]
pub struct ChunkAck {
    pub next_offset: u64,
    pub hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codecs_verify_boundaries_corruption_and_truncation() {
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(2)
                .build()
                .unwrap(),
        );
        for codec in [Codec::Lz4, Codec::Zstd] {
            for length in [0, 1, BUFFER_SIZE - 1, BUFFER_SIZE + 1, 4 * 1024 * 1024 + 17] {
                let data: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
                let mut encoded = Vec::new();
                let hash = encode_chunk(
                    data.as_slice(),
                    &mut encoded,
                    codec,
                    77,
                    length as u64,
                    pool.clone(),
                )
                .unwrap();
                let mut decoded = Vec::new();
                assert_eq!(
                    decode_chunk(encoded.as_slice(), &mut decoded, codec, 77, length as u64)
                        .unwrap(),
                    hash
                );
                assert_eq!(decoded, data);
                assert!(
                    decode_chunk(encoded.as_slice(), io::sink(), codec, 78, length as u64).is_err()
                );
                let mut corrupt = encoded.clone();
                *corrupt.last_mut().unwrap() ^= 1;
                assert!(
                    decode_chunk(corrupt.as_slice(), io::sink(), codec, 77, length as u64).is_err()
                );
                assert!(
                    decode_chunk(
                        &encoded[..encoded.len() - 1],
                        io::sink(),
                        codec,
                        77,
                        length as u64
                    )
                    .is_err()
                );
                encoded.push(0);
                assert!(
                    decode_chunk(encoded.as_slice(), io::sink(), codec, 77, length as u64).is_err()
                );
            }
        }
    }
}
