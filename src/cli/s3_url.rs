// SPDX-License-Identifier: GPL-3.0-only

//! Pure `s3://bucket/key` URL parser shared by `cp`, `ls`, `rm`,
//! `stats`, and `verify` CLI subcommands.
//!
//! Matches AWS-CLI behaviour: no percent-decoding, trailing slash on
//! the key preserved (meaningful to `ls` for prefix-vs-object).

/// A parsed `s3://bucket[/key]` location.
///
/// `key` is empty for bucket-only URLs (`s3://bucket` and `s3://bucket/`
/// both yield `key == ""`). A trailing slash on the key path is kept
/// — callers that care about prefix-vs-object semantics inspect it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Loc {
    pub bucket: String,
    pub key: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UrlError {
    #[error("URL must use the s3:// scheme")]
    NotS3Scheme,
    #[error("URL has an empty bucket name")]
    EmptyBucket,
    #[error("bucket name '{0}' is not a valid S3 bucket name")]
    InvalidBucketName(String),
}

/// Cheap prefix check — does this string look like an S3 URL? Used by
/// `cp` to decide upload-vs-download-vs-S3-to-S3 direction without
/// committing to a full parse.
pub fn is_s3_url(s: &str) -> bool {
    s.starts_with("s3://")
}

/// Parse `s3://bucket[/key]` into its components. Returns `Err` for
/// non-`s3://` strings, empty buckets, and IP-shaped bucket names.
pub fn parse_s3_url(s: &str) -> Result<S3Loc, UrlError> {
    let rest = s.strip_prefix("s3://").ok_or(UrlError::NotS3Scheme)?;
    let (bucket, key) = match rest.split_once('/') {
        Some((b, k)) => (b, k),
        None => (rest, ""),
    };
    if bucket.is_empty() {
        return Err(UrlError::EmptyBucket);
    }
    if !is_valid_bucket_name(bucket) {
        return Err(UrlError::InvalidBucketName(bucket.to_string()));
    }
    Ok(S3Loc {
        bucket: bucket.to_string(),
        key: key.to_string(),
    })
}

/// Minimal bucket-name validation that catches the IP-shaped cases the
/// upstream `security::bucket_name_is_ip_like` rejects, plus the basic
/// DNS-label rules. The S3 layer will re-reject anything we let through.
fn is_valid_bucket_name(name: &str) -> bool {
    if name.len() < 3 || name.len() > 63 {
        return false;
    }
    if crate::security::bucket_name_is_ip_like(name) {
        return false;
    }
    // Lowercase letters, digits, dot, dash; must start and end alnum.
    let bytes = name.as_bytes();
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes.iter().all(|&b| alnum(b) || b == b'.' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bucket_only() {
        let loc = parse_s3_url("s3://my-bucket").unwrap();
        assert_eq!(loc.bucket, "my-bucket");
        assert_eq!(loc.key, "");
    }

    #[test]
    fn parses_bucket_with_trailing_slash_as_empty_key() {
        let loc = parse_s3_url("s3://my-bucket/").unwrap();
        assert_eq!(loc.bucket, "my-bucket");
        assert_eq!(loc.key, "");
    }

    #[test]
    fn parses_bucket_with_object_key() {
        let loc = parse_s3_url("s3://my-bucket/releases/v1.zip").unwrap();
        assert_eq!(loc.bucket, "my-bucket");
        assert_eq!(loc.key, "releases/v1.zip");
    }

    #[test]
    fn preserves_trailing_slash_on_key() {
        let loc = parse_s3_url("s3://my-bucket/releases/").unwrap();
        assert_eq!(loc.key, "releases/");
    }

    #[test]
    fn rejects_non_s3_scheme() {
        assert_eq!(
            parse_s3_url("http://bucket/key"),
            Err(UrlError::NotS3Scheme)
        );
        assert_eq!(parse_s3_url("bucket/key"), Err(UrlError::NotS3Scheme));
        assert_eq!(parse_s3_url(""), Err(UrlError::NotS3Scheme));
    }

    #[test]
    fn rejects_empty_bucket() {
        assert_eq!(parse_s3_url("s3://"), Err(UrlError::EmptyBucket));
        assert_eq!(parse_s3_url("s3:///key"), Err(UrlError::EmptyBucket));
    }

    #[test]
    fn rejects_ip_shaped_bucket() {
        match parse_s3_url("s3://127.0.0.1/key") {
            Err(UrlError::InvalidBucketName(_)) => {}
            other => panic!("expected InvalidBucketName, got {other:?}"),
        }
    }

    #[test]
    fn rejects_uppercase_bucket() {
        // S3 forbids uppercase in bucket names; our parser mirrors that
        // so the SDK layer doesn't have to.
        match parse_s3_url("s3://MyBucket/key") {
            Err(UrlError::InvalidBucketName(_)) => {}
            other => panic!("expected InvalidBucketName, got {other:?}"),
        }
    }

    #[test]
    fn is_s3_url_cheap_check() {
        assert!(is_s3_url("s3://foo"));
        assert!(is_s3_url("s3://"));
        assert!(!is_s3_url("local/file"));
        assert!(!is_s3_url("/abs/path"));
        assert!(!is_s3_url("http://foo"));
        assert!(!is_s3_url(""));
    }
}
