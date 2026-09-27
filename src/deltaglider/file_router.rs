// SPDX-License-Identifier: BUSL-1.1

//! File type routing for delta compression eligibility

/// Compression strategy based on file type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionStrategy {
    /// File is eligible for delta compression (archives, etc.)
    DeltaEligible,
    /// Store file directly without delta compression
    DirectStore,
}

/// Routes files to appropriate compression strategy based on extension.
/// Dot-prefixed suffixes are pre-formatted at construction time to avoid
/// per-call allocations in `route()`.
pub struct FileRouter {
    /// Pre-formatted dot-prefixed suffixes (e.g., ".tar.gz", ".zip")
    delta_suffixes: Vec<String>,
}

impl Default for FileRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl FileRouter {
    /// Create a new file router with default delta-eligible extensions
    pub fn new() -> Self {
        let extensions: &[&str] = &[
            // Containers that are often delta-friendly in byte-exact mode
            "zip", "tar", // Java/JVM packages
            "jar", "war", "ear", // Disk images (often similar between versions)
            "dmg", "iso", // Database dumps
            "sql", "dump", // Backups
            "bak", "backup",
        ];
        Self {
            delta_suffixes: extensions.iter().map(|ext| format!(".{}", ext)).collect(),
        }
    }

    /// Determine the compression strategy for a file
    pub fn route(&self, filename: &str) -> CompressionStrategy {
        let lower = filename.to_lowercase();

        for suffix in &self.delta_suffixes {
            if lower.ends_with(suffix) {
                return CompressionStrategy::DeltaEligible;
            }
        }

        CompressionStrategy::DirectStore
    }

    /// Whether an object key (or a bare filename) is eligible for delta
    /// compression. No suffix holds a `/`, so the whole key and its last
    /// segment give the same answer: callers pass the key, never a split.
    pub fn is_delta_eligible(&self, key: &str) -> bool {
        self.route(key) == CompressionStrategy::DeltaEligible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delta_eligible_extensions() {
        let router = FileRouter::new();

        assert!(router.is_delta_eligible("app.zip"));
        assert!(router.is_delta_eligible("app.ZIP")); // case insensitive
        assert!(router.is_delta_eligible("app.jar"));
        assert!(router.is_delta_eligible("backup.tar"));
        assert!(router.is_delta_eligible("alpine.iso"));
        assert!(router.is_delta_eligible("data.sql"));
    }

    #[test]
    fn test_passthrough_store_extensions() {
        let router = FileRouter::new();

        assert!(!router.is_delta_eligible("app.exe"));
        assert!(!router.is_delta_eligible("image.png"));
        assert!(!router.is_delta_eligible("video.mp4"));
        assert!(!router.is_delta_eligible("document.pdf"));
        assert!(!router.is_delta_eligible("data.json"));
        assert!(!router.is_delta_eligible("backup.tar.gz"));
        assert!(!router.is_delta_eligible("release.tar.xz"));
        assert!(!router.is_delta_eligible("archive.tgz"));
        assert!(!router.is_delta_eligible("bundle.tar.bz2"));
        assert!(!router.is_delta_eligible("backup.rar"));
        assert!(!router.is_delta_eligible("snapshot.7z"));
    }

    /// N11: the old helpers split the key two ways (`ObjectKey::parse`'s
    /// filename, and `rsplit('/')`). A suffix never holds `/`, so both splits
    /// and the whole key give the same answer: the router takes the key as is.
    #[test]
    fn key_parsings_agree_on_eligibility() {
        let router = FileRouter::new();
        let by_parse =
            |k: &str| router.is_delta_eligible(&crate::types::ObjectKey::parse("_", k).filename);
        let by_rsplit = |k: &str| router.is_delta_eligible(k.rsplit('/').next().unwrap_or(k));
        for k in [
            "",
            "/",
            "//",
            "app.zip",
            "/app.zip",
            "//app.zip",
            "releases/v1/app.zip",
            "releases//app.ZIP",
            "releases.zip/",
            "releases.zip/readme",
            "a.zip/b.tar",
            "a/.zip",
            "a/b.zip.sha1",
            "a/b.tar.gz",
            ".zip",
            "x/",
        ] {
            let whole = router.is_delta_eligible(k);
            assert_eq!(whole, by_parse(k), "ObjectKey::parse disagrees: {k:?}");
            assert_eq!(whole, by_rsplit(k), "rsplit disagrees: {k:?}");
        }
    }

    proptest::proptest! {
        #[test]
        fn key_parsings_agree_on_any_key(k in "[a-zA-Z./]{0,24}") {
            let router = FileRouter::new();
            let parsed = crate::types::ObjectKey::parse("_", &k).filename;
            let split = k.rsplit('/').next().unwrap_or(&k);
            let whole = router.is_delta_eligible(&k);
            proptest::prop_assert_eq!(whole, router.is_delta_eligible(&parsed));
            proptest::prop_assert_eq!(whole, router.is_delta_eligible(split));
        }
    }

    #[test]
    fn test_no_extension() {
        let router = FileRouter::new();
        assert!(!router.is_delta_eligible("README"));
        assert!(!router.is_delta_eligible("Makefile"));
    }

    #[test]
    fn checksum_sidecars_are_never_delta_eligible() {
        // Parity's is_verbatim_sidecar skips HEADing these on the assumption they
        // are stored verbatim. If a future eligibility change made a sidecar
        // delta-eligible, that skip would read the wrong (delta) size — fail here.
        let router = FileRouter::new();
        for k in ["x.sha1", "x.sha256", "x.sha512", "app-1.2.3.zip.sha1"] {
            assert!(
                !router.is_delta_eligible(k),
                "sidecar must be verbatim: {k}"
            );
        }
    }
}
