//! xdelta3 codec wrapper for delta encoding/decoding
//!
//! Uses the xdelta3 CLI binary for both encoding and decoding to ensure
//! compatibility with deltas created by the original DeltaGlider Python CLI.

use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;
use thiserror::Error;
use tracing::{debug, instrument, warn};

/// Errors that can occur during delta encoding/decoding
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("Delta encoding failed: {0}")]
    EncodeFailed(String),

    #[error("Delta decoding failed: {0}")]
    DecodeFailed(String),

    #[error("Data too large: {size} bytes (max: {max} bytes)")]
    TooLarge { size: usize, max: usize },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Delta codec using the xdelta3 CLI binary
pub struct DeltaCodec {
    max_size: usize,
    /// Whether the xdelta3 CLI binary is available.
    /// Probed once at construction time to avoid per-request discovery failures.
    cli_available: bool,
}

impl DeltaCodec {
    /// Create a new codec with size limit.
    /// Probes for the xdelta3 CLI binary once at construction.
    pub fn new(max_size: usize) -> Self {
        let cli_available = Self::probe_cli();
        Self {
            max_size,
            cli_available,
        }
    }

    /// Check if the xdelta3 CLI binary is available.
    fn probe_cli() -> bool {
        match Command::new("xdelta3").arg("-V").output() {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }

    /// Returns whether the xdelta3 CLI is available.
    pub fn is_cli_available(&self) -> bool {
        self.cli_available
    }

    /// Encode a delta between source (reference) and target (new file)
    /// Returns the delta patch that can transform source into target
    #[instrument(skip(self, source, target))]
    pub fn encode(&self, source: &[u8], target: &[u8]) -> Result<Vec<u8>, CodecError> {
        // Validate sizes
        if source.len() > self.max_size {
            return Err(CodecError::TooLarge {
                size: source.len(),
                max: self.max_size,
            });
        }
        if target.len() > self.max_size {
            return Err(CodecError::TooLarge {
                size: target.len(),
                max: self.max_size,
            });
        }

        debug!(
            "Encoding delta: source={} bytes, target={} bytes",
            source.len(),
            target.len()
        );

        // Write source and target to temporary files
        let mut source_file = NamedTempFile::new()?;
        source_file.write_all(source)?;
        source_file.flush()?;

        let mut target_file = NamedTempFile::new()?;
        target_file.write_all(target)?;
        target_file.flush()?;

        let output_file = NamedTempFile::new()?;
        let output_path = output_file.path().to_owned();

        // Run xdelta3 -e -f -s source target output
        // -e for encode, -f to overwrite output file, -s for source (reference)
        let result = Command::new("xdelta3")
            .args([
                "-e",
                "-f",
                "-s",
                source_file.path().to_str().unwrap(),
                target_file.path().to_str().unwrap(),
                output_path.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        match result {
            Ok(output) => {
                if output.status.success() {
                    let delta = std::fs::read(&output_path)?;
                    debug!(
                        "Delta encoded: {} bytes (ratio: {:.2}%)",
                        delta.len(),
                        (delta.len() as f64 / target.len() as f64) * 100.0
                    );
                    Ok(delta)
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!("xdelta3 CLI encode failed: {}", stderr);
                    Err(CodecError::EncodeFailed(format!(
                        "xdelta3 CLI failed: {}",
                        stderr
                    )))
                }
            }
            Err(e) => {
                warn!("Failed to execute xdelta3 CLI: {}", e);
                Err(CodecError::EncodeFailed(format!(
                    "xdelta3 CLI not available: {}",
                    e
                )))
            }
        }
    }

    /// Decode a delta to reconstruct the target from source + delta
    #[instrument(skip(self, source, delta))]
    pub fn decode(&self, source: &[u8], delta: &[u8]) -> Result<Vec<u8>, CodecError> {
        if source.len() > self.max_size {
            return Err(CodecError::TooLarge {
                size: source.len(),
                max: self.max_size,
            });
        }

        debug!(
            "Decoding delta: source={} bytes, delta={} bytes",
            source.len(),
            delta.len()
        );

        if !self.cli_available {
            return Err(CodecError::DecodeFailed(
                "xdelta3 CLI binary is not available".to_string(),
            ));
        }

        // Write source and delta to temporary files
        let mut source_file = NamedTempFile::new()?;
        source_file.write_all(source)?;
        source_file.flush()?;

        let mut delta_file = NamedTempFile::new()?;
        delta_file.write_all(delta)?;
        delta_file.flush()?;

        let output_file = NamedTempFile::new()?;
        let output_path = output_file.path().to_owned();

        // Run xdelta3 -d -f -s source delta output
        // -f is needed to overwrite the output file created by NamedTempFile
        let result = Command::new("xdelta3")
            .args([
                "-d",
                "-f",
                "-s",
                source_file.path().to_str().unwrap(),
                delta_file.path().to_str().unwrap(),
                output_path.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        match result {
            Ok(output) => {
                if output.status.success() {
                    let target = std::fs::read(&output_path)?;
                    debug!("Delta decoded: {} bytes", target.len());
                    Ok(target)
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!("xdelta3 CLI decode failed: {}", stderr);
                    Err(CodecError::DecodeFailed(format!(
                        "xdelta3 CLI failed: {}",
                        stderr
                    )))
                }
            }
            Err(e) => {
                warn!("Failed to execute xdelta3 CLI: {}", e);
                Err(CodecError::DecodeFailed(format!(
                    "xdelta3 CLI not available: {}",
                    e
                )))
            }
        }
    }

    /// Calculate compression ratio (delta_size / original_size)
    pub fn compression_ratio(original_size: usize, delta_size: usize) -> f32 {
        if original_size == 0 {
            return 1.0;
        }
        delta_size as f32 / original_size as f32
    }
}

impl Default for DeltaCodec {
    fn default() -> Self {
        Self::new(100 * 1024 * 1024) // 100MB default
    }
}

impl std::fmt::Debug for DeltaCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeltaCodec")
            .field("max_size", &self.max_size)
            .field("cli_available", &self.cli_available)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let codec = DeltaCodec::default();

        let source = b"Hello, this is the original file content!";
        let target = b"Hello, this is the modified file content!";

        let delta = codec.encode(source, target).unwrap();
        let reconstructed = codec.decode(source, &delta).unwrap();

        assert_eq!(reconstructed, target);
    }

    #[test]
    fn test_identical_files() {
        let codec = DeltaCodec::default();

        // Use a larger payload so the delta is meaningfully smaller than the original.
        // The xdelta3 CLI has ~50 bytes of header overhead, so tiny inputs may
        // produce a delta larger than the source.
        let data = vec![0x42u8; 1024];
        let delta = codec.encode(&data, &data).unwrap();

        // Delta for identical files should be much smaller than 1 KiB of data
        assert!(delta.len() < data.len());

        let reconstructed = codec.decode(&data, &delta).unwrap();
        assert_eq!(reconstructed, data);
    }

    #[test]
    fn test_compression_ratio() {
        assert_eq!(DeltaCodec::compression_ratio(100, 50), 0.5);
        assert_eq!(DeltaCodec::compression_ratio(100, 100), 1.0);
        assert_eq!(DeltaCodec::compression_ratio(0, 50), 1.0);
    }

    #[test]
    fn test_size_limit() {
        let codec = DeltaCodec::new(100); // 100 byte limit

        let large_data = vec![0u8; 200];
        let result = codec.encode(&large_data, &large_data);

        assert!(matches!(result, Err(CodecError::TooLarge { .. })));
    }

    #[test]
    fn test_decode_corrupted_delta_fails() {
        let codec = DeltaCodec::default();

        let source = b"Hello, this is the original file content!";
        let target = b"Hello, this is the modified file content!";

        let mut delta = codec.encode(source, target).unwrap();
        // Corrupt the delta by flipping bytes
        for byte in delta.iter_mut() {
            *byte = byte.wrapping_add(1);
        }

        let result = codec.decode(source, &delta);
        assert!(result.is_err() || result.unwrap() != target);
    }

    #[test]
    fn test_encode_empty_target() {
        let codec = DeltaCodec::default();

        let source = b"non-empty source content";
        let target = b"";

        let delta = codec.encode(source, target).unwrap();
        let reconstructed = codec.decode(source, &delta).unwrap();
        assert_eq!(reconstructed, target);
    }
}
