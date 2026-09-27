// SPDX-License-Identifier: BUSL-1.1

use super::*;
use crate::config::BackendEncryptionConfig as E;

// ── apply_merge_patch: RFC 7396 conformance ──────────────────────────────

#[test]
fn merge_patch_null_removes_present_and_noops_absent() {
    use serde_json::json;
    let mut t = json!({"a": 1, "b": 2});
    apply_merge_patch(&mut t, &json!({"a": null, "c": null}));
    // null removes an existing key; null on an ABSENT key is a no-op —
    // it must NOT insert a literal null (the incident's root cause).
    assert_eq!(t, json!({"b": 2}));
}

#[test]
fn merge_patch_nested_null_in_inserted_subtree_is_dropped() {
    use serde_json::json;
    // THE incident shape: patching a bucket that has no server-side
    // policy yet, with the GUI's `public_prefixes: null` in the patch.
    let mut t = json!({ "buckets": {} });
    apply_merge_patch(
        &mut t,
        &json!({ "buckets": { "beshu-b2": { "backend": "b2", "public_prefixes": null } } }),
    );
    assert_eq!(t, json!({ "buckets": { "beshu-b2": { "backend": "b2" } } }));
}

#[test]
fn merge_patch_object_onto_non_object_coerces_and_strips_nulls() {
    use serde_json::json;
    let mut t = json!("scalar");
    apply_merge_patch(&mut t, &json!({"keep": 1, "drop": null}));
    assert_eq!(t, json!({"keep": 1}));
}

#[test]
fn merge_patch_arrays_replace_atomically_and_scalars_replace() {
    use serde_json::json;
    let mut t = json!({"list": [1, 2, 3], "s": "old"});
    apply_merge_patch(&mut t, &json!({"list": [9], "s": "new"}));
    assert_eq!(t, json!({"list": [9], "s": "new"}));
}

#[test]
fn merge_into_typed_names_the_offending_field_path() {
    use serde_json::json;
    // A genuinely wrong type must produce an error naming the exact
    // field, not a path-less serde diagnostic (the operator-facing
    // "invalid type: null, expected a sequence" incident class).
    let current = crate::config_sections::StorageSection::default();
    let patch = json!({ "buckets": { "beshu-b2": { "quota_bytes": "not-a-number" } } });
    let err =
        merge_into_typed::<crate::config_sections::StorageSection>(&current, &patch, "storage")
            .expect_err("string where number expected must fail");
    assert!(
        err.contains("buckets.beshu-b2.quota_bytes"),
        "error must name the field path, got: {err}"
    );
}

#[test]
fn merge_into_typed_accepts_gui_null_public_prefixes_on_fresh_bucket() {
    use serde_json::json;
    // End-to-end regression for the operator-blocking 400:
    // `invalid storage section body: invalid type: null, expected a
    // sequence`. A fresh (absent) bucket policy patched with the GUI's
    // `public_prefixes: null` must deserialize cleanly.
    let current = crate::config_sections::StorageSection::default();
    let patch = json!({
        "buckets": { "beshu-b2": { "backend": "b2", "public_prefixes": null } }
    });
    let merged: Result<crate::config_sections::StorageSection, String> =
        merge_into_typed(&current, &patch, "storage");
    let merged = merged.expect("null-for-list from the GUI must not 400");
    assert_eq!(
        merged
            .buckets
            .get("beshu-b2")
            .and_then(|b| b.backend.as_deref()),
        Some("b2")
    );
    assert!(merged
        .buckets
        .get("beshu-b2")
        .map(|b| b.public_prefixes.is_empty())
        .unwrap_or(false));
}

const HEX32: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const HEX32_B: &str = "0202020202020202020202020202020202020202020202020202020202020202";

fn proxy(key: Option<&str>, legacy: Option<&str>) -> E {
    E::Aes256GcmProxy {
        key: key.map(str::to_string),
        key_id: None,
        legacy_key: legacy.map(str::to_string),
        legacy_key_id: None,
    }
}

