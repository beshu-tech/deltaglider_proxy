// SPDX-License-Identifier: GPL-3.0-only

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
/// Override via `DGP_CODEC_TIMEOUT_SECS` for testing or constrained environments.
///
/// This wall-clock timeout governs the BUFFERED path (`encode`/`decode`), where
/// the whole operation is bounded (input ≤ max_size, finishes in seconds). The
/// STREAMING path (`encode_from_reader`/`decode_to_writer`) uses a STALL timeout
/// instead — see `codec_stall_timeout` — because a legitimate multi-GB stream
/// can run far past 60s while making steady progress.
fn codec_timeout() -> Duration {
    Duration::from_secs(crate::config::env_parse_with_default(
        "DGP_CODEC_TIMEOUT_SECS",
        60,
    ))
}

/// No-progress timeout for the STREAMING codec path. The streaming pump bumps a
/// progress clock on every chunk it reads from xdelta3's stdout; the watchdog
/// kills the child only if NO progress is made for this long. Spike B measured
/// the largest normal inter-chunk gap at ~13ms on a 1.6GB decode, so 30s has a
/// ~2300× margin over legitimate slow-but-working progress while still reaping a
/// genuinely hung process or a full-disk spool quickly.
fn codec_stall_timeout() -> Duration {
    Duration::from_secs(crate::config::env_parse_with_default(
        "DGP_CODEC_STALL_SECS",
        30,
    ))
}

/// Absolute ceiling for a single streaming codec op, regardless of progress.
/// The stall timeout alone can't bound a crafted delta that makes xdelta3 spin
/// forever while trickling a byte every few seconds (Spike B note). This hard
/// cap (default 2h — generous for a 100GB object even on slow storage) is the
/// backstop. Set high; it should never fire for a legitimate transfer.
fn codec_absolute_ceiling() -> Duration {
    Duration::from_secs(crate::config::env_parse_with_default(
        "DGP_CODEC_ABSOLUTE_SECS",
        2 * 60 * 60,
    ))
}

/// Shared progress clock for the streaming watchdog. The pump stores the
/// monotonic nanos of the last successful stdout read; the watchdog reads it to
/// decide whether xdelta3 has stalled. `start` is the op's spawn instant, used
/// for the absolute ceiling.
struct ProgressClock {
    start: Instant,
    last_progress: std::sync::atomic::AtomicU64, // nanos since `start`
}

