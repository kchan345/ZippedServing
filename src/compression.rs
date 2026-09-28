use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{Arc, mpsc},
};

use lz4::block::{CompressionMode, compress};
use rayon::ThreadPool;
use xxhash_rust::xxh32::xxh32;

pub const BLOCK_SIZE: usize = 4 * 1024 * 1024;
type BlockResult = io::Result<(Vec<u8>, bool)>;

/// LZ4 frame v1: independent 4 MiB blocks, block checksums, unknown content size.
/// Only the framing is implemented here; liblz4 performs the actual compression.
pub struct ParallelLz4<W> {
    output: W,
    pool: Arc<ThreadPool>,
    pending: VecDeque<mpsc::Receiver<BlockResult>>,
    input: Vec<u8>,
    window: usize,
}

impl<W: Write> ParallelLz4<W> {
    pub fn new(mut output: W, pool: Arc<ThreadPool>) -> io::Result<Self> {
        let descriptor = [0x70, 0x70];
        output.write_all(&[0x04, 0x22, 0x4d, 0x18])?;
        output.write_all(&descriptor)?;
        output.write_all(&[((xxh32(&descriptor, 0) >> 8) & 0xff) as u8])?;
        let window = pool.current_num_threads();
        Ok(Self {
            output,
            pool,
            pending: VecDeque::new(),
            input: Vec::with_capacity(BLOCK_SIZE),
            window,
        })
    }

    fn submit(&mut self) -> io::Result<()> {
        if self.input.is_empty() {
            return Ok(());
        }
        if self.pending.len() >= self.window {
            self.emit_next()?;
        }
        let input = std::mem::replace(&mut self.input, Vec::with_capacity(BLOCK_SIZE));
        let (send, receive) = mpsc::sync_channel(1);
        self.pool.spawn(move || {
            let result =
                compress(&input, Some(CompressionMode::FAST(1)), false).map(|compressed| {
                    if compressed.len() < input.len() {
                        (compressed, false)
                    } else {
                        (input, true)
                    }
                });
            // A disconnected receiver means the HTTP download was cancelled.
            let _ = send.send(result);
        });
        self.pending.push_back(receive);
        Ok(())
    }

    fn emit_next(&mut self) -> io::Result<()> {
        if let Some(receiver) = self.pending.pop_front() {
            let (block, uncompressed) = receiver
                .recv()
                .map_err(|_| io::Error::other("Compression worker disconnected"))??;
            let size = block.len() as u32 | if uncompressed { 0x8000_0000 } else { 0 };
            self.output.write_all(&size.to_le_bytes())?;
            self.output.write_all(&block)?;
            self.output.write_all(&xxh32(&block, 0).to_le_bytes())?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.flush()?;
        self.output.write_all(&0_u32.to_le_bytes())?;
        self.output.flush()?;
        Ok(self.output)
    }
}

impl<W: Write> Write for ParallelLz4<W> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let total = bytes.len();
        while !bytes.is_empty() {
            let take = bytes.len().min(BLOCK_SIZE - self.input.len());
            self.input.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.input.len() == BLOCK_SIZE {
                self.submit()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.submit()?;
        while !self.pending.is_empty() {
            self.emit_next()?;
        }
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn roundtrip(input: &[u8], threads: usize) -> Vec<u8> {
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap(),
        );
        let mut encoder = ParallelLz4::new(Vec::new(), pool).unwrap();
        for part in input.chunks(71_123) {
            encoder.write_all(part).unwrap();
        }
        let encoded = encoder.finish().unwrap();
        let mut decoder = lz4::Decoder::new(encoded.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        decoder.finish().1.unwrap();
        assert_eq!(input, decoded);
        encoded
    }

    #[test]
    fn empty_small_and_multiblock_frames_decode_with_liblz4() {
        roundtrip(&[], 1);
        roundtrip(b"hello, LZ4", 2);
        let input = vec![b'A'; BLOCK_SIZE * 3 + 17];
        assert!(roundtrip(&input, 3).len() < input.len() / 100);
    }

    #[test]
    fn incompressible_blocks_and_order_are_preserved() {
        let mut seed = 123456789_u32;
        let input: Vec<u8> = (0..BLOCK_SIZE * 2 + 89)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let encoded = roundtrip(&input, 2);
        assert_ne!(
            u32::from_le_bytes(encoded[7..11].try_into().unwrap()) & 0x8000_0000,
            0
        );
    }

    #[test]
    fn downstream_errors_propagate() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "disconnected"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .unwrap(),
        );
        assert!(ParallelLz4::new(Failing, pool).is_err());
    }
}
