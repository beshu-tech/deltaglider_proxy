//! File type routing for delta compression eligibility

/// Compression strategy based on file type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionStrategy {
    /// File is eligible for delta compression (archives, etc.)
    DeltaEligible,
    /// Store file directly without delta compression
    DirectStore,
}

/// Routes files to appropriate compression strategy based on extension
pub struct FileRouter {
    /// Extensions that benefit from delta compression
    delta_extensions: Vec<&'static str>,
}

impl Default for FileRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl FileRouter {
    /// Create a new file router with default delta-eligible extensions
    pub fn new() -> Self {
        Self {
            delta_extensions: vec![
                // Archives
                "zip", "tar", "tgz", "tar.gz", "tar.bz2", "tar.xz", // Java/JVM packages
                "jar", "war", "ear", // Other archive formats
                "rar", "7z", // Disk images (often similar between versions)
                "dmg", "iso", // Database dumps
                "sql", "dump", // Backups
                "bak", "backup",
            ],
        }
    }

    /// Determine the compression strategy for a file
    pub fn route(&self, filename: &str) -> CompressionStrategy {
        let lower = filename.to_lowercase();

        // Check for compound extensions first (e.g., .tar.gz)
        for ext in &self.delta_extensions {
            if lower.ends_with(&format!(".{}", ext)) {
                return CompressionStrategy::DeltaEligible;
            }
        }

        CompressionStrategy::DirectStore
    }

    /// Check if a file is eligible for delta compression
    pub fn is_delta_eligible(&self, filename: &str) -> bool {
        self.route(filename) == CompressionStrategy::DeltaEligible
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
        assert!(router.is_delta_eligible("backup.tar.gz"));
        assert!(router.is_delta_eligible("data.sql"));
    }

    #[test]
    fn test_direct_store_extensions() {
        let router = FileRouter::new();

        assert!(!router.is_delta_eligible("app.exe"));
        assert!(!router.is_delta_eligible("image.png"));
        assert!(!router.is_delta_eligible("video.mp4"));
        assert!(!router.is_delta_eligible("document.pdf"));
        assert!(!router.is_delta_eligible("data.json"));
    }

    #[test]
    fn test_no_extension() {
        let router = FileRouter::new();
        assert!(!router.is_delta_eligible("README"));
        assert!(!router.is_delta_eligible("Makefile"));
    }
}
