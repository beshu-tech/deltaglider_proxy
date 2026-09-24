// SPDX-License-Identifier: BUSL-1.1

//! The S3 request target, decoded exactly as s3s decodes it.
//!
//! s3s routes on the percent-DECODED path (`urlencoding::decode` of the whole
//! path, so `%2F` is a separator and `%74` is `t`) and on `serde_urlencoded`
//! query pairs (keys decoded too, `+` is a space). Every decision taken
//! BEFORE s3s — admission, IAM authorization, the maintenance and backend
//! health gates, SigV4 presigned detection — must read the same bucket, key
//! and query. Otherwise one encoded character makes the policy check a
//! different resource from the one s3s serves.
//!
//! THE one parser: call sites never split or decode `uri().path()` /
//! `uri().query()` themselves.

/// The path could not be percent-decoded to UTF-8. s3s answers such a
/// request with `InvalidURI`, so no handler ever serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUri;

/// A request target as s3s sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTarget {
    /// Percent-decoded path, leading `/` kept.
    pub path: String,
    /// Decoded query pairs in request order.
    pub query: Vec<(String, String)>,
}

impl RequestTarget {
    /// Decode a raw path and optional raw query string.
    pub fn parse(raw_path: &str, raw_query: Option<&str>) -> Result<Self, InvalidUri> {
        let path = urlencoding::decode(raw_path)
            .map_err(|_| InvalidUri)?
            .into_owned();
        let query = match raw_query {
            Some(q) if !q.is_empty() => {
                serde_urlencoded::from_str::<Vec<(String, String)>>(q).map_err(|_| InvalidUri)?
            }
            _ => Vec::new(),
        };
        Ok(Self { path, query })
    }

    pub fn from_uri(uri: &axum::http::Uri) -> Result<Self, InvalidUri> {
        Self::parse(uri.path(), uri.query())
    }

    /// Path-style `(bucket, key)`: the first segment after the leading `/`,
    /// and everything after the next `/` (empty for bucket-level requests).
    pub fn bucket_and_key(&self) -> (&str, &str) {
        let trimmed = self.path.trim_start_matches('/');
        trimmed.split_once('/').unwrap_or((trimmed, ""))
    }

    /// The bucket segment, or `None` for the service root (`/`).
    pub fn bucket(&self) -> Option<&str> {
        let (bucket, _) = self.bucket_and_key();
        (!bucket.is_empty()).then_some(bucket)
    }

    /// First value of query parameter `name`. s3s rejects a duplicated
    /// parameter, so the first value is the only one a handler can see.
    pub fn query_value(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn has_query(&self, name: &str) -> bool {
        self.query.iter().any(|(k, _)| k == name)
    }

    /// `has_query`, ignoring ASCII case of the parameter name.
    pub fn has_query_ignore_case(&self, name: &str) -> bool {
        self.query.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_separator_splits_bucket_and_key() {
        let t = RequestTarget::parse("/bucket%2Fsecret.txt", None).unwrap();
        assert_eq!(t.bucket_and_key(), ("bucket", "secret.txt"));
    }

    #[test]
    fn encoded_characters_decode_in_bucket_and_key() {
        let t = RequestTarget::parse("/pro%64/secre%74.txt", None).unwrap();
        assert_eq!(t.bucket_and_key(), ("prod", "secret.txt"));
    }

    #[test]
    fn plus_in_path_is_literal() {
        let t = RequestTarget::parse("/b/a+b.txt", None).unwrap();
        assert_eq!(t.bucket_and_key(), ("b", "a+b.txt"));
    }

    #[test]
    fn bucket_level_and_root() {
        let t = RequestTarget::parse("/bucket", None).unwrap();
        assert_eq!(t.bucket_and_key(), ("bucket", ""));
        assert_eq!(t.bucket(), Some("bucket"));
        let t = RequestTarget::parse("/bucket/", None).unwrap();
        assert_eq!(t.bucket_and_key(), ("bucket", ""));
        let t = RequestTarget::parse("/", None).unwrap();
        assert_eq!(t.bucket(), None);
    }

    #[test]
    fn query_keys_and_values_decode_like_s3s() {
        let t =
            RequestTarget::parse("/b", Some("list-type=2&%70refix=secret+x%2Fy&delete")).unwrap();
        assert_eq!(t.query_value("prefix"), Some("secret x/y"));
        assert_eq!(t.query_value("list-type"), Some("2"));
        assert!(t.has_query("delete"));
        assert!(!t.has_query("delimiter"));
    }

    #[test]
    fn presigned_parameter_name_may_be_encoded() {
        let t = RequestTarget::parse("/b/k", Some("%58-Amz-Credential=AK%2F20260101")).unwrap();
        assert!(t.has_query("X-Amz-Credential"));
        assert!(t.has_query_ignore_case("x-amz-credential"));
    }

    #[test]
    fn invalid_utf8_after_decoding_is_rejected() {
        assert_eq!(RequestTarget::parse("/b/%ff", None), Err(InvalidUri));
    }

    /// The pre-s3s decision points decode the request target only through
    /// this module. A hand-rolled decode or query split there re-opens the
    /// gap between the resource the policy checks and the one s3s serves.
    #[test]
    fn pre_s3s_decision_points_do_not_decode_by_hand() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Built at runtime so this test's own source is no hit.
        let needles = [
            ["urlencoding::", "decode("].concat(),
            [".split('", "&')"].concat(),
        ];
        for file in [
            "api/auth.rs",
            "iam/middleware.rs",
            "admission/middleware.rs",
            "maintenance/gate.rs",
            "coordination/health.rs",
        ] {
            let text = std::fs::read_to_string(root.join(file)).unwrap();
            for needle in &needles {
                assert!(
                    !text.contains(needle.as_str()),
                    "{file} decodes the request target by hand ({needle}); use RequestTarget"
                );
            }
        }
    }

    #[test]
    fn multibyte_after_percent_does_not_panic() {
        // S20: the old hand-rolled decoder sliced mid-character here.
        let t = RequestTarget::parse("/b/%a\u{e9}", Some("k=%a\u{e9}"));
        assert!(t.is_ok() || t == Err(InvalidUri));
    }
}
