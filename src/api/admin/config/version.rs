// SPDX-License-Identifier: BUSL-1.1

//! Optimistic concurrency for the config write endpoints.
//!
//! A config version is a keyed hash of the runtime config: of one section
//! (section GET / PUT) or of the whole document (export / apply). The GET
//! endpoints return it as `ETag`; a write that sends `If-Match` with an
//! older version gets `409 Conflict` with the current version, so a second
//! browser tab cannot overwrite an edit it never saw. A write without
//! `If-Match` is not checked (scripts, the CLI).
//!
//! The version is content-derived, so every mutation path (GUI, GitOps
//! apply, a background migrate flip, a restore) changes it without any
//! bookkeeping. The hash covers secrets too (a rotated key is a change), so
//! it is a MAC keyed with a key derived from the config DB key: the ETag
//! reveals nothing about the secrets, and it is stable across restarts.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::SectionName;
use crate::config::Config;
use crate::config_sections::SectionedConfig;

/// The key the version hash is keyed with: derived from the config DB key
/// at startup ([`install_version_key`]), so it is the same on every boot of
/// one deployment (a tab left open across a restart gets no false
/// conflict) and on every instance that shares the DB key. It is never the
/// DB key itself, and the ETag is a MAC, so it reveals nothing about secret
/// values. Before installation (tests, no config DB), a random per-process
/// key.
static VERSION_KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();

/// Pure: the version key for a config DB key (domain-separated HMAC).
fn derive_version_key(db_key: &str) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(db_key.as_bytes()).expect("HMAC takes a key of any size");
    mac.update(b"deltaglider config version key v1");
    mac.finalize().into_bytes().into()
}

/// Install the version key, derived from the config DB key (startup, once).
pub fn install_version_key(db_key: &str) {
    let _ = VERSION_KEY.set(derive_version_key(db_key));
}

fn version_key() -> &'static [u8; 32] {
    VERSION_KEY.get_or_init(|| {
        use rand::RngCore;
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        k
    })
}

/// The version of `section` of `cfg`, or of the whole document (`None`).
pub(super) fn config_version(cfg: &Config, section: Option<SectionName>) -> String {
    let sectioned = SectionedConfig::from_flat(cfg);
    // `to_value` first: its maps are sorted, so the bytes are canonical.
    let value = match section {
        None => serde_json::to_value(&sectioned),
        Some(SectionName::Admission) => {
            serde_json::to_value(sectioned.admission.unwrap_or_default())
        }
        Some(SectionName::Access) => serde_json::to_value(&sectioned.access),
        Some(SectionName::Storage) => serde_json::to_value(&sectioned.storage),
        Some(SectionName::Advanced) => serde_json::to_value(&sectioned.advanced),
    }
    .unwrap_or_default();
    let mut mac =
        Hmac::<Sha256>::new_from_slice(version_key()).expect("HMAC takes a key of any size");
    mac.update(section.map_or("document", SectionName::as_str).as_bytes());
    mac.update(&serde_json::to_vec(&value).unwrap_or_default());
    hex::encode(&mac.finalize().into_bytes()[..16])
}

/// `ETag` header value for a version (a strong, quoted entity tag).
pub(super) fn etag(version: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{version}\"")).expect("hex is a valid header value")
}

/// Pure: does the request's `If-Match` name a version other than
/// `current`? No header, or `*`, never conflicts. A list matches when any
/// entry matches; a weak `W/` prefix is ignored.
pub(super) fn if_match_conflicts(headers: &HeaderMap, current: &str) -> bool {
    let Some(raw) = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    !raw.split(',')
        .map(str::trim)
        .any(|tag| tag == "*" || tag.trim_start_matches("W/").trim_matches('"') == current)
}

/// The 409 for a stale write: says what happened and carries the current
/// version, so the client can reload or re-apply on top of it.
pub(super) fn conflict(current: &str, what: &str) -> Response {
    let mut resp = (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "config_conflict: the {what} changed after you loaded it (another tab, \
                 another admin, or a GitOps apply). Reload it and apply your edits again."
            ),
            "current_version": current,
        })),
    )
        .into_response();
    resp.headers_mut()
        .insert(axum::http::header::ETAG, etag(current));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(if_match: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = if_match {
            h.insert(axum::http::header::IF_MATCH, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn version_key_is_derived_and_stable() {
        assert_eq!(derive_version_key("k1"), derive_version_key("k1"));
        assert_ne!(derive_version_key("k1"), derive_version_key("k2"));
        assert_ne!(
            &derive_version_key("k1")[..2],
            "k1".as_bytes(),
            "not the raw key"
        );
    }

    #[test]
    fn if_match_truth_table() {
        assert!(!if_match_conflicts(&headers(None), "v1"));
        assert!(!if_match_conflicts(&headers(Some("*")), "v1"));
        assert!(!if_match_conflicts(&headers(Some("\"v1\"")), "v1"));
        assert!(!if_match_conflicts(&headers(Some("W/\"v1\"")), "v1"));
        assert!(!if_match_conflicts(&headers(Some("\"v0\", \"v1\"")), "v1"));
        assert!(if_match_conflicts(&headers(Some("\"v0\"")), "v1"));
    }

    #[test]
    fn a_version_changes_with_its_section_only() {
        let a = Config::default();
        let mut b = a.clone();
        b.cache_size_mb += 1; // an `advanced` field
        assert_eq!(
            config_version(&a, Some(SectionName::Advanced)),
            config_version(&a, Some(SectionName::Advanced)),
            "stable for the same config"
        );
        assert_ne!(
            config_version(&a, Some(SectionName::Advanced)),
            config_version(&b, Some(SectionName::Advanced))
        );
        assert_eq!(
            config_version(&a, Some(SectionName::Access)),
            config_version(&b, Some(SectionName::Access)),
            "another section's edit is not a conflict"
        );
        assert_ne!(config_version(&a, None), config_version(&b, None));
    }
}