fn kms(legacy: Option<&str>) -> E {
    E::SseKms {
        kms_key_id: "arn:aws:kms:us-east-1:123:key/abc".into(),
        bucket_key_enabled: true,
        legacy_key: legacy.map(str::to_string),
        legacy_key_id: None,
    }
}

#[test]
fn preserve_mode_flip_proxy_to_kms_auto_promotes_primary_to_legacy() {
    // Regression for correctness x-ray C3: operator edits the
    // storage section with the UI, which redacts the primary key
    // on GET. Body says `{mode: sse-kms, kms_key_id: ...}` — no
    // legacy_key. Before the fix, the old primary key silently
    // disappeared and every historical object on the backend
    // became unreadable.
    let old = proxy(Some(HEX32), None);
    let mut new = kms(None);
    let probe = BackendKeyPresence::default();
    preserve_backend_encryption_secrets("b", &mut new, &old, probe).unwrap();
    assert_eq!(
        new.legacy_key(),
        Some(HEX32),
        "mode flip Aes256GcmProxy → SseKms must promote old primary into new legacy_key slot"
    );
}

#[test]
fn preserve_mode_flip_explicit_null_legacy_key_disables_promotion() {
    // Escape hatch: operator explicitly passes `legacy_key: null`
    // in the body to say "discard old keys, don't preserve".
    // Must NOT auto-promote.
    let old = proxy(Some(HEX32), None);
    let mut new = kms(None);
    let probe = BackendKeyPresence {
        legacy_key: Presence::Null,
        ..Default::default()
    };
    preserve_backend_encryption_secrets("b", &mut new, &old, probe).unwrap();
    assert_eq!(
        new.legacy_key(),
        None,
        "explicit `legacy_key: null` in body must disable auto-promotion"
    );
}

#[test]
fn preserve_mode_flip_explicit_legacy_key_in_body_wins_over_promotion() {
    // Operator explicitly sets a DIFFERENT legacy_key in the
    // body. Must preserve the explicit value — not overwrite
    // it with the old primary.
    let old = proxy(Some(HEX32), None);
    let mut new = kms(Some(HEX32_B));
    let probe = BackendKeyPresence::default();
    preserve_backend_encryption_secrets("b", &mut new, &old, probe).unwrap();
    assert_eq!(
        new.legacy_key(),
        Some(HEX32_B),
        "explicit legacy_key in body must override auto-promotion"
    );
}

#[test]
fn preserve_mode_flip_proxy_to_none_auto_promotes() {
    // Recipe (D) via the UI: operator flips to mode:none but
    // forgets to paste legacy_key. Auto-promotion preserves
    // historical-read capability.
    let old = proxy(Some(HEX32), None);
    let mut new = E::None {
        legacy_key: None,
        legacy_key_id: None,
    };
    let probe = BackendKeyPresence::default();
    preserve_backend_encryption_secrets("b", &mut new, &old, probe).unwrap();
    assert_eq!(new.legacy_key(), Some(HEX32));
}

#[test]
fn preserve_same_mode_doesnt_promote_anywhere() {
    // Same-mode edit (Aes256GcmProxy → Aes256GcmProxy with a
    // rotated key): no promotion, legacy_key just preserves
    // from old → new via the same-slot preservation path.
    let old = proxy(Some(HEX32), Some(HEX32_B));
    let mut new = proxy(None, None); // redacted round-trip
    let probe = BackendKeyPresence::default();
    preserve_backend_encryption_secrets("b", &mut new, &old, probe).unwrap();
    assert_eq!(
        new.primary_key(),
        Some(HEX32),
        "same-mode preserves primary"
    );
    assert_eq!(
        new.legacy_key(),
        Some(HEX32_B),
        "same-mode preserves legacy_key"
    );
}

fn derived(name: &str, hex: &str) -> String {
    derive_hex_key_id(name, hex).unwrap()
}

