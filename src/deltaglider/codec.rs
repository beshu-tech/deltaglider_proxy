//! xdelta3 codec wrapper for delta encoding/decoding
//!
//! Uses the xdelta3 CLI binary for both encoding and decoding to ensure
//! compatibility with deltas created by the original DeltaGlider Python CLI.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;
use thiserror::Error;
use tracing::{debug, instrument, warn};

/// Errors that can occur during delta encoding/decoding
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("Delta encoding failed: {0}")]
    EncodeFailed(String),

    #[error("Delta decoding failed: {0}")]
    DecodeFailed(String),

    #[error("Data too large: {size} bytes (max: {max} bytes)")]
    TooLarge { size: usize, max: usize },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Delta codec using the xdelta3 CLI binary
pub struct DeltaCodec {
    max_size: usize,
    /// Whether the xdelta3 CLI binary is available.
    /// Probed once at construction time to avoid per-request discovery failures.
    cli_available: bool,
}

impl DeltaCodec {
    /// Create a new codec with size limit.
    /// Probes for the xdelta3 CLI binary once at construction.
    pub fn new(max_size: usize) -> Self {
        let cli_available = Self::probe_cli();
        Self {
            max_size,
            cli_available,
        }
    }

    /// Check if the xdelta3 CLI binary is available.
    fn probe_cli() -> bool {
        match Command::new("xdelta3").arg("-V").output() {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }

    /// Returns whether the xdelta3 CLI is available.
    pub fn is_cli_available(&self) -> bool {
        self.cli_available
    }

    /// Encode a delta between source (reference) and target (new file)
    /// Returns the delta patch that can transform source into target.
    ///
    /// PERF: This uses stdin/stdout piping instead of temp files for the target and
    /// delta data. Only the source remains as a temp file because xdelta3 needs
    /// random-access (mmap) to it. This reduces disk I/O from 3 temp files + 6 I/O
    /// ops to 1 temp file + 2 I/O ops per encode. Do NOT "simplify" by writing
    /// target to a temp file — that was the old slow path.
    #[instrument(skip(self, source, target))]
    pub fn encode(&self, source: &[u8], target: &[u8]) -> Result<Vec<u8>, CodecError> {
        // Validate sizes
        if source.len() > self.max_size {
            return Err(CodecError::TooLarge {
                size: source.len(),
                max: self.max_size,
            });
        }
        if target.len() > self.max_size {
            return Err(CodecError::TooLarge {
                size: target.len(),
                max: self.max_size,
            });
        }

        debug!(
            "Encoding delta: source={} bytes, target={} bytes",
            source.len(),
            target.len()
        );

        // PERF: Source MUST remain a temp file — xdelta3 needs random-access (mmap)
        // to the source for its sliding-window algorithm. Do NOT try to pipe it via
        // stdin; xdelta3 can only read source from a seekable file descriptor.
        let mut source_file = NamedTempFile::new()?;
        source_file.write_all(source)?;
        source_file.flush()?;

        // Run xdelta3 -e -s <source_file> -c
        // Target data piped to stdin, delta written to stdout (`-c` flag).
        let result = Command::new("xdelta3")
            .args(["-e", "-s", source_file.path().to_str().unwrap(), "-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        match result {
            Ok(mut child) => {
                let child_stdin = child.stdin.take().unwrap();
                let mut child_stdout = child.stdout.take().unwrap();

                // PERF: We MUST read stdout and write stdin concurrently using
                // thread::scope. If we write all data to stdin first THEN read
                // stdout, the OS pipe buffer (~64KB on Linux, ~16KB on macOS) will
                // fill up and xdelta3 will block on write(stdout) while we're still
                // blocked on write(stdin) → classic pipe deadlock. Do NOT "simplify"
                // this into sequential write-then-read.
                let (write_result, delta) = std::thread::scope(|s| {
                    let writer = s.spawn(|| {
                        let mut stdin = child_stdin;
                        stdin.write_all(target)?;
                        stdin.flush()?;
                        // CRITICAL: drop(stdin) closes the pipe so xdelta3 sees EOF
                        // and finishes processing. Without this, xdelta3 hangs
                        // forever waiting for more input.
                        drop(stdin);
                        Ok::<(), std::io::Error>(())
                    });

                    let reader = s.spawn(|| {
                        let mut buf = Vec::new();
                        child_stdout.read_to_end(&mut buf)?;
                        Ok::<Vec<u8>, std::io::Error>(buf)
                    });

                    (writer.join().unwrap(), reader.join().unwrap())
                });

                write_result?;
                let delta = delta?;

                let output = child.wait_with_output()?;
                if output.status.success() {
                    debug!(
                        "Delta encoded: {} bytes (ratio: {:.2}%)",
                        delta.len(),
                        (delta.len() as f64 / target.len() as f64) * 100.0
                    );
                    Ok(delta)
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!("xdelta3 CLI encode failed: {}", stderr);
                    Err(CodecError::EncodeFailed(format!(
                        "xdelta3 CLI failed: {}",
                        stderr
                    )))
                }
            }
            Err(e) => {
                warn!("Failed to execute xdelta3 CLI: {}", e);
                Err(CodecError::EncodeFailed(format!(
                    "xdelta3 CLI not available: {}",
                    e
                )))
            }
        }
    }

    /// Decode a delta to reconstruct the target from source + delta.
    ///
    /// PERF: Same piped I/O strategy as encode() — see encode() doc comment.
    /// Source stays as a temp file (xdelta3 needs random access); delta is piped
    /// via stdin; reconstructed output comes from stdout.
    #[instrument(skip(self, source, delta))]
    pub fn decode(&self, source: &[u8], delta: &[u8]) -> Result<Vec<u8>, CodecError> {
        if source.len() > self.max_size {
            return Err(CodecError::TooLarge {
                size: source.len(),
                max: self.max_size,
            });
        }

        debug!(
            "Decoding delta: source={} bytes, delta={} bytes",
            source.len(),
            delta.len()
        );

        if !self.cli_available {
            return Err(CodecError::DecodeFailed(
                "xdelta3 CLI binary is not available".to_string(),
            ));
        }

        // PERF: Source MUST remain a temp file — see encode() comment for why.
        let mut source_file = NamedTempFile::new()?;
        source_file.write_all(source)?;
        source_file.flush()?;

        // Run xdelta3 -d -s <source_file> -c
        // Delta data piped to stdin, reconstructed target written to stdout (`-c`).
        let result = Command::new("xdelta3")
            .args(["-d", "-s", source_file.path().to_str().unwrap(), "-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        match result {
            Ok(mut child) => {
                let child_stdin = child.stdin.take().unwrap();
                let mut child_stdout = child.stdout.take().unwrap();

                // PERF: Concurrent stdin/stdout — see encode() comment for why
                // sequential write-then-read causes pipe deadlocks.
                let (write_result, target) = std::thread::scope(|s| {
                    let writer = s.spawn(|| {
                        let mut stdin = child_stdin;
                        stdin.write_all(delta)?;
                        stdin.flush()?;
                        drop(stdin); // CRITICAL: EOF signal, see encode()
                        Ok::<(), std::io::Error>(())
                    });

                    let reader = s.spawn(|| {
                        let mut buf = Vec::new();
                        child_stdout.read_to_end(&mut buf)?;
                        Ok::<Vec<u8>, std::io::Error>(buf)
                    });

                    (writer.join().unwrap(), reader.join().unwrap())
                });

                write_result?;
                let target = target?;

                let output = child.wait_with_output()?;
                if output.status.success() {
                    debug!("Delta decoded: {} bytes", target.len());
                    Ok(target)
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!("xdelta3 CLI decode failed: {}", stderr);
                    Err(CodecError::DecodeFailed(format!(
                        "xdelta3 CLI failed: {}",
                        stderr
                    )))
                }
            }
            Err(e) => {
                warn!("Failed to execute xdelta3 CLI: {}", e);
                Err(CodecError::DecodeFailed(format!(
                    "xdelta3 CLI not available: {}",
                    e
                )))
            }
        }
    }

    /// Calculate compression ratio (delta_size / original_size)
    pub fn compression_ratio(original_size: usize, delta_size: usize) -> f32 {
        if original_size == 0 {
            return 1.0;
        }
        delta_size as f32 / original_size as f32
    }
}

impl Default for DeltaCodec {
    fn default() -> Self {
        Self::new(100 * 1024 * 1024) // 100MB default
    }
}

impl std::fmt::Debug for DeltaCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeltaCodec")
            .field("max_size", &self.max_size)
            .field("cli_available", &self.cli_available)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let codec = DeltaCodec::default();

        let source = b"Hello, this is the original file content!";
        let target = b"Hello, this is the modified file content!";

        let delta = codec.encode(source, target).unwrap();
        let reconstructed = codec.decode(source, &delta).unwrap();

        assert_eq!(reconstructed, target);
    }

    #[test]
    fn test_identical_files() {
        let codec = DeltaCodec::default();

        // Use a larger payload so the delta is meaningfully smaller than the original.
        // The xdelta3 CLI has ~50 bytes of header overhead, so tiny inputs may
        // produce a delta larger than the source.
        let data = vec![0x42u8; 1024];
        let delta = codec.encode(&data, &data).unwrap();

        // Delta for identical files should be much smaller than 1 KiB of data
        assert!(delta.len() < data.len());

        let reconstructed = codec.decode(&data, &delta).unwrap();
        assert_eq!(reconstructed, data);
    }

    #[test]
    fn test_compression_ratio() {
        assert_eq!(DeltaCodec::compression_ratio(100, 50), 0.5);
        assert_eq!(DeltaCodec::compression_ratio(100, 100), 1.0);
        assert_eq!(DeltaCodec::compression_ratio(0, 50), 1.0);
    }

    #[test]
    fn test_size_limit() {
        let codec = DeltaCodec::new(100); // 100 byte limit

        let large_data = vec![0u8; 200];
        let result = codec.encode(&large_data, &large_data);

        assert!(matches!(result, Err(CodecError::TooLarge { .. })));
    }

    #[test]
    fn test_decode_corrupted_delta_fails() {
        let codec = DeltaCodec::default();

        let source = b"Hello, this is the original file content!";
        let target = b"Hello, this is the modified file content!";

        let mut delta = codec.encode(source, target).unwrap();
        // Corrupt the delta by flipping bytes
        for byte in delta.iter_mut() {
            *byte = byte.wrapping_add(1);
        }

        let result = codec.decode(source, &delta);
        assert!(result.is_err() || result.unwrap() != target);
    }

    #[test]
    fn test_encode_empty_target() {
        let codec = DeltaCodec::default();

        let source = b"non-empty source content";
        let target = b"";

        let delta = codec.encode(source, target).unwrap();
        let reconstructed = codec.decode(source, &delta).unwrap();
        assert_eq!(reconstructed, target);
    }

    #[test]
    fn test_large_payload_no_pipe_deadlock() {
        let codec = DeltaCodec::default();

        let source = vec![0x42u8; 512 * 1024];
        let mut target = source.clone();
        for (i, byte) in target.iter_mut().enumerate().take(1000) {
            *byte = (i % 256) as u8;
        }

        let delta = codec.encode(&source, &target).unwrap();
        let reconstructed = codec.decode(&source, &delta).unwrap();
        assert_eq!(reconstructed, target);
    }

    #[test]
    fn test_very_large_payload_roundtrip() {
        let codec = DeltaCodec::default();

        let source = vec![0xAAu8; 2 * 1024 * 1024];
        let mut target = source.clone();
        let mut pos = 0;
        while pos < target.len() {
            target[pos] = target[pos].wrapping_add(1);
            pos += 1000;
        }

        let delta = codec.encode(&source, &target).unwrap();
        let reconstructed = codec.decode(&source, &delta).unwrap();
        assert_eq!(reconstructed, target);
    }

    #[test]
    fn test_encode_empty_source() {
        let codec = DeltaCodec::default();

        let source = b"";
        let target: Vec<u8> = (0..10240).map(|i| (i % 256) as u8).collect();

        let delta = codec.encode(source, &target).unwrap();
        let reconstructed = codec.decode(source, &delta).unwrap();
        assert_eq!(reconstructed, target);
    }

    #[test]
    fn test_encode_both_empty() {
        let codec = DeltaCodec::default();

        let source = b"";
        let target = b"";

        let delta = codec.encode(source, target).unwrap();
        let reconstructed = codec.decode(source, &delta).unwrap();
        assert_eq!(reconstructed.as_slice(), target.as_slice());
    }

    #[test]
    fn test_binary_with_nul_bytes() {
        let codec = DeltaCodec::default();

        let source: Vec<u8> = (0..4096)
            .map(|i| if i % 2 == 0 { 0u8 } else { (i % 255 + 1) as u8 })
            .collect();
        let target: Vec<u8> = (0..4096)
            .map(|i| if i % 2 == 1 { 0u8 } else { (i % 255 + 1) as u8 })
            .collect();

        let delta = codec.encode(&source, &target).unwrap();
        let reconstructed = codec.decode(&source, &delta).unwrap();
        assert_eq!(reconstructed, target);
    }

    #[test]
    fn test_exact_max_size_succeeds() {
        let codec = DeltaCodec::new(1000);

        let source = vec![0x42u8; 1000];
        let mut target = source.clone();
        target[0] = 0x43;

        let delta = codec.encode(&source, &target).unwrap();
        let reconstructed = codec.decode(&source, &delta).unwrap();
        assert_eq!(reconstructed, target);
    }

    #[test]
    fn test_one_byte_over_max_size_fails() {
        let codec = DeltaCodec::new(1000);

        // Source over limit
        let source_over = vec![0x42u8; 1001];
        let target_ok = vec![0x43u8; 1000];
        let result = codec.encode(&source_over, &target_ok);
        assert!(matches!(result, Err(CodecError::TooLarge { .. })));

        // Target over limit
        let source_ok = vec![0x42u8; 1000];
        let target_over = vec![0x43u8; 1001];
        let result = codec.encode(&source_ok, &target_over);
        assert!(matches!(result, Err(CodecError::TooLarge { .. })));
    }

    #[test]
    fn test_concurrent_encodes() {
        let codec = std::sync::Arc::new(DeltaCodec::default());

        std::thread::scope(|s| {
            let handles: Vec<_> = (0..8u8)
                .map(|i| {
                    let codec = std::sync::Arc::clone(&codec);
                    s.spawn(move || {
                        let source = vec![i.wrapping_mul(17); 50 * 1024];
                        let mut target = source.clone();
                        for byte in target.iter_mut().take(100) {
                            *byte = byte.wrapping_add(i).wrapping_add(1);
                        }

                        let delta = codec.encode(&source, &target).unwrap();
                        let reconstructed = codec.decode(&source, &delta).unwrap();
                        assert_eq!(reconstructed, target, "Thread {} roundtrip failed", i);
                    })
                })
                .collect();

            for h in handles {
                h.join().unwrap();
            }
        });
    }

    #[test]
    fn test_highly_compressible_identical_large() {
        let codec = DeltaCodec::default();

        let data = vec![0xBBu8; 256 * 1024];

        let delta = codec.encode(&data, &data).unwrap();
        assert!(
            delta.len() < 256 * 1024 / 2,
            "Delta for identical data should be much smaller than original, got {} bytes",
            delta.len()
        );

        let reconstructed = codec.decode(&data, &delta).unwrap();
        assert_eq!(reconstructed, data);
    }

    #[test]
    fn test_incompressible_random_data() {
        fn pseudo_random(seed: u64, size: usize) -> Vec<u8> {
            let mut data = Vec::with_capacity(size);
            let mut state = seed;
            for _ in 0..size {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                data.push((state >> 33) as u8);
            }
            data
        }

        let codec = DeltaCodec::default();

        let source = pseudo_random(42, 100_000);
        let target = pseudo_random(999, 100_000);

        let delta = codec.encode(&source, &target).unwrap();
        let reconstructed = codec.decode(&source, &delta).unwrap();
        assert_eq!(reconstructed, target);
    }
}
