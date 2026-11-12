// SPDX-License-Identifier: BUSL-1.1

//! Startup sweep of stale `.dg-tmp.*` files (epic #145, F22).
//!
//! An atomic write puts its bytes in `.dg-tmp.*` and renames it. A process
//! that dies in between (SIGKILL, OOM, power loss) never runs the temp
//! file's destructor, so the file stays for ever. LIST does not show it
//! (`is_internal_temp_name`), but it holds disk space.
//!
//! The state-directory lock covers the config DB folder, not the data
//! root: another process may share this root. So the rule is the one of the
//! spool's orphan sweep (same [`STALE_TEMP_MAX_AGE`]): only a temp file that
//! nobody wrote to for that long goes. A live writer touches its file all the time
//! and holds it for seconds, never for an hour.

use super::{is_internal_temp_name, INTERNAL_TEMP_PREFIX};
use std::path::Path;
use std::time::Duration;

/// A temp file untouched for this long is debris. ONE constant: the
/// filesystem sweep and the spool sweep (`SpoolDir::sweep_orphans`) both use it.
pub const STALE_TEMP_MAX_AGE: Duration = Duration::from_secs(3600);

/// THE rule: does a file named `name`, last written `age` ago, go?
///
/// `.dg-tmp.*` is this backend's own name space: no release stored a client
/// object under it, so age alone decides. `.tmpXXXXXX` is the temp name of
/// releases before 2.0, and those releases also accepted it as a key, so a
/// file with that name can be a client object. An object that the proxy
/// stored carries the DG metadata xattr; a write that died before its
/// rename mostly does not. So such a file goes only when it is old AND
/// carries no metadata (`carries_metadata` is asked only then; "cannot
/// tell" must answer `true`). A dead write that already had its metadata
/// stays: it costs disk space, never an object.
pub(crate) fn is_stale_temp(
    name: &str,
    age: Duration,
    max_age: Duration,
    carries_metadata: impl FnOnce() -> bool,
) -> bool {
    if !is_internal_temp_name(name) || age < max_age {
        return false;
    }
    name.starts_with(INTERNAL_TEMP_PREFIX) || !carries_metadata()
}

/// Does the file carry the DG metadata xattr? An error (no xattr support,
/// no permission) counts as yes: the sweep then keeps the file.
fn carries_metadata(path: &Path) -> bool {
    !matches!(
        xattr::get(path, crate::storage::xattr_meta::XATTR_NAME),
        Ok(None)
    )
}

/// Remove the stale temp files below `root`. Blocking: the caller runs it
/// on the blocking pool. Best effort: an unreadable folder is skipped.
/// Returns the number of files removed.
pub fn sweep_stale_temp_files(root: &Path, max_age: Duration) -> u64 {
    let mut removed = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                let age = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.elapsed().ok());
                let name = entry.file_name();
                let path = entry.path();
                if age.is_some_and(|a| {
                    is_stale_temp(&name.to_string_lossy(), a, max_age, || {
                        carries_metadata(&path)
                    })
                }) && std::fs::remove_file(&path).is_ok()
                {
                    removed += 1;
                }
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule_table() {
        let hour = Duration::from_secs(3600);
        // (name, age, carries metadata, goes)
        let cases = [
            (".dg-tmp.abc", hour, false, true),
            (".dg-tmp.abc", hour * 5, false, true),
            // Our own name space: the metadata does not matter.
            (".dg-tmp.abc", hour * 5, true, true),
            (".dg-tmp.abc", hour - Duration::from_secs(1), false, false),
            (".dg-tmp.abc", Duration::ZERO, false, false),
            // A temp file of an older release goes when it has no metadata.
            (".tmpAb12Cd", hour * 5, false, true),
            (".tmpAb12Cd", hour - Duration::from_secs(1), false, false),
            // With metadata it can be an object that an older release
            // stored: it never goes.
            (".tmpAb12Cd", hour * 5, true, false),
            // Any other name, however old, never goes.
            (".env", hour * 5, false, false),
            ("photo.jpg", hour * 5, false, false),
            (".tmp", hour * 5, false, false),
            (".tmpAb12", hour * 5, false, false),
            (".tmpAb12CdEf", hour * 5, false, false),
            ("x.dg-tmp.abc", hour * 5, false, false),
        ];
        for (name, age, meta, want) in cases {
            assert_eq!(
                is_stale_temp(name, age, hour, || meta),
                want,
                "{name} aged {age:?}, metadata {meta}"
            );
        }
    }

    /// The metadata is read only for an old file with the older temp name.
    #[test]
    fn the_metadata_is_asked_only_when_it_decides() {
        let hour = Duration::from_secs(3600);
        let never = || -> bool { panic!("asked") };
        assert!(is_stale_temp(".dg-tmp.abc", hour, hour, never));
        assert!(!is_stale_temp(".tmpAb12Cd", Duration::ZERO, hour, never));
        assert!(!is_stale_temp("photo.jpg", hour, hour, never));
    }

    /// An object that a release before 2.0 stored under the key `.tmpAb12Cd`
    /// has that file name and the DG metadata xattr. The sweep keeps it,
    /// and removes a dead write of the same name shape (no metadata).
    #[test]
    fn an_object_with_an_older_temp_name_survives_the_sweep() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("releases/deltaspaces/builds");
        std::fs::create_dir_all(&dir).unwrap();
        let object = dir.join(".tmpAb12Cd");
        std::fs::write(&object, b"client bytes").unwrap();
        xattr::set(&object, crate::storage::xattr_meta::XATTR_NAME, b"{}")
            .expect("the test file system has user xattrs");
        std::fs::write(dir.join(".tmpZz99Yy"), b"dead write").unwrap();
        assert_eq!(sweep_stale_temp_files(root.path(), Duration::ZERO), 1);
        assert_eq!(std::fs::read(&object).unwrap(), b"client bytes");
        assert!(!dir.join(".tmpZz99Yy").exists());
    }

    #[test]
    fn the_sweep_removes_only_stale_temp_files_at_any_depth() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("b/deltaspaces/x/y");
        std::fs::create_dir_all(&deep).unwrap();
        for name in [".dg-tmp.old", ".tmpAb12Cd", "keep.bin", ".env"] {
            std::fs::write(deep.join(name), b"x").unwrap();
        }
        std::fs::write(root.path().join(".dg-tmp.top"), b"x").unwrap();
        // Zero age: every temp file is stale.
        assert_eq!(sweep_stale_temp_files(root.path(), Duration::ZERO), 3);
        assert!(!deep.join(".dg-tmp.old").exists());
        assert!(!root.path().join(".dg-tmp.top").exists());
        assert!(deep.join("keep.bin").exists() && deep.join(".env").exists());
        // A fresh file survives the real threshold.
        std::fs::write(deep.join(".dg-tmp.live"), b"x").unwrap();
        assert_eq!(sweep_stale_temp_files(root.path(), STALE_TEMP_MAX_AGE), 0);
        assert!(deep.join(".dg-tmp.live").exists());
    }
}
