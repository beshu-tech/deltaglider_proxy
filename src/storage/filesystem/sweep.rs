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

use super::is_internal_temp_name;
use std::path::Path;
use std::time::Duration;

/// A temp file untouched for this long is debris. ONE constant: the
/// filesystem sweep and the spool sweep (`SpoolDir::sweep_orphans`) both use it.
pub const STALE_TEMP_MAX_AGE: Duration = Duration::from_secs(3600);

/// THE rule: does a file named `name`, last written `age` ago, go? The name
/// test is the backend's own (`is_internal_temp_name`: `.dg-tmp.*`, and
/// `.tmpXXXXXX` of older releases): both are refused as keys and hidden from
/// LIST, so neither is a user object, and killed writes of older releases
/// do not stay for ever. The refusal as a key is what keeps a client from
/// storing a file that this sweep would delete (test
/// `a_key_with_a_temp_file_name_is_refused`).
pub(crate) fn is_stale_temp(name: &str, age: Duration, max_age: Duration) -> bool {
    is_internal_temp_name(name) && age >= max_age
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
                if age.is_some_and(|a| is_stale_temp(&name.to_string_lossy(), a, max_age))
                    && std::fs::remove_file(entry.path()).is_ok()
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
        let cases = [
            (".dg-tmp.abc", hour, true),
            (".dg-tmp.abc", hour * 5, true),
            (".dg-tmp.abc", hour - Duration::from_secs(1), false),
            (".dg-tmp.abc", Duration::ZERO, false),
            // Temp files of older releases go as well.
            (".tmpAb12Cd", hour * 5, true),
            (".tmpAb12Cd", hour - Duration::from_secs(1), false),
            // A user object, however old, never goes.
            (".env", hour * 5, false),
            ("photo.jpg", hour * 5, false),
            (".tmp", hour * 5, false),
            (".tmpAb12", hour * 5, false),
            (".tmpAb12CdEf", hour * 5, false),
            ("x.dg-tmp.abc", hour * 5, false),
        ];
        for (name, age, want) in cases {
            assert_eq!(is_stale_temp(name, age, hour), want, "{name} aged {age:?}");
        }
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
