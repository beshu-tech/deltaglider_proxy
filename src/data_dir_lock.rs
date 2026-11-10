// SPDX-License-Identifier: BUSL-1.1

//! One proxy process per state directory.
//!
//! The state directory (the folder of the config DB) holds the SQLite files
//! `deltaglider_config.db`, `deltaglider_jobs.db` and `deltaglider_usage.db`.
//! Two processes on them corrupt the job state and the usage counters. The
//! first process takes an exclusive `flock` on `.deltaglider.lock` in that
//! folder and keeps it until it exits; the kernel drops it on a crash, so a
//! stale file never blocks a restart. A second process refuses to start.

use std::path::{Path, PathBuf};

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

/// Take the exclusive lock of `dir`. The error text names the directory and
/// says what to do.
pub fn acquire(dir: &Path) -> Result<DataDirLock, String> {
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
    lock_exclusive_nonblocking(&file).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            format!(
                "another DeltaGlider Proxy process already uses the state directory '{}' (lock \
                 file '{}'). Stop it first, or give this process its own directory with \
                 DGP_CONFIG",
                dir.display(),
                path.display()
            )
        } else {
            format!("cannot lock '{}': {e}", path.display())
        }
    })?;
    Ok(DataDirLock { _file: file, path })
}

#[cfg(unix)]
fn lock_exclusive_nonblocking(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `flock` on a file descriptor that `file` owns for the whole call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_exclusive_nonblocking(_file: &std::fs::File) -> std::io::Result<()> {
    // No flock here: the guard stays a no-op.
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_with_the_directory_named() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path()).expect("first lock");
        let err = acquire(dir.path()).expect_err("second lock must fail");
        assert!(err.contains("another DeltaGlider Proxy process"), "{err}");
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
        drop(first);
        acquire(dir.path()).expect("the lock is free after the holder drops");
    }

    #[test]
    fn a_missing_directory_names_the_lock_file() {
        let err = acquire(Path::new("/nonexistent-dgp-dir")).expect_err("no directory");
        assert!(
            err.contains("/nonexistent-dgp-dir/.deltaglider.lock"),
            "{err}"
        );
    }
}
