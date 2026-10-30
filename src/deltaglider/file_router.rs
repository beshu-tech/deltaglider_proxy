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

/// File extensions (without the dot) that the proxy tries to store as a
/// delta. A file type belongs here when two versions of it share most of
/// their bytes, which is true of uncompressed binaries and of stored-mode
/// archives. Already-compressed formats (`.tar.gz`, `.7z`, media) stay out:
/// a delta of them is about as large as the file. The ratio gate
/// (`max_delta_ratio`) still stores a poor delta whole, so a wrong guess
/// costs codec time, never bytes. A bucket adds or removes types with
/// `delta_extensions` (see [`normalize_extension`]).
pub const DEFAULT_DELTA_EXTENSIONS: &[&str] = &[
    // Archives and their ZIP-based packages
    "zip",
    "tar",
    "jar",
    "war",
    "ear",
    "apk",
    "aab",
    "ipa",
    "whl",
    "nupkg",
    "vsix",
    "xpi",
    // Disk and firmware images
    "dmg",
    "iso",
    "img",
    "bin",
    "raw",
    "fw",
    "rom",
    "vhd",
    "vhdx",
    "vmdk",
    "vdi",
    "qcow2",
    // Executables and libraries
    "exe",
    "dll",
    "so",
    "dylib",
    "wasm",
    // Model weights and checkpoints
    "safetensors",
    "gguf",
    "onnx",
    "ckpt",
    "pt",
    "pth",
    "h5",
    "npy",
    "npz",
    // Database dumps and files, backups
    "sql",
    "dump",
    "sqlite",
    "db",
    "bak",
    "backup",
];

/// Checksum sidecars are stored byte for byte on every backend, and replication
/// parity trusts their listed size without a HEAD. A bucket may never make them
/// delta-eligible.
const VERBATIM_SIDECARS: &[&str] = &["sha1", "sha256", "sha512"];

/// Validate and normalize one entry of a bucket's `delta_extensions` list:
/// lowercase, without a leading dot. Inner dots are allowed (`tar.zst`).
pub fn normalize_extension(raw: &str) -> Result<String, String> {
    let ext = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    if ext.is_empty() {
        return Err(format!("`{raw}` is not a file extension"));
    }
    if ext.len() > 32
        || ext.ends_with('.')
        || ext.contains("..")
        || !ext
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(format!(
            "`{raw}` is not a file extension: use letters, digits, `-`, `_` and inner dots, \
             at most 32 characters (for example `bin` or `tar.zst`)"
        ));
    }
    if VERBATIM_SIDECARS.contains(&ext.as_str()) {
        return Err(format!(
            "`{raw}` cannot be delta-compressed: checksum sidecars are always stored as they are"
        ));
    }
    Ok(ext)
}

/// Routes files to appropriate compression strategy based on extension.
/// Dot-prefixed suffixes are pre-formatted at construction time to avoid
/// per-call allocations in `route()`.
#[derive(Debug, Clone)]
pub struct FileRouter {
    /// Pre-formatted dot-prefixed suffixes (e.g., ".tar", ".zip")
    delta_suffixes: Vec<String>,
}

impl Default for FileRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl FileRouter {
    /// A router over [`DEFAULT_DELTA_EXTENSIONS`].
    pub fn new() -> Self {
        Self::with_overrides(&[], &[])
    }

    /// The default list plus `add`, minus `remove`. Both take normalized
    /// entries ([`normalize_extension`]).
    pub fn with_overrides(add: &[String], remove: &[String]) -> Self {
        let mut exts: Vec<String> = DEFAULT_DELTA_EXTENSIONS
            .iter()
            .map(|e| e.to_string())
            .chain(add.iter().cloned())
            .filter(|e| !remove.contains(e))
            .collect();
        exts.sort();
        exts.dedup();
        Self {
            delta_suffixes: exts.iter().map(|ext| format!(".{}", ext)).collect(),
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
    fn uncompressed_binaries_and_model_weights_are_eligible() {
        let router = FileRouter::new();
        for k in [
            "fw/widget-1.4.0.bin",
            "disk.img",
            "app.exe",
            "libfoo.so",
            "model.safetensors",
            "llama-7b.Q4.gguf",
            "net.onnx",
            "weights.pt",
            "app.apk",
            "pkg-1.0-py3-none-any.whl",
        ] {
            assert!(router.is_delta_eligible(k), "{k}");
        }
    }

    #[test]
    fn overrides_add_and_remove() {
        let router = FileRouter::with_overrides(&["tar.zst".into()], &["sql".into()]);
        assert!(router.is_delta_eligible("a.tar.zst"));
        assert!(!router.is_delta_eligible("dump.sql"));
        assert!(router.is_delta_eligible("app.zip"));
    }

    #[test]
    fn normalize_extension_truth_table() {
        assert_eq!(normalize_extension(".BIN").unwrap(), "bin");
        assert_eq!(normalize_extension(" tar.zst ").unwrap(), "tar.zst");
        for bad in [
            "",
            ".",
            "a/b",
            "a b",
            "a..b",
            "a.",
            "*",
            "sha256",
            ".SHA1",
            &"x".repeat(33),
        ] {
            assert!(normalize_extension(bad).is_err(), "{bad:?}");
        }
    }

    proptest::proptest! {
        /// A normalized extension never holds `/`, so eligibility stays a
        /// property of the last key segment (see `key_parsings_agree_on_eligibility`),
        /// and normalizing twice changes nothing.
        #[test]
        fn normalized_extensions_hold_no_slash(raw in "\\PC{0,40}") {
            if let Ok(ext) = normalize_extension(&raw) {
                proptest::prop_assert!(!ext.contains('/'));
                proptest::prop_assert_eq!(normalize_extension(&ext).unwrap(), ext);
            }
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
