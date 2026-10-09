// SPDX-License-Identifier: BUSL-1.1

//! Every probe of the filesystem backend: stat, existence, read, open and
//! remove (review B11). One rule decides what an I/O error means: only "no
//! file at this path" ([`absent`]) is an absence. Any other error (EIO,
//! EACCES, ESTALE) is an error. Read as "absent", a failed stat let a PUT
//! write a new delta reference over the live one, a delete reclaim the
//! reference under live deltas, and a DELETE "succeed" without deleting.
//! `filesystem_backend_probes_only_through_fsio` keeps the backend here.

use super::super::io_to_storage_error;
use super::super::traits::{io_error_is_path_type_conflict, StorageError};
use std::io;
use std::path::Path;

/// THE rule: the error means that no file is at this path (ENOENT, or a
/// path component that is a file: ENOTDIR, EISDIR).
pub(super) fn absent(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || io_error_is_path_type_conflict(e)
}

/// The storage error for `e` on the object `what`: an absence is NotFound.
pub(super) fn storage_error(e: io::Error, what: impl FnOnce() -> String) -> StorageError {
    if absent(&e) {
        StorageError::NotFound(what())
    } else {
        io_to_storage_error(e)
    }
}

/// A test's injected fault on `path` (see `fault::fail_io`).
#[cfg(test)]
fn injected(path: &Path) -> Option<io::Error> {
    super::fault::io_fault(path)
}

#[cfg(not(test))]
#[inline(always)]
fn injected(_path: &Path) -> Option<io::Error> {
    None
}

/// The file's metadata; `None` when no file is there.
pub(super) async fn stat(path: &Path) -> Result<Option<std::fs::Metadata>, StorageError> {
    let res = match injected(path) {
        Some(e) => Err(e),
        None => tokio::fs::metadata(path).await,
    };
    match res {
        Ok(m) => Ok(Some(m)),
        Err(e) if absent(&e) => Ok(None),
        Err(e) => Err(io_to_storage_error(e)),
    }
}

pub(super) async fn exists(path: &Path) -> Result<bool, StorageError> {
    Ok(stat(path).await?.is_some())
}

pub(super) async fn dir_exists(path: &Path) -> Result<bool, StorageError> {
    Ok(stat(path).await?.is_some_and(|m| m.is_dir()))
}

/// [`dir_exists`] for blocking code (inside `spawn_blocking`).
pub(super) fn dir_exists_blocking(path: &Path) -> Result<bool, StorageError> {
    let res = match injected(path) {
        Some(e) => Err(e),
        None => std::fs::metadata(path),
    };
    match res {
        Ok(m) => Ok(m.is_dir()),
        Err(e) if absent(&e) => Ok(false),
        Err(e) => Err(io_to_storage_error(e)),
    }
}

pub(super) async fn read(path: &Path) -> io::Result<Vec<u8>> {
    match injected(path) {
        Some(e) => Err(e),
        None => tokio::fs::read(path).await,
    }
}

pub(super) async fn open(path: &Path) -> io::Result<tokio::fs::File> {
    match injected(path) {
        Some(e) => Err(e),
        None => tokio::fs::File::open(path).await,
    }
}

pub(super) async fn remove_file(path: &Path) -> io::Result<()> {
    match injected(path) {
        Some(e) => Err(e),
        None => tokio::fs::remove_file(path).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review B11: the backend probes the disk only through this module,
    /// so no site reads an I/O error as an absence. A line that must
    /// touch the disk directly says why with `fsio-exempt`.
    #[test]
    fn filesystem_backend_probes_only_through_fsio() {
        const RAW: &[&str] = &[
            "try_exists(",
            "unwrap_or(false)",
            ".exists()",
            "_dir.is_dir()",
            "path.is_dir()",
            "if let Ok(md)",
            "fs::metadata(",
            "symlink_metadata(",
            "fs::remove_file(",
            "fs::read(",
            "File::open(",
        ];
        let test_module = regex_lite::Regex::new(r"\n#\[cfg\(test\)\]\nmod \w+ \{").unwrap();
        let mut offenders = Vec::new();
        for file in ["mod.rs", "write.rs"] {
            let path = format!("src/storage/filesystem/{file}");
            let src = std::fs::read_to_string(&path).unwrap();
            let body = test_module.split(&src).next().unwrap();
            let lines: Vec<&str> = body.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                let exempt = line.contains("fsio-exempt")
                    || n.checked_sub(1)
                        .is_some_and(|p| lines[p].contains("fsio-exempt"));
                if exempt || line.trim_start().starts_with("//") {
                    continue;
                }
                for raw in RAW {
                    if line.contains(raw) {
                        offenders.push(format!("{path}:{}: {raw}", n + 1));
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "probe through fsio: {offenders:#?}");
    }

    #[test]
    fn only_no_such_file_is_absent() {
        for errno in [libc::ENOENT, libc::ENOTDIR, libc::EISDIR] {
            assert!(absent(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
        for errno in [
            libc::EIO,
            libc::EACCES,
            libc::ESTALE,
            libc::EPERM,
            libc::ENOSPC,
        ] {
            assert!(!absent(&io::Error::from_raw_os_error(errno)), "{errno}");
        }
    }

    #[tokio::test]
    async fn a_stat_error_is_an_error_and_a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        assert!(stat(&file).await.unwrap().is_none());
        assert!(!dir_exists_blocking(&file).unwrap());
        std::fs::write(&file, b"x").unwrap();
        assert!(exists(&file).await.unwrap());
        // A path under a file is no file.
        assert!(!exists(&file.join("below")).await.unwrap());
        let fault = super::super::fault::fail_io(&file, libc::EIO);
        assert!(exists(&file).await.is_err());
        assert!(dir_exists(&file).await.is_err());
        assert!(dir_exists_blocking(&file).is_err());
        assert_eq!(fault.fired(), 3);
        drop(fault);
        assert!(exists(&file).await.unwrap());
    }
}
