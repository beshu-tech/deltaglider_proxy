// SPDX-License-Identifier: BUSL-1.1

//! One proxy process per state directory.
//!
//! The state directory (the folder of the config DB) holds the SQLite files
//! `deltaglider_config.db`, `deltaglider_jobs.db` and `deltaglider_usage.db`.
//! Two processes on them corrupt the job state and the usage counters. The
//! first process takes an exclusive `flock` on `.deltaglider.lock` in that
//! folder and keeps it until it exits; the kernel drops it on a crash, so a
//! stale file never blocks a restart. A second process waits a bounded time
//! (`DGP_STATE_LOCK_WAIT_SECS`) for the first one to stop, then refuses to
//! start. The holder writes its process id into the lock file, so the
//! refusal can name it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The lock file name inside the state directory.
pub const LOCK_FILE_NAME: &str = ".deltaglider.lock";

/// Held for the life of the process. Dropping it releases the lock.
#[derive(Debug)]
pub struct DataDirLock {
    _file: std::fs::File,
    path: PathBuf,
    /// False when the file system offers no lock (`Attempt::Unsupported`,
    /// some network file systems): the guard then protects nothing.
    held: bool,
}

impl DataDirLock {
    /// The lock file this guard holds.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Does the file system really hold the lock? `false` on a file system
    /// without locks, where this process cannot tell whether another one
    /// uses the directory.
    pub fn is_held(&self) -> bool {
        self.held
    }

    /// Is this the lock of the directory that holds `file`? The comparison
    /// is on the real paths, so `./` and an absolute path agree.
    pub fn covers(&self, file: &Path) -> bool {
        let dir_of = |p: &Path| {
            let parent = p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf())
        };
        dir_of(&self.path) == dir_of(file)
    }
}

/// How often a waiting process asks for the lock again.
const POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// The result of one lock attempt. Pure input of the wait loop.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    Held,
    /// Another process holds it.
    Busy,
    /// The file system cannot lock (some network file systems): go on without.
    Unsupported,
    Failed(String),
}

/// `ENOLCK` and `EOPNOTSUPP` mean that the file system offers no lock.
fn classify(result: Result<(), std::fs::TryLockError>) -> Attempt {
    match result {
        Ok(()) => Attempt::Held,
        Err(std::fs::TryLockError::WouldBlock) => Attempt::Busy,
        Err(std::fs::TryLockError::Error(e)) => {
            #[cfg(unix)]
            if matches!(e.raw_os_error(), Some(libc::ENOLCK | libc::EOPNOTSUPP)) {
                return Attempt::Unsupported;
            }
            Attempt::Failed(e.to_string())
        }
    }
}

/// Take the exclusive lock of `dir`. While another process holds it, ask
/// again for up to `wait` (a rolling deploy: the old process is still
/// stopping), then fail. `on_wait` runs once, at the first refusal. The
/// error text names the directory and says what to do.
pub fn acquire(
    dir: &Path,
    wait: Duration,
    mut on_wait: impl FnMut(),
) -> Result<DataDirLock, String> {
    let path = dir.join(LOCK_FILE_NAME);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| {
            format!(
                "cannot open the state directory lock file '{}': {e}. Make sure the directory \
                 exists and this user can write to it",
                path.display()
            )
        })?;
    let deadline = Instant::now() + wait;
    let mut announced = false;
    loop {
        match classify(file.try_lock()) {
            Attempt::Held => {
                write_holder(&file);
                return Ok(DataDirLock {
                    _file: file,
                    path,
                    held: true,
                });
            }
            Attempt::Unsupported => {
                tracing::warn!(
                    "the file system of '{}' does not support file locks; this process cannot \
                     tell whether another process uses the state directory",
                    dir.display()
                );
                return Ok(DataDirLock {
                    _file: file,
                    path,
                    held: false,
                });
            }
            Attempt::Failed(e) => return Err(format!("cannot lock '{}': {e}", path.display())),
            Attempt::Busy if Instant::now() >= deadline => {
                return Err(busy_message(dir, &path, wait));
            }
            Attempt::Busy => {
                if !announced {
                    announced = true;
                    on_wait();
                }
                std::thread::sleep(POLL);
            }
        }
    }
}

/// Record who holds the lock, in the lock file: one write of one line. The
/// process id is the one in the holder's own namespace (in a container it
/// is often 1), so the line also has the host name (the container id in a
/// container) and the start time. Best effort: the lock does not depend on
/// it, it only makes the refusal of a second process plainer.
fn write_holder(file: &std::fs::File) {
    use std::io::Write;
    let mut file = file;
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .unwrap_or_default();
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let line = holder_text(std::process::id(), host.trim(), started);
    let _ = file.set_len(0);
    let _ = file.write_all(line.as_bytes());
}

