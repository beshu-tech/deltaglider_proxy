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
//! start.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The lock file name inside the state directory.
pub const LOCK_FILE_NAME: &str = ".deltaglider.lock";

/// Held for the life of the process. Dropping it releases the lock.
#[derive(Debug)]
pub struct DataDirLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl DataDirLock {
    /// The lock file this guard holds.
    pub fn path(&self) -> &Path {
        &self.path
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
            Attempt::Held => return Ok(DataDirLock { _file: file, path }),
            Attempt::Unsupported => {
                tracing::warn!(
                    "the file system of '{}' does not support file locks; this process cannot \
                     tell whether another process uses the state directory",
                    dir.display()
                );
                return Ok(DataDirLock { _file: file, path });
            }
            Attempt::Failed(e) => return Err(format!("cannot lock '{}': {e}", path.display())),
            Attempt::Busy if Instant::now() >= deadline => {
                return Err(format!(
                    "another DeltaGlider Proxy process already uses the state directory '{}' \
                     (lock file '{}'){}. Stop it first, or give this process its own directory \
                     with DGP_CONFIG",
                    dir.display(),
                    path.display(),
                    if wait.is_zero() {
                        String::new()
                    } else {
                        format!(" and did not stop within {} s", wait.as_secs().max(1))
                    }
                ));
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
