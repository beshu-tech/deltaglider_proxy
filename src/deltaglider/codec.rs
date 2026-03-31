//! xdelta3 codec wrapper for delta encoding/decoding
//!
//! Uses the xdelta3 CLI binary for both encoding and decoding to ensure
//! compatibility with deltas created by the original DeltaGlider Python CLI.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use thiserror::Error;
use tracing::{debug, instrument, warn};

/// Maximum time to wait for xdelta3 subprocess to complete.
/// Default 60s is generous for 100MB max object size — xdelta3 typically
/// processes 100MB in <5s. Hung processes are killed to prevent cascading.
/// Override via `DGP_codec_timeout()_SECS` for testing or constrained environments.
fn codec_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("DGP_codec_timeout()_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    )
}

/// Wait for a child process with a timeout. Kills the child if the deadline is exceeded.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus, CodecError> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap zombie
                    return Err(CodecError::EncodeFailed(format!(
                        "xdelta3 subprocess timed out after {}s",
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(CodecError::Io(e)),
        }
    }
}

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

type IoResult<T> = Result<T, std::io::Error>;

/// Pipe data to a child process's stdin while concurrently reading stdout and
/// draining stderr. All three streams are consumed concurrently to prevent
/// pipe-buffer deadlocks. A watchdog thread kills the child after `timeout`
/// to prevent hung xdelta3 processes from permanently consuming codec slots.
///
/// `max_stdout` caps how many bytes are read from stdout to guard against
/// decompression bombs (a crafted VCDIFF delta can amplify small input to
/// gigabytes of output). If exceeded, the read is aborted with an error.
///
/// Returns `(write_result, stdout_bytes, stderr_bytes)`.
///
/// PERF: We MUST handle stdin/stdout/stderr concurrently using `thread::scope`.
/// If any pipe buffer fills (~64KB Linux, ~16KB macOS), the child blocks on
/// write() and we deadlock. All three pipes must be drained in parallel.
fn pipe_stdin_stdout_stderr(
    child_stdin: std::process::ChildStdin,
    child_stdout: std::process::ChildStdout,
    mut child_stderr: std::process::ChildStderr,
    input: &[u8],
    max_stdout: usize,
    child_id: u32,
    timeout: Duration,
) -> (IoResult<()>, IoResult<Vec<u8>>, IoResult<Vec<u8>>) {
    // Flag set to true when pipe I/O completes normally, signalling the
    // watchdog to stand down. Using AtomicBool + Condvar so the watchdog
    // can wake immediately instead of sleeping the full timeout.
    let done = std::sync::Arc::new((
        std::sync::atomic::AtomicBool::new(false),
        std::sync::Condvar::new(),
        std::sync::Mutex::new(()),
    ));

    std::thread::scope(|s| {
        // Watchdog: kills the child if pipe I/O takes longer than codec_timeout().
        // When the child is killed, its pipe ends close, unblocking the reader
        // threads. Without this, a hung xdelta3 blocks read_to_end() forever
        // and the codec semaphore slot is permanently lost.
        let done_clone = done.clone();
        s.spawn(move || {
            let (ref flag, ref condvar, ref mutex) = *done_clone;
            let guard = mutex.lock().unwrap();
            let _result = condvar.wait_timeout(guard, timeout).unwrap();
            if !flag.load(std::sync::atomic::Ordering::Acquire) {
                // Timeout expired and I/O hasn't finished — kill the child.
                // Use raw kill(pid, SIGKILL) since we don't own the Child handle.
                #[cfg(unix)]
                {
                    unsafe {
                        libc::kill(child_id as i32, libc::SIGKILL);
                    }
                }
                warn!(
                    "Watchdog killed hung xdelta3 process (pid {}) after {}s",
                    child_id,
                    timeout.as_secs()
                );
            }
        });

        let writer = s.spawn(|| {
            let mut stdin = child_stdin;
            stdin.write_all(input)?;
            stdin.flush()?;
            // CRITICAL: drop(stdin) closes the pipe so the child sees EOF
            // and finishes processing. Without this, the child hangs forever
            // waiting for more input.
            drop(stdin);
            Ok::<(), std::io::Error>(()) // close stdin → child sees EOF
        });

        let stdout_reader = s.spawn(move || {
            // Read with a size cap to prevent decompression bombs.
            let mut buf = Vec::new();
            let mut limited = child_stdout.take(max_stdout as u64 + 1);
            limited.read_to_end(&mut buf)?;
            if buf.len() > max_stdout {
                return Err(std::io::Error::other(format!(
                    "output exceeds maximum size ({} > {} bytes)",
                    buf.len(),
                    max_stdout
                )));
            }
            Ok::<Vec<u8>, std::io::Error>(buf)
        });

        let stderr_reader = s.spawn(|| {
            let mut buf = Vec::new();
            child_stderr.read_to_end(&mut buf)?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        });

        let result = (
            writer.join().unwrap(),
            stdout_reader.join().unwrap(),
            stderr_reader.join().unwrap(),
        );

        // Signal the watchdog to stand down
        let (ref flag, ref condvar, _) = *done;
        flag.store(true, std::sync::atomic::Ordering::Release);
        condvar.notify_one();

        result
    })
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

    /// Returns the max object size this codec will accept (in bytes).
    pub fn max_size(&self) -> usize {
        self.max_size
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
                let child_id = child.id();
                let child_stdin = child.stdin.take().unwrap();
                let child_stdout = child.stdout.take().unwrap();
                let child_stderr = child.stderr.take().unwrap();

                let (write_result, delta, stderr_result) = pipe_stdin_stdout_stderr(
                    child_stdin,
                    child_stdout,
                    child_stderr,
                    target,
                    self.max_size,
                    child_id,
                    codec_timeout(),
                );
                write_result?;
                let delta = delta?;
                let stderr_bytes = stderr_result.unwrap_or_default();

                let status = wait_with_timeout(&mut child, codec_timeout())?;
                if status.success() {
                    debug!(
                        "Delta encoded: {} bytes (ratio: {:.2}%)",
                        delta.len(),
                        (delta.len() as f64 / target.len() as f64) * 100.0
                    );
                    Ok(delta)
                } else {
                    let stderr = String::from_utf8_lossy(&stderr_bytes);
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
                let child_id = child.id();
                let child_stdin = child.stdin.take().unwrap();
                let child_stdout = child.stdout.take().unwrap();
                let child_stderr = child.stderr.take().unwrap();

                let (write_result, target, stderr_result) = pipe_stdin_stdout_stderr(
                    child_stdin,
                    child_stdout,
                    child_stderr,
                    delta,
                    self.max_size,
                    child_id,
                    codec_timeout(),
                );
                write_result?;
                let target = target?;
                let stderr_bytes = stderr_result.unwrap_or_default();

                let status = wait_with_timeout(&mut child, codec_timeout())?;
                if status.success() {
                    debug!("Delta decoded: {} bytes", target.len());
                    Ok(target)
                } else {
                    let stderr = String::from_utf8_lossy(&stderr_bytes);
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