/// Pure: the holder line of the lock file.
fn holder_text(pid: u32, host: &str, started_unix: u64) -> String {
    let host = if host.is_empty() {
        "unknown host"
    } else {
        host
    };
    format!("pid {pid} on host {host}, started at unix time {started_unix}\n")
}

/// The holder text of the lock file, when it has one.
fn read_holder(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The refusal text. Pure over its inputs and the lock file text. It names
/// the directory, the holder (when known), what happened, and the fix: a
/// deploy must stop the old process before it starts the new one.
fn busy_message(dir: &Path, path: &Path, wait: Duration) -> String {
    let holder = read_holder(path)
        .map(|h| format!("; held by {h}; the pid is the one in the holder's own container or host"))
        .unwrap_or_default();
    let waited = if wait.is_zero() {
        String::new()
    } else {
        format!(" and did not stop within {} s", wait.as_secs().max(1))
    };
    format!(
        "another DeltaGlider Proxy process already uses the state directory '{}' \
         (lock file '{}'{holder}){waited}. Two processes on one state directory are not allowed. \
         Stop the old process before the new one starts: with Docker Compose run \
         `docker compose down` first, and on Kubernetes set the Deployment `strategy` to \
         `Recreate`. A rolling update works only when the old process stops within \
         DGP_STATE_LOCK_WAIT_SECS: raise it, or stop the old process first. To run a second process, give it its own directory with \
         DGP_CONFIG",
        dir.display(),
        path.display(),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const NO_WAIT: Duration = Duration::ZERO;

    #[test]
    fn a_second_holder_is_refused_with_the_directory_named() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path(), NO_WAIT, || {}).expect("first lock");
        let err = acquire(dir.path(), NO_WAIT, || {}).expect_err("second lock must fail");
        assert!(err.contains("another DeltaGlider Proxy process"), "{err}");
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
        drop(first);
        acquire(dir.path(), NO_WAIT, || {}).expect("the lock is free after the holder drops");
    }

    #[test]
    fn the_refusal_names_the_holder_and_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let _first = acquire(dir.path(), NO_WAIT, || {}).expect("first lock");
        let err = acquire(dir.path(), NO_WAIT, || {}).expect_err("second lock must fail");
        assert!(
            err.contains(&format!("pid {} on host", std::process::id())),
            "the holder is named: {err}"
        );
        assert!(err.contains("own container"), "{err}");
        for fix in [
            "Stop the old process",
            "Recreate",
            "DGP_STATE_LOCK_WAIT_SECS",
        ] {
            assert!(err.contains(fix), "{fix}: {err}");
        }
    }

    #[test]
    fn a_lock_covers_only_its_own_directory() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let lock = acquire(a.path(), NO_WAIT, || {}).unwrap();
        assert!(lock.is_held());
        assert!(lock.covers(&a.path().join("deltaglider_config.db")));
        assert!(!lock.covers(&b.path().join("deltaglider_config.db")));
    }

    #[test]
    fn the_holder_line_has_pid_host_and_start() {
        assert_eq!(
            holder_text(1, "3f2a9c", 1_700_000_000),
            "pid 1 on host 3f2a9c, started at unix time 1700000000\n"
        );
        assert!(holder_text(7, "", 5).contains("unknown host"));
    }

    #[test]
    fn a_waiting_holder_takes_the_lock_when_the_first_one_drops() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path(), NO_WAIT, || {}).expect("first lock");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop(first);
        });
        let mut announced = 0;
        acquire(dir.path(), Duration::from_secs(20), || announced += 1)
            .expect("the waiter gets the lock");
        assert_eq!(announced, 1, "the wait is announced once");
        releaser.join().unwrap();
    }

    #[test]
    fn a_waiter_gives_up_at_the_deadline_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let _first = acquire(dir.path(), NO_WAIT, || {}).expect("first lock");
        let err =
            acquire(dir.path(), Duration::from_millis(400), || {}).expect_err("the holder stays");
        assert!(err.contains("did not stop within"), "{err}");
    }

    #[test]
    fn a_file_system_without_locks_is_not_a_refusal() {
        for code in [libc::ENOLCK, libc::EOPNOTSUPP] {
            let e = std::io::Error::from_raw_os_error(code);
            assert_eq!(
                classify(Err(std::fs::TryLockError::Error(e))),
                Attempt::Unsupported
            );
        }
        let e = std::io::Error::from_raw_os_error(libc::EIO);
        assert!(matches!(
            classify(Err(std::fs::TryLockError::Error(e))),
            Attempt::Failed(_)
        ));
    }

    #[test]
    fn a_missing_directory_names_the_lock_file() {
        let err =
            acquire(Path::new("/nonexistent-dgp-dir"), NO_WAIT, || {}).expect_err("no directory");
        assert!(
            err.contains("/nonexistent-dgp-dir/.deltaglider.lock"),
            "{err}"
        );
    }
}
