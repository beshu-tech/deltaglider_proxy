// SPDX-License-Identifier: BUSL-1.1

//! Pure key helpers shared by the recursive CLI verbs (`cp -r`, `sync`,
//! `rm -r`, `migrate`).
//!
//! [`local_path_for_key`] is the ONLY way a remote key becomes a local
//! path. A key is attacker-controlled data (anyone with write access to
//! the bucket picks it), so the result must always stay under the
//! destination root.

use std::path::{Component, Path, PathBuf};

/// Why a remote key has no safe local path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalPathError {
    /// The key ends with `/` (an S3 "folder" marker). Callers skip it:
    /// local directories are created on demand.
    DirectoryMarker,
    /// The key would escape the destination root or is not portable.
    Unsafe(&'static str),
}

impl std::fmt::Display for LocalPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DirectoryMarker => f.write_str("directory marker"),
            Self::Unsafe(why) => f.write_str(why),
        }
    }
}

/// Pure: map a key (relative to the listed prefix) to a path under
/// `root`. Refuses, on every platform, anything that could resolve
/// outside `root`: empty segments (a leading `/` or `//`), `.` and `..`,
/// backslashes, drive letters (`C:`), and NUL.
pub fn local_path_for_key(root: &Path, rel: &str) -> Result<PathBuf, LocalPathError> {
    if rel.is_empty() {
        return Err(LocalPathError::Unsafe("empty key"));
    }
    if rel.ends_with('/') {
        return Err(LocalPathError::DirectoryMarker);
    }
    let mut out = root.to_path_buf();
    for seg in rel.split('/') {
        match seg {
            "" => return Err(LocalPathError::Unsafe("empty path segment")),
            "." | ".." => return Err(LocalPathError::Unsafe("`.` or `..` path segment")),
            _ => {}
        }
        if seg.contains('\\') {
            return Err(LocalPathError::Unsafe("backslash in key"));
        }
        if seg.contains('\0') {
            return Err(LocalPathError::Unsafe("NUL in key"));
        }
        let b = seg.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return Err(LocalPathError::Unsafe("drive letter in key"));
        }
        // Belt and braces: the platform parser must see exactly one
        // plain component (catches any prefix/root form we missed).
        let mut comps = Path::new(seg).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(_)), None) => out.push(seg),
            _ => {
                return Err(LocalPathError::Unsafe(
                    "key segment is not a plain file name",
                ))
            }
        }
    }
    Ok(out)
}

/// Pure: the listing prefix for a directory-style (recursive) operation.
/// A non-empty prefix gets a trailing `/`, so `releases` selects
/// `releases/…` and never `releases-old/…`.
pub fn dir_prefix(prefix: &str) -> String {
    if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

/// Pure: the part of `key` under `dir` (a [`dir_prefix`] result).
/// `None` when the key is outside the prefix or equals it.
pub fn rel_under<'a>(key: &'a str, dir: &str) -> Option<&'a str> {
    key.strip_prefix(dir).filter(|r| !r.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/tmp/dest")
    }

    #[test]
    fn plain_keys_land_under_root() {
        assert_eq!(
            local_path_for_key(&root(), "a/b/c.zip").unwrap(),
            root().join("a").join("b").join("c.zip")
        );
        assert_eq!(
            local_path_for_key(&root(), "v1.zip").unwrap(),
            root().join("v1.zip")
        );
        // Dots inside a name are fine.
        assert!(local_path_for_key(&root(), "a/..b/c..").is_ok());
    }

    #[test]
    fn escaping_keys_are_refused() {
        for k in [
            "/etc/cron.d/evil",
            "a//etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            "..",
            "a\\..\\..\\x",
            "C:/Windows/x",
            "c:x",
            "a/C:/x",
            "a\0b",
            "",
        ] {
            assert!(
                matches!(
                    local_path_for_key(&root(), k),
                    Err(LocalPathError::Unsafe(_))
                ),
                "{k:?} must be refused"
            );
        }
    }

    #[test]
    fn folder_markers_are_reported_as_such() {
        assert_eq!(
            local_path_for_key(&root(), "dir/"),
            Err(LocalPathError::DirectoryMarker)
        );
    }

    #[test]
    fn dir_prefix_adds_one_slash() {
        assert_eq!(dir_prefix(""), "");
        assert_eq!(dir_prefix("releases"), "releases/");
        assert_eq!(dir_prefix("releases/"), "releases/");
    }

    #[test]
    fn rel_under_is_strict_about_the_directory_boundary() {
        let d = dir_prefix("releases");
        assert_eq!(rel_under("releases/v1.zip", &d), Some("v1.zip"));
        assert_eq!(rel_under("releases-old/v1.zip", &d), None);
        assert_eq!(rel_under("releases/", &d), None);
        assert_eq!(rel_under("a/b", ""), Some("a/b"));
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Whatever the key, an accepted path is under the root and made
        /// only of plain components after it.
        #[test]
        fn accepted_paths_never_leave_the_root(
            key in prop_oneof![
                any::<String>(),
                "[a-zA-Z0-9./\\\\:_-]{0,40}",
                proptest::collection::vec(
                    prop_oneof![
                        Just("..".to_string()),
                        Just(".".to_string()),
                        Just(String::new()),
                        Just("C:".to_string()),
                        "[a-z.\\\\]{1,6}",
                    ],
                    0..6,
                ).prop_map(|v| v.join("/")),
            ]
        ) {
            let root = PathBuf::from("/tmp/dest");
            if let Ok(p) = local_path_for_key(&root, &key) {
                let rest = p.strip_prefix(&root).expect("path must stay under root");
                prop_assert!(rest.components().count() > 0);
                for c in rest.components() {
                    prop_assert!(matches!(c, Component::Normal(_)), "{key:?} -> {p:?}");
                }
            }
        }

        #[test]
        fn rel_under_result_rejoins_to_the_key(
            prefix in "[a-z/]{0,8}",
            key in "[a-z/-]{0,16}",
        ) {
            let d = dir_prefix(&prefix);
            if let Some(rel) = rel_under(&key, &d) {
                prop_assert_eq!(format!("{d}{rel}"), key);
                prop_assert!(d.is_empty() || d.ends_with('/'));
            }
        }
    }
}
