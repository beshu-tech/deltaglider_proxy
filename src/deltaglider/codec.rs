//! xdelta3 codec wrapper for delta encoding/decoding
//!
//! Uses the xdelta3 CLI binary for decoding to ensure compatibility with
//! deltas created by the original DeltaGlider Python CLI, and the Rust
//! xdelta3 crate for encoding (which produces CLI-compatible deltas).

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

/// Delta codec using xdelta3
pub struct DeltaCodec {
    max_size: usize,
}

impl DeltaCodec {
    /// Create a new codec with size limit
    pub fn new(max_size: usize) -> Self {
        Self { max_size }
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

        // xdelta3::encode(new_data, old_data) - note the parameter order!
        let delta = xdelta3::encode(target, source)
            .ok_or_else(|| CodecError::EncodeFailed("xdelta3 encoding failed".to_string()))?;

        debug!(
            "Delta encoded: {} bytes (ratio: {:.2}%)",
            delta.len(),
            (delta.len() as f64 / target.len() as f64) * 100.0
        );

        Ok(delta)
    }

    /// Decode a delta to reconstruct the target from source + delta
    /// Uses the xdelta3 CLI binary for compatibility with original DeltaGlider CLI
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

        // Try Rust crate first (faster, works for deltas we created)
        if let Some(target) = xdelta3::decode(delta, source) {
            debug!("Delta decoded via Rust crate: {} bytes", target.len());
            return Ok(target);
        }

        // Fallback to CLI for compatibility with original DeltaGlider CLI deltas
        debug!("Rust crate decode failed, falling back to xdelta3 CLI");
        self.decode_via_cli(source, delta)
    }

    /// Decode using the xdelta3 CLI binary
    /// This handles deltas created by the original DeltaGlider Python CLI
    fn decode_via_cli(&self, source: &[u8], delta: &[u8]) -> Result<Vec<u8>, CodecError> {
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
                    debug!("Delta decoded via CLI: {} bytes", target.len());
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

        let data = b"Same content in both files";
        let delta = codec.encode(data, data).unwrap();

        // Delta for identical files should be very small
        assert!(delta.len() < data.len());

        let reconstructed = codec.decode(data, &delta).unwrap();
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
}
