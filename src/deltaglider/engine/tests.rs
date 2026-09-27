// SPDX-License-Identifier: BUSL-1.1

use super::*;

// ──────────────────────────────────────────────────────────────
// Step 2: per-backend wrapping + key_id collision detection
// ──────────────────────────────────────────────────────────────

/// Fake inner backend that records nothing — used only to check
/// that `wrap_backend_with_encryption` constructs without error
/// for every mode. Actual put/get semantics are covered by the
/// CountingBackend tests in `storage::encrypting::tests::wrapper_tests`.
struct NullInner;

#[async_trait::async_trait]
impl crate::storage::StorageBackend for NullInner {
    async fn reference_fence(
        &self,
        b: &str,
        p: &str,
    ) -> Result<crate::storage::RefFence, crate::storage::StorageError> {
        crate::storage::unfenced_reference_fence(self, b, p).await
    }
    async fn write_reference_fenced(
        &self,
        b: &str,
        p: &str,
        op: crate::storage::RefWrite<'_>,
        _: &crate::storage::RefFence,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<crate::storage::RefFence, crate::storage::StorageError> {
        crate::storage::unfenced_reference_write(self, b, p, op, proof).await
    }
    async fn create_bucket(&self, _: &str) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn delete_bucket(&self, _: &str) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn list_buckets(&self) -> Result<Vec<String>, crate::storage::StorageError> {
        Ok(vec![])
    }
    async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, crate::storage::StorageError> {
        Ok(vec![])
    }
    async fn head_bucket(&self, _: &str) -> Result<bool, crate::storage::StorageError> {
        Ok(true)
    }
    async fn has_reference(&self, _: &str, _: &str) -> Result<bool, StorageError> {
        Ok(false)
    }
    async fn put_reference(
        &self,
        _: &str,
        _: &str,
        _: &[u8],
        _: &crate::types::FileMetadata,
        _proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn get_reference(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Vec<u8>, crate::storage::StorageError> {
        Ok(vec![])
    }
    async fn get_reference_metadata(
        &self,
        _: &str,
        _: &str,
    ) -> Result<crate::types::FileMetadata, crate::storage::StorageError> {
        Err(crate::storage::StorageError::Other("null".into()))
    }
    async fn put_reference_metadata(
        &self,
        _: &str,
        _: &str,
        _: &crate::types::FileMetadata,
        _proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn delete_reference(
        &self,
        _: &str,
        _: &str,
        _proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn flush_pending(&self) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn put_delta(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &[u8],
        _: &crate::types::FileMetadata,
        _proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn get_delta(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Vec<u8>, crate::storage::StorageError> {
        Ok(vec![])
    }
    async fn get_delta_metadata(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<crate::types::FileMetadata, crate::storage::StorageError> {
        Err(crate::storage::StorageError::Other("null".into()))
    }
    async fn delete_delta(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn put_passthrough(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &[u8],
        _: &crate::types::FileMetadata,
    ) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn get_passthrough(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Vec<u8>, crate::storage::StorageError> {
        Ok(vec![])
    }
    async fn open_object(
        &self,
        b: &str,
        p: &str,
        o: crate::storage::StoredObject<'_>,
    ) -> Result<
        (crate::storage::ByteStream, crate::types::FileMetadata),
        crate::storage::StorageError,
    > {
        crate::storage::open_object_by_parts(self, b, p, o).await
    }
    async fn get_passthrough_stream(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<bytes::Bytes, crate::storage::StorageError>>,
        crate::storage::StorageError,
    > {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn get_passthrough_stream_range(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: u64,
        _: u64,
    ) -> Result<
        (
            futures::stream::BoxStream<'static, Result<bytes::Bytes, crate::storage::StorageError>>,
            u64,
        ),
        crate::storage::StorageError,
    > {
        Ok((Box::pin(futures::stream::empty()), 0))
    }
    async fn get_passthrough_metadata(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<crate::types::FileMetadata, crate::storage::StorageError> {
        Err(crate::storage::StorageError::Other("null".into()))
    }
    async fn delete_passthrough(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn scan_deltaspace(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Vec<crate::types::FileMetadata>, crate::storage::StorageError> {
        Ok(vec![])
    }
    async fn list_deltaspaces(&self, _: &str) -> Result<Vec<String>, crate::storage::StorageError> {
        Ok(vec![])
    }
    async fn total_size(&self, _: Option<&str>) -> Result<u64, crate::storage::StorageError> {
        Ok(0)
    }
    async fn put_directory_marker(
        &self,
        _: &str,
        _: &str,
    ) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
    async fn bulk_list_objects(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Vec<(String, crate::types::FileMetadata)>, crate::storage::StorageError> {
        Ok(vec![])
    }
    async fn enrich_list_metadata(
        &self,
        _: &str,
        o: Vec<(String, crate::types::FileMetadata)>,
    ) -> Result<Vec<(String, crate::types::FileMetadata)>, crate::storage::StorageError> {
        Ok(o)
    }
}

const HEX32_KEY_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HEX32_KEY_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

#[test]
fn test_wrap_backend_with_none_mode_wraps_anyway() {
    // Even mode:none gets wrapped — the sniffer defense
    // (xattr-strip case, B9 from the earlier audit) needs the
    // wrapper in the pipeline to fire. This test just verifies
    // construction succeeds; the sniffer behaviour itself is
    // covered in `storage::encrypting::tests::wrapper_tests::test_stripped_xattr_*`.
    let inner: Box<dyn StorageBackend> = Box::new(NullInner);
    let mut coll = KeyIdCollisionCheck::new();
    let wrapped = wrap_backend_with_encryption(
        "some-backend",
        inner,
        &crate::config::BackendEncryptionConfig::default(),
        &mut coll,
    );
    assert!(wrapped.is_ok());
}

#[test]
fn test_wrap_backend_with_aes_mode_accepts_hex_key() {
    let inner: Box<dyn StorageBackend> = Box::new(NullInner);
    let mut coll = KeyIdCollisionCheck::new();
    let wrapped = wrap_backend_with_encryption(
        "enc-backend",
        inner,
        &crate::config::BackendEncryptionConfig::Aes256GcmProxy {
            key: Some(HEX32_KEY_A.into()),
            key_id: Some("abc".into()),
            legacy_key: None,
            legacy_key_id: None,
        },
        &mut coll,
    );
    assert!(
        wrapped.is_ok(),
        "well-formed hex key + id must wrap cleanly"
    );
}

#[test]
fn test_wrap_backend_with_aes_mode_rejects_malformed_hex() {
    let inner: Box<dyn StorageBackend> = Box::new(NullInner);
    let mut coll = KeyIdCollisionCheck::new();
    let result = wrap_backend_with_encryption(
        "bad",
        inner,
        &crate::config::BackendEncryptionConfig::Aes256GcmProxy {
            key: Some("not-hex!".into()),
            key_id: None,
            legacy_key: None,
            legacy_key_id: None,
        },
        &mut coll,
    );
    // Box<dyn StorageBackend> doesn't impl Debug, so we can't use
    // `.unwrap_err()`; destructure by hand.
    let err = match result {
        Ok(_) => panic!("malformed hex must error"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("hex") || msg.contains("32 bytes"),
        "malformed hex must produce a hex-shaped error, got: {msg}"
    );
}

#[test]
fn test_key_id_collision_detected_at_construction() {
    // Two backends with the SAME explicit key_id but DIFFERENT
    // keys must fail at construction time. The read-side check
    // in EncryptingBackend.decrypt_if_needed would then fire on
    // every cross-backend read; surfacing it at startup beats
    // silent per-read failures in production.
    let mut coll = KeyIdCollisionCheck::new();
    let first = wrap_backend_with_encryption(
        "a",
        Box::new(NullInner),
        &crate::config::BackendEncryptionConfig::Aes256GcmProxy {
            key: Some(HEX32_KEY_A.into()),
            key_id: Some("shared-id".into()),
            legacy_key: None,
            legacy_key_id: None,
        },
        &mut coll,
    );
    assert!(first.is_ok());
    let second = wrap_backend_with_encryption(
        "b",
        Box::new(NullInner),
        &crate::config::BackendEncryptionConfig::Aes256GcmProxy {
            key: Some(HEX32_KEY_B.into()),
            key_id: Some("shared-id".into()),
            legacy_key: None,
            legacy_key_id: None,
        },
        &mut coll,
    );
    let err = match second {
        Ok(_) => panic!("collision must error"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("shared-id") && msg.contains("DIFFERENT"),
        "expected collision error citing key_id + 'DIFFERENT', got: {msg}"
    );
}

#[test]
fn test_key_id_collision_allowed_with_same_key() {
    // The documented escape hatch: two backends with the same
    // key_id AND the same key bytes are legal — used by operators
    // who want cross-backend portability (e.g. two aliases for
    // the same physical bucket). This must NOT error.
    let mut coll = KeyIdCollisionCheck::new();
    let first = wrap_backend_with_encryption(
        "primary",
        Box::new(NullInner),
        &crate::config::BackendEncryptionConfig::Aes256GcmProxy {
            key: Some(HEX32_KEY_A.into()),
            key_id: Some("portable".into()),
            legacy_key: None,
            legacy_key_id: None,
        },
        &mut coll,
    );
    assert!(first.is_ok());
    let second = wrap_backend_with_encryption(
        "replica",
        Box::new(NullInner),
        &crate::config::BackendEncryptionConfig::Aes256GcmProxy {
            key: Some(HEX32_KEY_A.into()),
            key_id: Some("portable".into()),
            legacy_key: None,
            legacy_key_id: None,
        },
        &mut coll,
    );
    match second {
        Ok(_) => { /* expected */ }
        Err(e) => {
            panic!("same id + same key must be allowed (portability escape hatch), got: {e}")
        }
    }
}

#[test]
fn test_wrap_backend_sse_modes_wrap_for_sniffer_defense() {
    // Step 4: SSE-KMS and SSE-S3 delegate encryption to AWS (see
    // `native_encryption_for` + `S3Backend::new`). The proxy
    // wrapper is STILL constructed for those modes — it holds no
    // proxy key, but it keeps the sniffer defense in the read
    // path for the xattr-strip scenario.
    let mut coll = KeyIdCollisionCheck::new();
    let wrapped = wrap_backend_with_encryption(
        "s3-kms",
        Box::new(NullInner),
        &crate::config::BackendEncryptionConfig::SseKms {
            kms_key_id: "arn:aws:kms:us-east-1:1:key/x".into(),
            bucket_key_enabled: true,
            legacy_key: None,
            legacy_key_id: None,
        },
        &mut coll,
    );
    assert!(wrapped.is_ok());

    let wrapped2 = wrap_backend_with_encryption(
        "s3-aes",
        Box::new(NullInner),
        &crate::config::BackendEncryptionConfig::SseS3 {
            legacy_key: None,
            legacy_key_id: None,
        },
        &mut coll,
    );
    assert!(wrapped2.is_ok());
}

#[test]
fn test_native_encryption_for_maps_modes_correctly() {
    use crate::config::BackendEncryptionConfig as E;
    use crate::storage::NativeEncryptionConfig as N;

    // Non-native modes produce N::None; the S3Backend gets no
    // SSE headers, and `EncryptingBackend` handles encryption
    // at the wrapper layer (or nothing, for mode:none).
    assert!(matches!(native_encryption_for(&E::default()), N::None));
    assert!(matches!(
        native_encryption_for(&E::Aes256GcmProxy {
            key: Some("hex".into()),
            key_id: None,
            legacy_key: None,
            legacy_key_id: None,
        }),
        N::None
    ));

    // SseS3 → N::SseS3 — AES256 headers, no KMS.
    assert!(matches!(
        native_encryption_for(&E::SseS3 {
            legacy_key: None,
            legacy_key_id: None,
        }),
        N::SseS3
    ));

    // SseKms → N::SseKms with the ARN and bucket_key_enabled
    // threaded through verbatim.
    match native_encryption_for(&E::SseKms {
        kms_key_id: "arn:aws:kms:us-east-1:111:key/abc".into(),
        bucket_key_enabled: false,
        legacy_key: None,
        legacy_key_id: None,
    }) {
        N::SseKms {
            kms_key_id,
            bucket_key_enabled,
        } => {
            assert_eq!(kms_key_id, "arn:aws:kms:us-east-1:111:key/abc");
            assert!(!bucket_key_enabled);
        }
        other => panic!("expected SseKms, got {other:?}"),
    }
}

// ──────────────────────────────────────────────────────────────
// Step 5: decrypt-only shim resolution
// ──────────────────────────────────────────────────────────────

#[test]
fn test_resolve_legacy_shim_absent_is_none_none() {
    // A mode with no legacy_* fields set returns (None, None).
    // The wrapper treats (None, None) as "no shim" — the
    // one-sided case (legacy_key without legacy_key_id or vice
    // versa) is silently ignored here, matching the bilateral
    // check in `pick_decrypt_key`.
    let (k, kid) = resolve_legacy_shim(
        "b",
        &crate::config::BackendEncryptionConfig::Aes256GcmProxy {
            key: Some(HEX32_KEY_A.into()),
            key_id: None,
            legacy_key: None,
            legacy_key_id: None,
        },
    )
    .unwrap();
    assert!(k.is_none());
    assert!(kid.is_none());
}

#[test]
fn test_derive_key_id_shape_invariants() {
    // Contract: 16 lowercase hex chars (8 bytes of SHA-256).
    // Integration tests used to assert this shape; now pinned
    // here so the integration suite can focus on the wiring
    // (same-backend ⇒ same-kid) rather than the format.
    let key = [0xab; 32];
    let id = derive_key_id("my-backend", &key);
    assert_eq!(id.len(), 16, "derived key_id must be 16 hex chars");
    assert!(
        id.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "derived key_id must be lowercase hex, got: {id}"
    );
}

#[test]
fn test_derive_key_id_name_disambiguates_same_key() {
    // Two backends sharing identical key bytes but distinct
    // names MUST produce distinct ids. This is the invariant
    // that lets the read-side key_id check reject "same key,
    // different backend" without relying on AEAD to fail.
    let key = [0xcd; 32];
    let id_a = derive_key_id("backend-a", &key);
    let id_b = derive_key_id("backend-b", &key);
    assert_ne!(id_a, id_b);
}

#[test]
fn test_derive_key_id_separator_prevents_collision() {
    // "ab" + "c" vs "a" + "bc": without the 0x00 separator the
    // SHA-256 pre-image would collide, and operators naming
    // backends "ab" and "a" with keys that differ only by
    // prefix alignment could accidentally produce the same id.
    let key1 = [0x11; 32];
    let key2 = [0x22; 32];
    // Names chosen to make the concat-ambiguity visible.
    assert_ne!(
        derive_key_id("ab", &key1),
        derive_key_id("a", &key2),
        "name/key separator missing — two different (name, key) pairs collided"
    );
}

#[test]
fn test_reference_integrity_empty_expected_is_pass() {
    // Out-of-band / CLI-uploaded references carry no recorded checksum.
    // We cannot verify, so this must pass (the downstream per-object
    // checksum is the safety net) — NOT regress the passthrough case.
    assert!(reference_integrity_ok("deadbeef", "").is_ok());
    assert!(reference_integrity_ok("", "").is_ok());
}

#[test]
fn test_reference_integrity_match_is_pass() {
    assert!(reference_integrity_ok("abc123", "abc123").is_ok());
}

#[test]
fn test_reference_integrity_mismatch_returns_expected() {
    // A present-but-disagreeing checksum means the on-disk reference is
    // corrupt; the caller must fail fast and skip caching.
    let err = reference_integrity_ok("actual_hash", "expected_hash")
        .expect_err("mismatch must be rejected");
    assert_eq!(err, "expected_hash");
}

#[test]
fn test_resolve_legacy_shim_explicit_kid_wins() {
    // Operator pinned both legacy_key AND legacy_key_id; the
    // resolver uses the explicit id verbatim.
    let (k, kid) = resolve_legacy_shim(
        "b",
        &crate::config::BackendEncryptionConfig::SseKms {
            kms_key_id: "arn".into(),
            bucket_key_enabled: true,
            legacy_key: Some(HEX32_KEY_B.into()),
            legacy_key_id: Some("explicit-legacy".into()),
        },
    )
    .unwrap();
    assert!(k.is_some());
    assert_eq!(kid.as_deref(), Some("explicit-legacy"));
}

#[test]
fn test_resolve_legacy_shim_works_on_mode_none() {
    // Regression for correctness x-ray C2: before the fix,
    // BackendEncryptionConfig::None was a unit variant with no
    // legacy_key field. Serde would silently drop legacy_key +
    // legacy_key_id from `{mode: none, legacy_key: ..., legacy_key_id: ...}`,
    // and recipe (D) in the docs (disable encryption but keep
    // reading historical objects) was dead-on-arrival.
    //
    // The fix promoted `None` to a struct variant with the same
    // legacy_* fields as the other modes. This test pins the
    // end-to-end shape — parsing + shim resolution — to make
    // sure a future refactor doesn't regress recipe (D).
    let yaml = r#"
mode: none
legacy_key: "0101010101010101010101010101010101010101010101010101010101010101"
legacy_key_id: "old-kid"
"#;
    let enc: crate::config::BackendEncryptionConfig =
        serde_yaml::from_str(yaml).expect("mode:none with legacy_key must parse");
    assert_eq!(
        enc.legacy_key().map(str::to_string),
        Some("0101010101010101010101010101010101010101010101010101010101010101".to_string())
    );
    assert_eq!(enc.legacy_key_id(), Some("old-kid"));
    let (key, kid) = resolve_legacy_shim("b", &enc).unwrap();
    assert!(
        key.is_some(),
        "mode:none + legacy_key must activate the decrypt-only shim"
    );
    assert_eq!(kid.as_deref(), Some("old-kid"));
}

#[test]
fn test_resolve_legacy_shim_derives_distinct_from_primary() {
    // legacy_key set without legacy_key_id — resolver derives
    // from `{name}::legacy` + key bytes. This MUST differ from
    // the primary's derived id so the mismatch check doesn't
    // accidentally let primary-stamped objects match the
    // legacy slot (or vice versa).
    let key_bytes = crate::storage::EncryptionKey::from_hex(HEX32_KEY_A)
        .unwrap()
        .0;
    let primary_id = derive_key_id("b", &key_bytes);
    let (_, legacy_id) = resolve_legacy_shim(
        "b",
        &crate::config::BackendEncryptionConfig::SseS3 {
            legacy_key: Some(HEX32_KEY_A.into()),
            legacy_key_id: None,
        },
    )
    .unwrap();
    assert!(legacy_id.is_some());
    assert_ne!(
        legacy_id.as_deref(),
        Some(primary_id.as_str()),
        "legacy-shim derivation MUST differ from primary derivation, \
             even when the key material is identical — name suffix `::legacy` \
             keeps them distinct"
    );
}

#[test]
fn test_resolve_legacy_shim_rejects_bad_hex() {
    // Bad hex in legacy_key surfaces at construction time with
    // the backend name in the error message.
    let result = resolve_legacy_shim(
        "my-backend",
        &crate::config::BackendEncryptionConfig::Aes256GcmProxy {
            key: None,
            key_id: None,
            legacy_key: Some("not-hex".into()),
            legacy_key_id: None,
        },
    );
    // EncryptionKey doesn't impl Debug (to prevent key leakage
    // via panic messages), so we destructure by hand.
    let err = match result {
        Ok(_) => panic!("bad legacy_key hex must error"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("my-backend") && (msg.contains("hex") || msg.contains("32 bytes")),
        "bad legacy_key hex must cite backend name + hex/length, got: {msg}"
    );
}

#[test]
fn test_local_prefix_could_match() {
    // Empty prefix matches everything
    assert!(DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match("releases/v1.0", ""));
    assert!(DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match("", ""));

    // Prefix drills into a deltaspace
    assert!(
        DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
            "releases/v1.0",
            "releases/v1.0/"
        )
    );
    assert!(
        DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
            "releases/v1.0",
            "releases/v1.0/app"
        )
    );

    // Prefix is broader than deltaspace
    assert!(
        DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
            "releases/v1.0",
            "releases/"
        )
    );
    assert!(
        DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match("releases/v1.0", "rel")
    );

    // No match — disjoint paths
    assert!(
        !DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
            "releases/v1.0",
            "backups/"
        )
    );
    assert!(
        !DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
            "releases/v1.0",
            "staging/"
        )
    );

    // Root local prefix (empty) — matches only prefixes without '/'
    assert!(DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match("", "app"));
    assert!(!DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match("", "releases/"));
}

#[test]
fn list_entry_needs_head_skips_passthrough_non_eligible() {
    use crate::types::StorageInfo;
    let router = FileRouter::new();

    let passthrough = |name: &str| {
        FileMetadata::fallback(
            name.to_string(),
            42,
            "md5".to_string(),
            chrono::Utc::now(),
            None,
            StorageInfo::Passthrough,
        )
    };
    let delta = || {
        let mut m = passthrough("app.zip");
        m.storage_info = StorageInfo::Delta {
            ref_path: "reference.bin".to_string(),
            ref_sha256: "sha".to_string(),
            delta_size: 10,
            delta_cmd: "xdelta3".to_string(),
        };
        m
    };

    // Passthrough + non-delta-eligible extension → NO head (the win):
    // the LIST size is authoritative for a verbatim-stored object.
    for key in [
        "ror/builds/1.70.0/readonlyrest-1.70.0_es7.8.1.zip.sha1",
        "ror/builds/1.70.0/readonlyrest-1.70.0_es7.8.1.zip.sha512",
        "images/logo.png",
        "ror/builds/1.70.0/checksums.txt",
    ] {
        assert!(
            !DeltaGliderEngine::<FilesystemBackend>::list_entry_needs_head(
                &router,
                key,
                &passthrough(key.rsplit('/').next().unwrap()),
            ),
            "{key} should skip HEAD"
        );
    }

    // Delta-eligible extension (even if this LIST entry is passthrough) →
    // HEAD, because it MIGHT be stored as a delta and need original-size.
    for key in [
        "ror/builds/1.70.0/readonlyrest-1.70.0_es7.8.1.zip",
        "backups/db.sql",
        "images/disk.iso",
    ] {
        assert!(
            DeltaGliderEngine::<FilesystemBackend>::list_entry_needs_head(
                &router,
                key,
                &passthrough(key.rsplit('/').next().unwrap()),
            ),
            "{key} should HEAD"
        );
    }

    // An entry already flagged as a delta always needs the HEAD,
    // regardless of extension.
    assert!(
        DeltaGliderEngine::<FilesystemBackend>::list_entry_needs_head(
            &router,
            "anything.bin",
            &delta(),
        )
    );
}

/// X-ray H3: a throttle-aborted HEAD sweep returns delta STUBS (empty
/// ref_sha256, stored delta_size). Those must be detected so the caller
/// skips caching them — else a later HEAD/GET serves the stub's wrong
/// (stored, not original) size from the poisoned cache.
#[test]
fn unresolved_delta_stub_is_detected() {
    use crate::types::StorageInfo;
    let stub = {
        let mut m = FileMetadata::fallback(
            "app.zip".to_string(),
            123,
            "etag".to_string(),
            chrono::Utc::now(),
            None,
            StorageInfo::delta_stub(123),
        );
        m.storage_info = StorageInfo::delta_stub(123);
        m
    };
    assert!(
        DeltaGliderEngine::<FilesystemBackend>::is_unresolved_delta_stub(&stub),
        "empty-ref_sha256 delta must be recognised as an unresolved stub"
    );
    // A genuinely HEAD-resolved delta (populated ref_sha256) is cacheable.
    let mut resolved = stub.clone();
    resolved.storage_info = StorageInfo::Delta {
        ref_path: "reference.bin".into(),
        ref_sha256: "realsha".into(),
        delta_size: 123,
        delta_cmd: "xdelta3".into(),
    };
    assert!(
        !DeltaGliderEngine::<FilesystemBackend>::is_unresolved_delta_stub(&resolved),
        "resolved delta must be cacheable"
    );
    // Passthrough is never a stub.
    let pt = FileMetadata::fallback(
        "x.png".into(),
        10,
        "e".into(),
        chrono::Utc::now(),
        None,
        StorageInfo::Passthrough,
    );
    assert!(!DeltaGliderEngine::<FilesystemBackend>::is_unresolved_delta_stub(&pt));
}
