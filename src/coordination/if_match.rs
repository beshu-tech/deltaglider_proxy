// SPDX-License-Identifier: BUSL-1.1

//! THE one place that puts an ETag into an `If-Match` header.
//!
//! S3 returns ETags quoted (`"abc"`), and AWS and MinIO compare `If-Match`
//! against that quoted form. Ceph RGW (Hetzner Object Storage, hel1,
//! measured 2026-11-01) compares the `If-Match` of a PUT against the BARE
//! ETag: the correct ETag, quoted as HEAD returned it, gets 412; the same
//! ETag without quotes gets 200. The same backend unquotes `If-Match` on
//! GET. It also ignores `If-Match` on DELETE (a wrong ETag deletes).
//!
//! So each backend definition has a dialect ([`IfMatchDialect`]), which the
//! CAS probe ([`super::cas_probe::probe_cas`]) finds, and a flag for
//! whether DELETE enforces the condition. Every conditional request sets its
//! ETag through [`IfMatchRequest::if_match_etag`]; the source guard
//! `if_match_headers_go_through_one_helper` (src/lib.rs) refuses a raw
//! `.if_match(` or `.copy_source_if_match(` anywhere else.
//!
//! The data backends get their profile from the capability probe
//! ([`record`] / [`lookup`], indexed by the backend definition's
//! fingerprint). The coordination clients (lease, reference lock, config
//! sync) get it from the coordination-bucket validation.

use std::collections::HashMap;

/// How a backend wants the ETag of an `If-Match` (and
/// `x-amz-copy-source-if-match`) header.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum IfMatchDialect {
    /// `"etag"`, as S3 returns it (AWS, MinIO). The default: a verdict or a
    /// witness from before the dialect existed was made by a probe that
    /// sent the quoted form.
    #[default]
    Quoted,
    /// `etag` with no quotes (Ceph RGW on PUT: Hetzner Object Storage).
    Unquoted,
}

impl IfMatchDialect {
    /// Pure: the header value for `etag` (quoted or bare, as the backend
    /// returned it). `*` passes through.
    pub fn header_value(self, etag: &str) -> String {
        let etag = etag.trim();
        if etag == "*" {
            return etag.to_string();
        }
        let bare = etag
            .strip_prefix('"')
            .and_then(|e| e.strip_suffix('"'))
            .unwrap_or(etag);
        match self {
            IfMatchDialect::Quoted => format!("\"{bare}\""),
            IfMatchDialect::Unquoted => bare.to_string(),
        }
    }

    /// The dialect's name, for logs.
    pub fn label(self) -> &'static str {
        match self {
            IfMatchDialect::Quoted => "quoted",
            IfMatchDialect::Unquoted => "unquoted",
        }
    }
}

/// The conditional-request behaviour of one backend definition, as its CAS
/// probe found it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConditionalProfile {
    /// The form of the ETag in `If-Match`.
    pub if_match: IfMatchDialect,
    /// DELETE enforces `If-Match` (a wrong ETag gets 412). When false, a
    /// delete that must not remove a newer object reads it first.
    pub conditional_delete: bool,
}

/// What the process knows about the coordination bucket's conditional
/// writes: nothing yet, or a proven profile. ONE cell is shared by every
/// coordination writer (job lease, reference lock, encryption witness,
/// config sync) and read per request, so the background re-probe that
/// proves the bucket after a boot with an unknown verdict reaches all of
/// them. A writer with no proven profile refuses to write (see
/// [`ProfileCell::proven`]): a conditional write on a guess is the
/// silent-clobber risk the boot probe exists to rule out.
#[derive(Debug, Clone, Default)]
pub struct ProfileCell(std::sync::Arc<parking_lot::RwLock<Option<ConditionalProfile>>>);

impl ProfileCell {
    /// A cell with no verdict yet: its writers fail closed.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// A cell that holds `profile` already.
    pub fn with(profile: ConditionalProfile) -> Self {
        let cell = Self::default();
        cell.prove(profile);
        cell
    }

    /// Record the profile that a probe proved. Every holder of this cell
    /// sees it from its next request on.
    pub fn prove(&self, profile: ConditionalProfile) {
        *self.0.write() = Some(profile);
    }

    /// The proven profile, `None` while the verdict is unknown.
    pub fn get(&self) -> Option<ConditionalProfile> {
        *self.0.read()
    }

    /// The proven profile, or the refusal that a writer returns (its error
    /// type wraps the text; the request path maps it to a retryable 503).
    pub fn proven(&self) -> Result<ConditionalProfile, &'static str> {
        self.get().ok_or(NOT_PROVEN)
    }
}

