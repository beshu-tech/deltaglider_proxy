//! xdelta3 codec wrapper for delta encoding/decoding

use thiserror::Error;
use tracing::{debug, instrument};

/// Errors that can occur during delta encoding/decoding
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("Delta encoding failed: {0}")]
    EncodeFailed(String),

    #[error("Delta decoding failed: {0}")]
    DecodeFailed(String),

    #[error("Data too large: {size} bytes (max: {max} bytes)")]
    TooLarge { size: usize, max: usize },
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

        // xdelta3::decode(patch_data, old_data) - note the parameter order!
        let target = xdelta3::decode(delta, source)
            .ok_or_else(|| CodecError::DecodeFailed("xdelta3 decoding failed".to_string()))?;

        debug!("Delta decoded: {} bytes", target.len());

        Ok(target)
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
