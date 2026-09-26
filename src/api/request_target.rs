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

    /// Path-style `(bucket, key)`, as the engine resolves them.
    ///
    /// The bucket is the segment after the ONE leading `/` (s3s strips one;
    /// `//b/k` has an empty bucket, which s3s refuses). The key is everything
    /// after the next `/`, with its leading slashes removed: s3s passes
    /// `/b//k` on as key `/k`, and `ObjectKey::parse` serves it as `k`. So the
    /// policy must check `k` too, or `GET /b//secret` escapes a Deny on
    /// `b/secret*`.
    pub fn bucket_and_key(&self) -> (&str, &str) {
        let path = self.path.strip_prefix('/').unwrap_or(&self.path);
        let (bucket, key) = path.split_once('/').unwrap_or((path, ""));
        (bucket, key.trim_start_matches('/'))
    }

    /// The bucket, when s3s parses the path as bucket-level (`/b` or `/b/`,
    /// decoded). `/b//` is an object request for s3s (key `/`), so `None`.
    pub fn bucket_only(&self) -> Option<&str> {
        let rest = self.path.strip_prefix('/')?;
        let (bucket, tail) = rest.split_once('/').unwrap_or((rest, ""));
        (!bucket.is_empty() && tail.is_empty()).then_some(bucket)
    }

    /// The bucket segment, or `None` for the service root (`/`).
    pub fn bucket(&self) -> Option<&str> {
        let (bucket, _) = self.bucket_and_key();
        (!bucket.is_empty()).then_some(bucket)
    }

    /// First value of query parameter `name`. s3s refuses a duplicated
    /// parameter when it parses it as a handler argument (`prefix`,
    /// `delimiter`, `max-keys`, …), so for those the first value is the only
    /// one a handler can see. s3s ROUTING instead treats a duplicate as absent
    /// (`get_unique`); do not use this for a routing parameter.
    pub fn query_value(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn has_query(&self, name: &str) -> bool {
        self.query.iter().any(|(k, _)| k == name)
    }

    /// Whether s3s treats this request as SigV4-presigned: it selects the
    /// presigned path on the `X-Amz-Signature` parameter (name decoded,
    /// case-sensitive). THE one presigned test for every pre-s3s decision.
    pub fn is_presigned_v4(&self) -> bool {
        self.has_query("X-Amz-Signature")
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
    fn leading_slashes_of_the_key_are_dropped_like_the_engine() {
        let t = RequestTarget::parse("/b//secret.txt", None).unwrap();
        assert_eq!(t.bucket_and_key(), ("b", "secret.txt"));
        let t = RequestTarget::parse("/b/%2F%2Fsecret.txt", None).unwrap();
        assert_eq!(t.bucket_and_key(), ("b", "secret.txt"));
        // s3s strips ONE leading slash: `//b/k` has an empty bucket.
        let t = RequestTarget::parse("//b/k", None).unwrap();
        assert_eq!(t.bucket_and_key(), ("", "b/k"));
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
    fn bucket_only_matches_the_s3s_bucket_path() {
        let only = |p: &str| {
            RequestTarget::parse(p, None)
                .unwrap()
                .bucket_only()
                .map(str::to_string)
        };
        assert_eq!(only("/b").as_deref(), Some("b"));
        assert_eq!(only("/b/").as_deref(), Some("b"));
        assert_eq!(only("/pro%64").as_deref(), Some("prod"));
        assert_eq!(only("//b"), None);
        assert_eq!(only("/b//"), None);
        assert_eq!(only("/b%2F"), Some("b".to_string()));
        assert_eq!(only("/b%2Fk"), None);
        assert_eq!(only("/"), None);
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
    fn presigned_is_keyed_on_the_signature_parameter_like_s3s() {
        let t =
            RequestTarget::parse("/b/k", Some("X-Amz-Credential=AK&%58-Amz-Signature=ab")).unwrap();
        assert!(
            t.is_presigned_v4(),
            "an encoded parameter NAME is still the parameter"
        );
        let t = RequestTarget::parse("/b/k", Some("X-Amz-Algorithm=AWS4-HMAC-SHA256")).unwrap();
        assert!(!t.is_presigned_v4());
        let t = RequestTarget::parse("/b/k", Some("x-amz-signature=ab")).unwrap();
        assert!(
            !t.is_presigned_v4(),
            "s3s matches the name case-sensitively"
        );
        let t = RequestTarget::parse("/b/k", Some("foo=X-Amz-Signature")).unwrap();
        assert!(!t.is_presigned_v4());
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
        let decode = ["urlencoding::", "decode("].concat();
        let query_split = [".split('", "&')"].concat();
        // Splitting a path into bucket/key by hand (the raw-path bug class).
        let path_split = ["split_once('", "/')"].concat();
        let path_trim = ["trim_start_matches('", "/')"].concat();
        let path_split_all = [".split('", "/')"].concat();
        let path_trim_both = ["trim_matches('", "/')"].concat();
        let all = [
            decode.clone(),
            query_split.clone(),
            path_split,
            path_trim,
            path_split_all,
            path_trim_both,
        ];
        // `api/auth.rs` also splits SigV4 credential scopes on `/`, which is
        // not a request path, so it gets only the decode/query needles.
        let auth_only = [decode, query_split];
        for (file, needles) in [
            ("api/auth.rs", &auth_only[..]),
            // Splits form fields and credential scopes on `/`, not paths.
            ("api/handlers/form_post.rs", &auth_only[..]),
            ("iam/middleware.rs", &all[..]),
            ("admission/middleware.rs", &all[..]),
            ("maintenance/gate.rs", &all[..]),
            ("coordination/health.rs", &all[..]),
        ] {
            let text = std::fs::read_to_string(root.join(file)).unwrap();
            for needle in needles {
                assert!(
                    !text.contains(needle.as_str()),
                    "{file} decodes the request target by hand ({needle}); use RequestTarget"
                );
            }
        }
    }

    #[test]
    fn multibyte_after_percent_is_kept_literally() {
        // The old hand-rolled decoder sliced mid-character here and panicked.
        // An incomplete escape stays literal text, as s3s keeps it.
        let t = RequestTarget::parse("/b/%a\u{e9}", Some("k=%a\u{e9}")).unwrap();
        assert_eq!(t.bucket_and_key(), ("b", "%a\u{e9}"));
        assert_eq!(t.query_value("k"), Some("%a\u{e9}"));
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Pre-auth input: the decoder never panics, whatever the bytes.
        #[test]
        fn parse_never_panics(path in "\\PC{0,64}", query in proptest::option::of("\\PC{0,64}")) {
            if let Ok(t) = RequestTarget::parse(&path, query.as_deref()) {
                let _ = t.bucket_and_key();
                let _ = t.bucket();
                let _ = t.is_presigned_v4();
            }
        }

        /// Percent escapes of arbitrary bytes, mixed with multi-byte text.
        #[test]
        fn parse_never_panics_on_escapes(parts in proptest::collection::vec(
            prop_oneof![
                any::<u8>().prop_map(|b| format!("%{b:02x}")),
                Just("%".to_string()),
                Just("%a".to_string()),
                "[a-z\u{e9}\u{4e2d}/]{0,4}",
            ],
            0..16,
        )) {
            let s: String = parts.concat();
            let _ = RequestTarget::parse(&format!("/{s}"), Some(&s));
        }
    }
}