/// Why a coordination writer refuses while the verdict is unknown.
pub const NOT_PROVEN: &str =
    "the coordination bucket is not proven to enforce conditional writes yet; retry later";

/// Set an ETag condition on a request in the backend's dialect. The only
/// code that calls the SDK's `if_match` / `copy_source_if_match`.
pub trait IfMatchRequest: Sized {
    /// The request with `If-Match: etag` (or, on a copy, the source's
    /// `x-amz-copy-source-if-match: etag`) in `dialect`.
    fn if_match_etag(self, etag: &str, dialect: IfMatchDialect) -> Self;
}

macro_rules! if_match_on {
    ($builder:ty, $method:ident) => {
        impl IfMatchRequest for $builder {
            fn if_match_etag(self, etag: &str, dialect: IfMatchDialect) -> Self {
                self.$method(dialect.header_value(etag))
            }
        }
    };
}

if_match_on!(
    aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    if_match
);
if_match_on!(
    aws_sdk_s3::operation::delete_object::builders::DeleteObjectFluentBuilder,
    if_match
);
if_match_on!(
    aws_sdk_s3::operation::copy_object::builders::CopyObjectFluentBuilder,
    copy_source_if_match
);
if_match_on!(
    aws_sdk_s3::operation::upload_part_copy::builders::UploadPartCopyFluentBuilder,
    copy_source_if_match
);

/// A GET is a read: RGW unquotes its `If-Match` (measured: the quoted form
/// gets 200 on Hetzner), and the RFC form is quoted. The dialect is
/// ignored, so a read never depends on a probe.
impl IfMatchRequest for aws_sdk_s3::operation::get_object::builders::GetObjectFluentBuilder {
    fn if_match_etag(self, etag: &str, _dialect: IfMatchDialect) -> Self {
        self.if_match(IfMatchDialect::Quoted.header_value(etag))
    }
}

/// The profiles found by the capability probe, by backend definition
/// fingerprint (`capability::fingerprint`). Process-wide, like the passive
/// health sink: the storage backends are built from the config before any
/// probe runs, and look their profile up per conditional request.
static PROFILES: std::sync::LazyLock<parking_lot::RwLock<HashMap<String, ConditionalProfile>>> =
    std::sync::LazyLock::new(Default::default);

/// Store the profile that a probe of the definition `fingerprint` found.
pub fn record(fingerprint: &str, profile: ConditionalProfile) {
    PROFILES.write().insert(fingerprint.to_string(), profile);
}

/// Store the profile that a probe of the backend definition `def` found.
pub fn record_for(def: &crate::config::BackendConfig, profile: ConditionalProfile) {
    record(&super::capability::fingerprint(def), profile);
}

/// The profile of the definition `fingerprint`, when a probe found one.
/// `None`: never probed. The caller then sends the quoted form and treats
/// DELETE `If-Match` as not enforced.
pub fn lookup(fingerprint: &str) -> Option<ConditionalProfile> {
    PROFILES.read().get(fingerprint).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_value_truth_table() {
        use IfMatchDialect::*;
        assert_eq!(Quoted.header_value("\"abc\""), "\"abc\"");
        assert_eq!(Quoted.header_value("abc"), "\"abc\"");
        assert_eq!(Unquoted.header_value("\"abc\""), "abc");
        assert_eq!(Unquoted.header_value("abc"), "abc");
        assert_eq!(Unquoted.header_value(" \"a-3\" "), "a-3");
        assert_eq!(Quoted.header_value("*"), "*");
        assert_eq!(Unquoted.header_value("*"), "*");
        // A lone quote is not a quoted ETag.
        assert_eq!(Unquoted.header_value("\""), "\"");
    }

    #[test]
    fn a_missing_dialect_reads_as_quoted() {
        #[derive(serde::Deserialize)]
        struct Old {
            #[serde(default)]
            if_match: IfMatchDialect,
        }
        let old: Old = serde_json::from_str("{}").unwrap();
        assert_eq!(old.if_match, IfMatchDialect::Quoted);
        assert_eq!(
            serde_json::to_value(IfMatchDialect::Unquoted).unwrap(),
            serde_json::json!("unquoted")
        );
    }

    #[test]
    fn profiles_are_found_by_fingerprint() {
        let fp = format!("test-{}", uuid::Uuid::new_v4());
        assert_eq!(lookup(&fp), None);
        let p = ConditionalProfile {
            if_match: IfMatchDialect::Unquoted,
            conditional_delete: false,
        };
        record(&fp, p);
        assert_eq!(lookup(&fp), Some(p));
    }
}