impl ProgressClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last_progress: std::sync::atomic::AtomicU64::new(0),
        }
    }
    /// Record forward progress (called by the pump after each chunk).
    fn tick(&self) {
        let elapsed = self.start.elapsed().as_nanos() as u64;
        self.last_progress
            .store(elapsed, std::sync::atomic::Ordering::Relaxed);
    }
    /// Nanos since the last tick (how long we've been stalled).
    fn stalled_for(&self) -> Duration {
        let now = self.start.elapsed().as_nanos() as u64;
        let last = self
            .last_progress
            .load(std::sync::atomic::Ordering::Relaxed);
        Duration::from_nanos(now.saturating_sub(last))
    }
    fn total_elapsed(&self) -> Duration {
        self.start.elapsed()
    }
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
            // Loop against a real deadline: Condvar::wait_timeout is subject to
            // SPURIOUS wakeups, and a single wait that treats any wakeup as
            // "timeout expired" would SIGKILL a healthy xdelta3 mid-encode (a
            // sporadic, unreproducible 5xx). Re-check the done flag and the
            // remaining time on every wakeup; only proceed to kill once the flag
            // is still unset AND the wait genuinely timed out.
            let deadline = std::time::Instant::now() + timeout;
            let mut guard = mutex.lock().unwrap();
            loop {
                if flag.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (g, res) = condvar.wait_timeout(guard, remaining).unwrap();
                guard = g;
                if res.timed_out() {
                    break;
                }
            }
            drop(guard);
            if !flag.load(std::sync::atomic::Ordering::Acquire) {
                // Timeout expired and I/O hasn't finished — kill the child.
                // Use raw kill(pid, SIGKILL) since we don't own the Child handle
                // here (it's split into stdin/stdout/stderr and the `Child` stays
                // in run_xdelta3 for wait_with_timeout). This is a best-effort
                // timeout with a known, accepted PID-reuse race: in theory the
                // child could exit naturally and the OS could recycle `child_id`
                // for an unrelated process before this signal lands. In practice
                // the condvar synchronisation closes the window — we only reach
                // this branch after waiting the FULL timeout without the `done`
                // flag being set, and the child cannot be reaped (its `Child`
                // handle isn't waited on until run_xdelta3 calls
                // wait_with_timeout AFTER the pipe threads — and hence this
                // watchdog — have joined), so its PID stays reserved as a
                // zombie and cannot be reused while we signal it.
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

/// STREAMING pump: feed xdelta3 stdin from a `Read`, drain stdout in fixed
/// chunks to a `sink` callback, all with BOUNDED memory (the chunk buffers,
/// never the whole object). Replaces the `read_to_end` buffering of
/// `pipe_stdin_stdout_stderr` for the unbounded-size path.
///
/// Memory model (Spike A/C proven): peak heap is `CHUNK` × a small constant; the
/// source is mmap'd by xdelta3 from a file (not piped), and the output is handed
/// chunk-by-chunk to `sink` (which writes a spool file + hashes), never buffered.
///
/// Watchdog: STALL-based, not wall-clock. `progress.tick()` fires after every
/// stdout chunk; the watchdog kills the child if no progress for
/// `stall_timeout`, OR if the absolute `ceiling` is exceeded (backstop against a
/// trickle-forever crafted delta). Spike B confirmed normal inter-chunk gaps are
/// ~13ms, so the stall timeout never misfires on legitimate slow streams.
///
/// Returns `(write_result, stdout_bytes_total)`. `sink` errors abort the pump.
#[allow(clippy::too_many_arguments)]
fn pipe_streaming<R, W>(
    child_stdin: std::process::ChildStdin,
    child_stdout: std::process::ChildStdout,
    mut child_stderr: std::process::ChildStderr,
    mut input: R,
    mut sink: W,
    child_id: u32,
    stall_timeout: Duration,
    ceiling: Duration,
    progress: &ProgressClock,
) -> (IoResult<()>, IoResult<u64>, IoResult<Vec<u8>>)
where
    R: Read + Send,
    W: FnMut(&[u8]) -> IoResult<()> + Send,
{
    /// Streaming pump chunk size. 256KiB matches Spike B's measurement chunk and
    /// is a good balance of syscall overhead vs resident memory.
    const PUMP_CHUNK: usize = 256 * 1024;

    let done = std::sync::Arc::new((
        std::sync::atomic::AtomicBool::new(false),
        std::sync::Condvar::new(),
        std::sync::Mutex::new(()),
    ));

    std::thread::scope(|s| {
        // Stall-based watchdog. Wakes every `slice` to re-check progress instead
        // of sleeping the whole timeout, so it reaps a hung child promptly.
        let done_clone = done.clone();
        s.spawn(move || {
            let (ref flag, ref condvar, ref mutex) = *done_clone;
            let slice = std::cmp::min(stall_timeout, Duration::from_secs(1));
            loop {
                let guard = mutex.lock().unwrap();
                let (_g, _to) = condvar.wait_timeout(guard, slice).unwrap();
                if flag.load(std::sync::atomic::Ordering::Acquire) {
                    return; // pump finished — stand down
                }
                let stalled = progress.stalled_for() >= stall_timeout;
                let over_ceiling = progress.total_elapsed() >= ceiling;
                if stalled || over_ceiling {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(child_id as i32, libc::SIGKILL);
                    }
                    warn!(
                        "Watchdog killed streaming xdelta3 (pid {}): {}",
                        child_id,
                        if over_ceiling {
                            format!("exceeded absolute ceiling {}s", ceiling.as_secs())
                        } else {
                            format!("no progress for {}s", stall_timeout.as_secs())
                        }
                    );
                    return;
                }
            }
        });

        // Writer: stream the input into xdelta3 stdin in chunks (bounded).
        // Tick progress here too: a large-input/sparse-output encode (an
        // incompressible target → tiny delta) can read GBs of stdin while
        // emitting almost no stdout, which would otherwise look like a stall and
        // get the healthy encode SIGKILLed (x-ray finding).
        let writer = s.spawn(move || {
            let mut stdin = child_stdin;
            let mut buf = vec![0u8; PUMP_CHUNK];
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                stdin.write_all(&buf[..n])?;
                progress.tick();
            }
            stdin.flush()?;
            drop(stdin); // EOF → child finishes
            Ok::<(), std::io::Error>(())
        });

        // Reader: drain stdout chunk-by-chunk to the sink, ticking progress.
        let stdout_reader = s.spawn(move || {
            let mut rdr = child_stdout;
            let mut buf = vec![0u8; PUMP_CHUNK];
            let mut total: u64 = 0;
            loop {
                let n = rdr.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                progress.tick();
                sink(&buf[..n])?;
                total += n as u64;
            }
            Ok::<u64, std::io::Error>(total)
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
    /// The installed xdelta3 version line (`xdelta3 -V`), captured at probe time.
    /// `None` when the binary is absent. Surfaced at boot so operators can see
    /// exactly which xdelta3 is in play (the version determines the delta
    /// FORMAT + the armor default — see `armor_supported`).
    cli_version: Option<String>,
    /// Whether this xdelta3 accepts the `-a` (disable armor) flag. `-a` is a
    /// 3.1+ option: newer xdelta3 turns "armor" (a whole-stream BLAKE3 frame)
    /// ON BY DEFAULT, which can't write to a pipe and aborts our piped encode.
    /// Older xdelta3 (3.0.x, e.g. Debian/Ubuntu's 3.0.11) has NO `-a` and would
    /// ERROR on the unknown flag — so we PROBE for support and pass `-a` only
    /// when accepted. Detected at construction, never per-request.
    armor_supported: bool,
}

impl DeltaCodec {
    /// Create a new codec with size limit.
    /// Probes for the xdelta3 CLI binary once at construction.
    pub fn new(max_size: usize) -> Self {
        let cli_version = Self::probe_version();
        let cli_available = cli_version.is_some();
        let armor_supported = cli_available && Self::probe_armor_flag();
        Self {
            max_size,
            cli_available,
            cli_version,
            armor_supported,
        }
    }

    /// Probe `xdelta3 -V`, returning the trimmed first version line on success
    /// (e.g. "Xdelta version 3.0.11..."), or `None` if the binary is missing /
    /// errors. `-V` prints to stderr.
    fn probe_version() -> Option<String> {
        let output = Command::new("xdelta3").arg("-V").output().ok()?;
        if !output.status.success() {
            return None;
        }
        // Version banner goes to stderr; fall back to stdout just in case.
        let banner = if output.stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout)
        } else {
            String::from_utf8_lossy(&output.stderr)
        };
        banner.lines().next().map(|l| l.trim().to_string())
    }

    /// Probe whether `-a` (disable armor) is a recognised flag. We do a real
    /// tiny encode WITH `-a` (source+target = the same 1 byte, piped exactly
    /// like the hot path) and accept the flag only if that round-trips. This is
    /// robust against version-string parsing — it tests the actual behaviour,
    /// and never false-positives on an old build that prints its banner-and-
    /// exits-0 on an unknown flag (the encode itself must succeed).
    fn probe_armor_flag() -> bool {
        use std::io::Write;
        let Ok(src) = NamedTempFile::new() else {
            return false;
        };
        // Source file content is irrelevant; just needs to exist + be seekable.
        if std::fs::write(src.path(), b"x").is_err() {
            return false;
        }
        let Some(src_path) = src.path().to_str() else {
            return false;
        };
        let child = Command::new("xdelta3")
            .args(["-a", "-e", "-D", "-s", src_path, "-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            return false;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"y");
            // drop closes the pipe → EOF
        }
        match child.wait_with_output() {
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    }

    /// Returns whether the xdelta3 CLI is available.
    pub fn is_cli_available(&self) -> bool {
        self.cli_available
    }

    /// The installed xdelta3 version line, if available.
    pub fn cli_version(&self) -> Option<&str> {
        self.cli_version.as_deref()
    }

    /// Whether `-a` (disable armor) is passed to xdelta3 — true on 3.1+ builds
    /// that accept it (and need it to encode through a pipe), false on 3.0.x.
    pub fn armor_disabled(&self) -> bool {
        self.armor_supported
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
        // Validate sizes — both source and target must fit within max_size.
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

        let output = self.run_xdelta3("-e", source, target)?;

        debug!(
            "Delta encoded: {} bytes (ratio: {:.2}%)",
            output.len(),
            (output.len() as f64 / target.len() as f64) * 100.0
        );
        Ok(output)
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

        let output = self.run_xdelta3("-d", source, delta)?;

        debug!("Delta decoded: {} bytes", output.len());
        Ok(output)
    }

    /// Run xdelta3 in encode (`-e`) or decode (`-d`) mode.
    ///
    /// Shared implementation for `encode()` and `decode()`. The `mode` argument
    /// is either `"-e"` (encode) or `"-d"` (decode).
    ///
    /// PERF: Source MUST remain a temp file — xdelta3 needs random-access (mmap)
    /// to the source for its sliding-window algorithm. Do NOT try to pipe it via
    /// stdin; xdelta3 can only read source from a seekable file descriptor.
    /// The input (target for encode, delta for decode) is piped via stdin;
    /// output comes from stdout (`-c` flag).
    fn run_xdelta3(&self, mode: &str, source: &[u8], input: &[u8]) -> Result<Vec<u8>, CodecError> {
        let make_error = |msg: String| -> CodecError {
            if mode == "-e" {
                CodecError::EncodeFailed(msg)
            } else {
                CodecError::DecodeFailed(msg)
            }
        };

        if !self.cli_available {
            return Err(make_error(
                "xdelta3 CLI binary is not available".to_string(),
            ));
        }

        let mut source_file = NamedTempFile::new()?;
        source_file.write_all(source)?;
        source_file.flush()?;

        let source_path = source_file
            .path()
            .to_str()
            .ok_or_else(|| make_error("temp file path is not valid UTF-8".to_string()))?;

        // Base args. -D is critical for transparent object storage: xdelta3
        // otherwise auto-decompresses recognised compressed inputs (gzip/xz/etc.)
        // and recompresses on decode, which preserves logical content but not
        // byte identity. S3 clients require exact original bytes.
        //
        // -a disables "armor" — a whole-stream BLAKE3 integrity frame that newer
        // xdelta3 (3.1+) turns ON BY DEFAULT. Armor must seek back over the whole
        // stream to hash it, but we feed the target via a PIPE (non-seekable), so
        // without -a newer builds abort with "armor requires a seekable target:
        // /dev/stdin". -a also keeps the output a plain RFC-3284 VCDIFF — format-
        // identical to deltas made by older xdelta3 — so a delta produced on any
        // version decodes on any other. We never rely on armor's check: the
        // engine SHA-256-verifies every reconstruction itself.
        //
        // CRITICAL: -a is a 3.1+ flag. Older xdelta3 (3.0.x, what Debian/Ubuntu
        // ship today) has no -a and would ERROR on it, so we only pass it when
        // `armor_supported` (probed at construction). On 3.0.x there's no armor
        // to disable anyway.
        let mut args: Vec<&str> = vec![mode, "-D"];
        if self.armor_supported {
            args.push("-a");
        }
        args.extend(["-s", source_path, "-c"]);
        let result = Command::new("xdelta3")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let op_name = if mode == "-e" { "encode" } else { "decode" };

        match result {
            Ok(mut child) => {
                let child_id = child.id();
                // These .expect() calls are safe: we configured piped stdin/stdout/stderr
                // above, so .take() only returns None if called twice (which we don't).
                let child_stdin = child.stdin.take().expect("piped stdin");
                let child_stdout = child.stdout.take().expect("piped stdout");
                let child_stderr = child.stderr.take().expect("piped stderr");

                let (write_result, output, stderr_result) = pipe_stdin_stdout_stderr(
                    child_stdin,
                    child_stdout,
                    child_stderr,
                    input,
                    self.max_size,
                    child_id,
                    codec_timeout(),
                );
                write_result?;
                let output = output?;
                let stderr_bytes = stderr_result.unwrap_or_default();

                let status = wait_with_timeout(&mut child, codec_timeout())?;
                if status.success() {
                    Ok(output)
                } else {
                    let stderr = String::from_utf8_lossy(&stderr_bytes);
                    warn!("xdelta3 CLI {} failed: {}", op_name, stderr);
                    Err(make_error(format!("xdelta3 CLI failed: {}", stderr)))
                }
            }
            Err(e) => {
                warn!("Failed to execute xdelta3 CLI: {}", e);
                Err(make_error(format!("xdelta3 CLI not available: {}", e)))
            }
        }
    }

    /// STREAMING decode: reconstruct target from a source FILE + a streamed
    /// delta, writing the output chunk-by-chunk to `out`. Bounded memory — the
    /// source is mmap'd by xdelta3 (Spike A: 73MB RSS on a 2.5GB source), the
    /// delta streams in, the output streams out. Returns bytes written.
    ///
    /// The caller owns `source_path` (a seekable file) so the reference temp
    /// file's lifecycle / quota can be managed outside the codec (Phase 2).
    pub fn decode_to_writer<R: Read + Send, W: Write + Send>(
        &self,
        source_path: &std::path::Path,
        delta: R,
        out: W,
    ) -> Result<u64, CodecError> {
        self.run_xdelta3_streaming("-d", source_path, delta, out)
    }

    /// STREAMING encode: produce a delta of a streamed target against a source
    /// FILE, writing the delta chunk-by-chunk to `out`. Bounded memory (Spike C:
    /// 2MB RSS on a 1.5GB target). The caller decides delta-vs-passthrough from
    /// the bytes written to `out` (it can cap `out` and abort). Returns delta
    /// bytes written.
    pub fn encode_from_reader<R: Read + Send, W: Write + Send>(
        &self,
        source_path: &std::path::Path,
        target: R,
        out: W,
    ) -> Result<u64, CodecError> {
        self.run_xdelta3_streaming("-e", source_path, target, out)
    }

    /// Shared streaming driver for `decode_to_writer` / `encode_from_reader`.
    /// Mirrors `run_xdelta3` (same args, same -a/armor logic) but drives the
    /// STALL-based streaming pump instead of the buffered one, and takes the
    /// source as a path the caller owns rather than a `&[u8]` it copies.
    fn run_xdelta3_streaming<R: Read + Send, W: Write + Send>(
        &self,
        mode: &str,
        source_path: &std::path::Path,
        input: R,
        mut out: W,
    ) -> Result<u64, CodecError> {
        let make_error = |msg: String| -> CodecError {
            if mode == "-e" {
                CodecError::EncodeFailed(msg)
            } else {
                CodecError::DecodeFailed(msg)
            }
        };
        if !self.cli_available {
            return Err(make_error(
                "xdelta3 CLI binary is not available".to_string(),
            ));
        }
        let source_str = source_path
            .to_str()
            .ok_or_else(|| make_error("source path is not valid UTF-8".to_string()))?;

        // Same arg construction as the buffered path (incl. version-aware -a).
        let mut args: Vec<&str> = vec![mode, "-D"];
        if self.armor_supported {
            args.push("-a");
        }
        args.extend(["-s", source_str, "-c"]);

        let mut child = Command::new("xdelta3")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| make_error(format!("xdelta3 CLI not available: {}", e)))?;

        let child_id = child.id();
        let child_stdin = child.stdin.take().expect("piped stdin");
        let child_stdout = child.stdout.take().expect("piped stdout");
        let child_stderr = child.stderr.take().expect("piped stderr");

        let progress = ProgressClock::new();
        let (write_result, total, stderr_result) = pipe_streaming(
            child_stdin,
            child_stdout,
            child_stderr,
            input,
            |chunk: &[u8]| out.write_all(chunk),
            child_id,
            codec_stall_timeout(),
            codec_absolute_ceiling(),
            &progress,
        );
        write_result?;
        let total = total?;
        let stderr_bytes = stderr_result.unwrap_or_default();

        // The streaming watchdog uses raw kill(); reap the child here (its
        // wall-clock is bounded by the absolute ceiling the watchdog enforces).
        let status = wait_with_timeout(&mut child, codec_absolute_ceiling())?;
        if status.success() {
            out.flush()?;
            Ok(total)
        } else {
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            let op = if mode == "-e" { "encode" } else { "decode" };
            warn!("xdelta3 streaming {} failed: {}", op, stderr);
            Err(make_error(format!("xdelta3 CLI failed: {}", stderr)))
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

    /// Write `data` to a fresh temp file and return it (the seekable source the
    /// streaming codec entry points require).
    fn source_tempfile(data: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(data).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn streaming_encode_then_streaming_decode_roundtrips() {
        let codec = DeltaCodec::default();
        // A meaningfully-sized, similar pair so the delta is real but small.
        let source: Vec<u8> = (0..200_000u32).flat_map(|n| n.to_le_bytes()).collect();
        let mut target = source.clone();
        for off in [1000usize, 400_000, 790_000] {
            target[off] ^= 0xff;
        }
        let src_file = source_tempfile(&source);

        // Streaming encode: target via Read, delta to a Vec sink.
        let mut delta = Vec::new();
        let dn = codec
            .encode_from_reader(src_file.path(), &target[..], &mut delta)
            .unwrap();
        assert_eq!(dn as usize, delta.len());
        assert!(delta.len() < target.len(), "delta should beat raw size");

        // Streaming decode: delta via Read, reconstruction to a Vec sink.
        let mut recon = Vec::new();
        let rn = codec
            .decode_to_writer(src_file.path(), &delta[..], &mut recon)
            .unwrap();
        assert_eq!(rn as usize, recon.len());
        assert_eq!(recon, target, "streaming roundtrip must be byte-exact");
    }

    #[test]
    fn streaming_decode_matches_buffered_decode() {
        // A delta made by the BUFFERED encode must decode identically through the
        // STREAMING path — proves format compatibility between the two paths.
        let codec = DeltaCodec::default();
        let source = vec![7u8; 100_000];
        let mut target = source.clone();
        target.extend_from_slice(b"appended tail that differs");
        let delta = codec.encode(&source, &target).unwrap();

        let src_file = source_tempfile(&source);
        let mut recon = Vec::new();
        codec
            .decode_to_writer(src_file.path(), &delta[..], &mut recon)
            .unwrap();
        assert_eq!(recon, target);
    }

    #[test]
    fn progress_clock_detects_stall() {
        // The stall watchdog rests on ProgressClock: after a tick, stalled_for
        // starts climbing; another tick resets it.
        let clock = ProgressClock::new();
        clock.tick();
        std::thread::sleep(Duration::from_millis(30));
        let stalled = clock.stalled_for();
        assert!(
            stalled >= Duration::from_millis(20),
            "stall should accrue after a tick: {stalled:?}"
        );
        clock.tick();
        assert!(
            clock.stalled_for() < Duration::from_millis(20),
            "a fresh tick resets the stall window"
        );
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

    #[test]
    fn test_xz_magic_bytes_roundtrip_preserves_exact_compressed_bytes() {
        let codec = DeltaCodec::default();

        // xdelta3 auto-detects common compressed formats by magic bytes and,
        // unless -D is passed, transparently decompresses/recompresses them.
        // That is useful for patches but invalid for S3 transparency because
        // recompression changes bytes/checksums. These are not valid xz files,
        // but they exercise the "compressed input" magic-byte path.
        let mut source = b"\xFD7zXZ\x00".to_vec();
        source.extend((0..4096).map(|i| (i % 251) as u8));

        let mut target = source.clone();
        target.extend_from_slice(b"-new-release-bytes");
        for i in (128..target.len()).step_by(257) {
            target[i] = target[i].wrapping_add(17);
        }

        let delta = codec.encode(&source, &target).unwrap();
        let reconstructed = codec.decode(&source, &delta).unwrap();
        assert_eq!(reconstructed, target);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    // THE INVARIANT: `decode(source, encode(source, target)) == target` must hold
    // byte-for-byte for every input. This is the single most important correctness
    // property of the whole product — if it ever fails, customer data is silently
    // corrupted on GET (the delta reconstructed during retrieval would not equal
    // the bytes that were stored). These property tests fuzz that round-trip across
    // random, empty-source, and realistic-mutation workloads.
    //
    // Each case shells out to the xdelta3 subprocess twice (encode + decode), so we
    // keep input sizes bounded (≤4 KiB) and rely on proptest's default case count to
    // keep wall-clock cost reasonable. The whole module is guarded on CLI
    // availability — like the rest of the codec tests, it short-circuits (here via
    // `prop_assume!`) when the xdelta3 binary isn't installed so it never fails on a
    // machine that lacks it.

    /// Max generated input size. Bounded because every case spawns xdelta3 twice.
    const MAX_LEN: usize = 4096;

    proptest! {
        /// Round-trip holds for arbitrary source/target byte vectors (random noise).
        #[test]
        fn prop_roundtrip_arbitrary(
            source in proptest::collection::vec(any::<u8>(), 0..MAX_LEN),
            target in proptest::collection::vec(any::<u8>(), 0..MAX_LEN),
        ) {
            let codec = DeltaCodec::default();
            prop_assume!(codec.is_cli_available());

            let delta = codec.encode(&source, &target).unwrap();
            let reconstructed = codec.decode(&source, &delta).unwrap();
            prop_assert_eq!(reconstructed, target);
        }

        /// Round-trip holds for the "no reference / first upload" case: encoding
        /// against an empty source and decoding back must reproduce the target.
        #[test]
        fn prop_roundtrip_empty_source(
            target in proptest::collection::vec(any::<u8>(), 0..MAX_LEN),
        ) {
            let codec = DeltaCodec::default();
            prop_assume!(codec.is_cli_available());

            let source: &[u8] = &[];
            let delta = codec.encode(source, &target).unwrap();
            let reconstructed = codec.decode(source, &delta).unwrap();
            prop_assert_eq!(reconstructed, target);
        }

        /// Round-trip holds for the realistic delta-compression workload: a target
        /// that is a small mutation of the source (bytes flipped + a few inserted).
        /// This exercises xdelta3's actual copy/add VCDIFF paths rather than the
        /// degenerate all-noise case.
        #[test]
        fn prop_roundtrip_small_mutation(
            source in proptest::collection::vec(any::<u8>(), 1..MAX_LEN),
            // Indices (mod source.len()) at which to flip a byte.
            flips in proptest::collection::vec(any::<usize>(), 0..16),
            // Bytes to splice into the middle of the target.
            insert in proptest::collection::vec(any::<u8>(), 0..32),
        ) {
            let codec = DeltaCodec::default();
            prop_assume!(codec.is_cli_available());

            let mut target = source.clone();
            for idx in flips {
                let i = idx % target.len();
                target[i] = target[i].wrapping_add(1);
            }
            // Splice the insert bytes near the middle so the delta has both a copy
            // window before and after the edit.
            let mid = target.len() / 2;
            target.splice(mid..mid, insert);

            let delta = codec.encode(&source, &target).unwrap();
            let reconstructed = codec.decode(&source, &delta).unwrap();
            prop_assert_eq!(reconstructed, target);
        }
    }
}