#[test]
fn merge_kept_field_truth_table() {
    use Presence::*;
    // (presence, new before, old) -> new after
    type Case = (
        Presence,
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
    );
    let cases: [Case; 7] = [
        (Absent, None, Some("old"), Some("old")),
        (Absent, None, None, None),
        (Absent, Some("merged"), Some("old"), Some("merged")),
        (Null, None, Some("old"), None),
        (Set, Some("new"), Some("old"), Some("new")),
        (Set, Some("new"), None, Some("new")),
        (Null, None, None, None),
    ];
    for (presence, before, old, after) in cases {
        let mut v = before.map(str::to_string);
        merge_kept_field(presence, &mut v, old);
        assert_eq!(v.as_deref(), after, "{presence:?} {before:?} {old:?}");
    }
}

#[test]
fn key_id_follows_the_rule_only_with_its_own_key() {
    let old = E::Aes256GcmProxy {
        key: Some(HEX32.into()),
        key_id: Some("k-explicit".into()),
        legacy_key: None,
        legacy_key_id: None,
    };
    let kid = |e: &E| match e {
        E::Aes256GcmProxy { key_id, .. } => key_id.clone(),
        _ => None,
    };
    let run = |key: Option<&str>, key_id: Option<&str>, p: BackendKeyPresence| {
        let mut new = E::Aes256GcmProxy {
            key: key.map(str::to_string),
            key_id: key_id.map(str::to_string),
            legacy_key: None,
            legacy_key_id: None,
        };
        preserve_backend_encryption_secrets("b", &mut new, &old, p).map(|_| new)
    };
    let p = |key_id| BackendKeyPresence {
        key_id,
        ..Default::default()
    };
    // Same key (absent in body): absent keeps, null clears, a value replaces.
    assert_eq!(
        kid(&run(None, None, p(Presence::Absent)).unwrap()).as_deref(),
        Some("k-explicit")
    );
    assert_eq!(kid(&run(None, None, p(Presence::Null)).unwrap()), None);
    assert_eq!(
        kid(&run(None, Some("k-2"), p(Presence::Set)).unwrap()).as_deref(),
        Some("k-2")
    );
    // Same key sent again: the id is kept too.
    assert_eq!(
        kid(&run(
            Some(HEX32),
            None,
            BackendKeyPresence {
                key: Presence::Set,
                ..p(Presence::Absent)
            }
        )
        .unwrap())
        .as_deref(),
        Some("k-explicit")
    );
    // A new key without an id: derived id, and the old id goes to the shim.
    let rotated = run(
        Some(HEX32_B),
        None,
        BackendKeyPresence {
            key: Presence::Set,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(kid(&rotated), None);
    assert_eq!(rotated.legacy_key_id(), Some("k-explicit"));
}

#[test]
fn absent_key_id_is_kept_with_an_unchanged_key() {
    // A named-backend entry is replaced as a whole, so a body that omits
    // `key_id` (the GET redacts `key`, and the UI builds the block from the
    // patch) must not drop an explicit id: new writes would get a derived
    // id, and objects stamped with the explicit id would match no key.
    let old = E::Aes256GcmProxy {
        key: Some(HEX32.into()),
        key_id: Some("k-explicit".into()),
        legacy_key: Some(HEX32_B.into()),
        legacy_key_id: Some("k-old".into()),
    };
    let mut new = E::Aes256GcmProxy {
        key: None,
        key_id: None,
        legacy_key: None,
        legacy_key_id: None,
    };
    preserve_backend_encryption_secrets("b", &mut new, &old, BackendKeyPresence::default())
        .unwrap();
    assert_eq!(new.primary_key(), Some(HEX32));
    assert!(
        matches!(&new, E::Aes256GcmProxy { key_id: Some(k), .. } if k == "k-explicit"),
        "{new:?}"
    );
}

#[test]
fn rotation_keeps_the_old_key_as_legacy_with_its_stamped_id() {
    // The UI rotate flow: body carries only the new key. Old objects
    // carry the id derived from the OLD key; the shim must match it.
    let old = proxy(Some(HEX32), None);
    let mut new = proxy(Some(HEX32_B), None);
    preserve_backend_encryption_secrets("b", &mut new, &old, BackendKeyPresence::default())
        .unwrap();
    assert_eq!(new.primary_key(), Some(HEX32_B));
    assert_eq!(new.legacy_key(), Some(HEX32));
    assert_eq!(new.legacy_key_id(), Some(derived("b", HEX32).as_str()));
}

#[test]
fn rotation_with_explicit_old_key_id_keeps_that_id() {
    let old = E::Aes256GcmProxy {
        key: Some(HEX32.into()),
        key_id: Some("k-old".into()),
        legacy_key: None,
        legacy_key_id: None,
    };
    let mut new = proxy(Some(HEX32_B), None);
    preserve_backend_encryption_secrets("b", &mut new, &old, BackendKeyPresence::default())
        .unwrap();
    assert_eq!(new.legacy_key_id(), Some("k-old"));
}

#[test]
fn rotation_that_keeps_the_key_id_is_refused() {
    // Singleton merge-patch keeps an explicit key_id: the new key would
    // be picked for objects stamped with the old key's id.
    let old = E::Aes256GcmProxy {
        key: Some(HEX32.into()),
        key_id: Some("k1".into()),
        legacy_key: None,
        legacy_key_id: None,
    };
    let mut new = E::Aes256GcmProxy {
        key: Some(HEX32_B.into()),
        key_id: Some("k1".into()),
        legacy_key: None,
        legacy_key_id: None,
    };
    let err =
        preserve_backend_encryption_secrets("b", &mut new, &old, BackendKeyPresence::default())
            .unwrap_err();
    assert!(err.contains("key id"), "{err}");
}

#[test]
fn rotation_refuses_to_overwrite_a_different_legacy_key() {
    const HEX32_C: &str = "0303030303030303030303030303030303030303030303030303030303030303";
    let old = proxy(Some(HEX32), Some(HEX32_C));
    let mut new = proxy(Some(HEX32_B), Some(HEX32_C)); // merge-patch kept it
    let err =
        preserve_backend_encryption_secrets("b", &mut new, &old, BackendKeyPresence::default())
            .unwrap_err();
    assert!(err.contains("legacy"), "{err}");
    // An explicit legacy_key: null is the operator's "drop it" choice.
    let mut new = proxy(Some(HEX32_B), None);
    let probe = BackendKeyPresence {
        legacy_key: Presence::Null,
        ..Default::default()
    };
    preserve_backend_encryption_secrets("b", &mut new, &old, probe).unwrap();
    assert_eq!(new.legacy_key(), None);
}

#[test]
fn mode_flip_promotion_carries_the_old_key_id() {
    // Without the id, the engine derives `<name>::legacy` for the shim
    // and it matches no object written under the old primary.
    let old = proxy(Some(HEX32), None);
    let mut new = E::None {
        legacy_key: None,
        legacy_key_id: None,
    };
    preserve_backend_encryption_secrets("b", &mut new, &old, BackendKeyPresence::default())
        .unwrap();
    assert_eq!(new.legacy_key_id(), Some(derived("b", HEX32).as_str()));
}

#[test]
fn unchanged_key_keeps_the_existing_legacy_pair() {
    let old = E::Aes256GcmProxy {
        key: Some(HEX32_B.into()),
        key_id: None,
        legacy_key: Some(HEX32.into()),
        legacy_key_id: Some("k-old".into()),
    };
    // Named-list PUT: the whole encryption object is replaced, key and
    // legacy_key_id absent.
    let mut new = proxy(None, None);
    preserve_backend_encryption_secrets("b", &mut new, &old, BackendKeyPresence::default())
        .unwrap();
    assert_eq!(new.primary_key(), Some(HEX32_B));
    assert_eq!(new.legacy_key(), Some(HEX32));
    assert_eq!(new.legacy_key_id(), Some("k-old"));
}
