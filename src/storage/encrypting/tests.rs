// SPDX-License-Identifier: BUSL-1.1

//! Unit tests of the encrypting wrapper and its formats.

use super::*;

#[cfg(test)]
mod wrapper_tests {
    use super::*;

    #[test]
    fn ciphertext_listings_without_a_cache_hit_are_stored_only() {
        let m = FileMetadata::fallback(
            "x".into(),
            1,
            "e".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        let objects = vec![
            ("a.bin".to_string(), m.clone()),
            ("b.bin".to_string(), m.clone()),
            ("dir/".to_string(), m),
        ];
        let mut sizes = vec![ListedSize::Listed, ListedSize::Cached, ListedSize::Listed];
        downgrade_ciphertext_listings(&objects, &mut sizes);
        assert_eq!(
            sizes,
            [
                ListedSize::StoredOnly,
                ListedSize::Cached,
                ListedSize::Listed
            ]
        );
    }

    fn test_key() -> EncryptionKey {
        EncryptionKey::from_hex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
            .unwrap()
    }

    /// strip_encryption_markers must remove BOTH proxy-encryption markers and
    /// leave user metadata otherwise intact. Copy/move paths (transfer.rs, s3s
    /// CopyObject, admin bulk copy/move) rely on this to avoid stamping stale
    /// "this is encrypted" markers onto a decrypted body — which would make the
    /// destination unreadable (and, for move, unrecoverable after source delete).
    #[test]
    fn strip_encryption_markers_removes_both_and_preserves_rest() {
        let mut md = std::collections::HashMap::new();
        md.insert(ENCRYPTION_MARKER_KEY.to_string(), "aes256-gcm-proxy".into());
        md.insert(ENCRYPTION_KEY_ID_KEY.to_string(), "k1".into());
        md.insert("user-tag".to_string(), "keep-me".into());
        strip_encryption_markers(&mut md);
        assert!(
            !md.contains_key(ENCRYPTION_MARKER_KEY),
            "dg-encrypted removed"
        );
        assert!(
            !md.contains_key(ENCRYPTION_KEY_ID_KEY),
            "dg-encryption-key-id removed"
        );
        assert_eq!(md.get("user-tag").map(String::as_str), Some("keep-me"));
    }

    /// The streaming `put_passthrough_file` must produce a chunked object that
    /// decrypts byte-identically via the normal read path — across the framing
    /// boundaries (empty, sub-window, exact window, exact 2 windows, multi+tail).
    /// storage-2: the adapter clamps a range against a size from the
    /// metadata cache, which can be stale. The v1 (single-shot) branch then
    /// sliced `plain[s..e]` with `s` past the end and panicked the request.
    /// Out-of-range is an `InvalidRange` error; an end past the object is
    /// clamped.
    #[tokio::test]
    async fn v1_range_reads_out_of_bounds_are_an_error_not_a_panic() {
        use crate::storage::filesystem::FilesystemBackend;
        use futures::StreamExt;
        let dir = tempfile::tempdir().unwrap();
        let fs = FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap();
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(test_key()),
            key_id: Some("kid-1".to_string()),
            ..Default::default()
        })));
        let wrapper = EncryptingBackend::new(fs, cfg);
        wrapper.create_bucket("b").await.unwrap();
        let meta = FileMetadata::fallback(
            "o.bin".into(),
            10,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        wrapper
            .put_passthrough("b", "p", "o.bin", b"0123456789", &meta)
            .await
            .unwrap();
        for (start, end) in [(20, 25), (10, 10), (8, 2)] {
            let res = wrapper
                .get_passthrough_stream_range("b", "p", "o.bin", start, end)
                .await;
            assert!(
                matches!(res, Err(StorageError::InvalidRange(_))),
                "({start},{end}) must be InvalidRange"
            );
        }
        let (stream, len) = wrapper
            .get_passthrough_stream_range("b", "p", "o.bin", 5, 100)
            .await
            .unwrap();
        let body: Vec<u8> = stream.map(|c| c.unwrap().to_vec()).concat().await;
        assert_eq!((len, body.as_slice()), (5, &b"56789"[..]));
    }

    #[tokio::test]
    async fn streaming_put_passthrough_file_roundtrips_all_boundaries() {
        use crate::storage::filesystem::FilesystemBackend;
        let cw = CHUNK_PLAINTEXT_SIZE;
        for size in [
            0usize,
            1,
            100,
            cw - 1,
            cw,
            cw + 1,
            2 * cw,
            2 * cw + 7,
            5 * cw + 123,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let fs = FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap();
            let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
                key: Some(test_key()),
                key_id: Some("kid-1".to_string()),
                ..Default::default()
            })));
            let wrapper = EncryptingBackend::new(fs, cfg);
            wrapper.create_bucket("b").await.unwrap();
            let sd = test_spool(&dir);

            let plaintext: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut src = tempfile::NamedTempFile::new().unwrap();
            std::io::Write::write_all(&mut src, &plaintext).unwrap();
            let meta = FileMetadata::fallback(
                "o.bin".into(),
                size as u64,
                "md5".into(),
                Utc::now(),
                None,
                crate::types::StorageInfo::Passthrough,
            );
            wrapper
                .put_passthrough_file("b", "p", "o.bin", src.path(), &meta, unheld(&sd))
                .await
                .unwrap_or_else(|e| panic!("size {size}: put failed: {e:?}"));
            let wire = wrapper
                .inner
                .get_passthrough("b", "p", "o.bin")
                .await
                .unwrap();
            assert!(
                wire.len() as u64 <= chunked_wire_len_bound(size as u64),
                "size {size}: wire {} over the reservation bound",
                wire.len()
            );

            let got = wrapper.get_passthrough("b", "p", "o.bin").await.unwrap();
            assert_eq!(got, plaintext, "size {size}: roundtrip mismatch");
            // On-disk body must be chunked-encrypted (not plaintext, not single-shot).
            let stored_meta = wrapper
                .inner
                .get_passthrough_metadata("b", "p", "o.bin")
                .await
                .unwrap();
            assert!(
                is_chunked_encrypted(&stored_meta),
                "size {size}: not chunked on disk"
            );

            // Same via the RELAYED-PARTS sink (the multipart-upload path): split
            // into 3 part files, store, read back byte-identical.
            let third = size / 3;
            let mut part_files = Vec::new();
            let mut off = 0usize;
            for len in [third, third, size - 2 * third] {
                let mut pf = tempfile::NamedTempFile::new().unwrap();
                std::io::Write::write_all(&mut pf, &plaintext[off..off + len]).unwrap();
                off += len;
                part_files.push(pf);
            }
            let part_paths: Vec<std::path::PathBuf> =
                part_files.iter().map(|f| f.path().to_path_buf()).collect();
            wrapper
                .put_passthrough_parts("b", "p", "parts.bin", &part_paths, &meta, unheld(&sd))
                .await
                .unwrap_or_else(|e| panic!("size {size}: parts put failed: {e:?}"));
            let got_parts = wrapper
                .get_passthrough("b", "p", "parts.bin")
                .await
                .unwrap();
            assert_eq!(
                got_parts, plaintext,
                "size {size}: parts roundtrip mismatch"
            );
        }
    }

    fn other_key() -> EncryptionKey {
        EncryptionKey::from_hex("fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210")
            .unwrap()
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = test_key();
        let pt = b"hello, encryption at rest!";
        let blob = encrypt(&key, pt).unwrap();
        assert_eq!(decrypt(&key, &blob).unwrap(), pt);
    }

    #[test]
    fn test_unique_ivs() {
        let key = test_key();
        let pt = b"same data";
        let b1 = encrypt(&key, pt).unwrap();
        let b2 = encrypt(&key, pt).unwrap();
        assert_ne!(b1, b2);
        assert_eq!(decrypt(&key, &b1).unwrap(), pt);
        assert_eq!(decrypt(&key, &b2).unwrap(), pt);
    }

    #[test]
    fn test_wrong_key_error() {
        let blob = encrypt(&test_key(), b"secret").unwrap();
        let r = decrypt(&other_key(), &blob);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("decryption failed"));
    }

    #[test]
    fn test_tampered_ciphertext() {
        let key = test_key();
        let mut blob = encrypt(&key, b"important").unwrap();
        blob[IV_LEN + 5] ^= 0xFF;
        assert!(decrypt(&key, &blob).is_err());
    }

    #[test]
    fn test_empty_data() {
        let key = test_key();
        let blob = encrypt(&key, b"").unwrap();
        assert_eq!(blob.len(), IV_LEN + 16);
        assert!(decrypt(&key, &blob).unwrap().is_empty());
    }

    #[test]
    fn test_large_data() {
        let key = test_key();
        let pt: Vec<u8> = (0..10_000_000u32).map(|i| (i % 256) as u8).collect();
        let blob = encrypt(&key, &pt).unwrap();
        assert_eq!(decrypt(&key, &blob).unwrap(), pt);
    }

    #[test]
    fn test_metadata_detection() {
        let mut m = FileMetadata::fallback(
            "test".into(),
            100,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        assert!(!is_encrypted(&m));
        mark_encrypted(&mut m, None);
        assert!(is_encrypted(&m));
    }

    #[test]
    fn test_key_validation() {
        assert!(EncryptionKey::from_hex("0123").is_err());
        assert!(EncryptionKey::from_hex("zzzz").is_err());
        assert!(EncryptionKey::from_hex(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )
        .is_ok());
    }

    #[test]
    fn test_blob_too_short() {
        let r = decrypt(&test_key(), &[0u8; 10]);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("too short"));
    }

    // ─────────────────────────────────────────────────────────────────
    // Chunked-format codec tests
    //
    // These cover the AEAD primitives in isolation; integration tests in
    // `tests/encryption_test.rs` exercise the streaming trait impl
    // (chunking on upload, decoding on range GET, etc.).
    // ─────────────────────────────────────────────────────────────────

    fn test_base_iv() -> [u8; IV_LEN] {
        // Fixed value for deterministic tests — real callers generate with OsRng.
        [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
    }

    #[test]
    fn test_chunk_nonce_is_derived_deterministically() {
        let iv = test_base_iv();
        let n0 = chunk_nonce(&iv, 0);
        // chunk_index=0 XORs zeros into the low 4 bytes — nonce equals base_iv.
        assert_eq!(n0, iv, "chunk 0 nonce must equal base_iv");

        let n1 = chunk_nonce(&iv, 1);
        assert_ne!(n1, iv, "chunk 1 nonce differs from base_iv");
        assert_eq!(n1[8], iv[8]);
        assert_eq!(n1[9], iv[9]);
        assert_eq!(n1[10], iv[10]);
        assert_eq!(n1[11], iv[11] ^ 0x01);
    }

    #[test]
    fn test_chunk_nonces_unique_across_sequential_indices() {
        // A real stream might have millions of chunks; we sanity-check a
        // small range and confirm each maps to a distinct nonce.
        let iv = test_base_iv();
        let mut seen = std::collections::HashSet::new();
        for i in 0u32..10_000 {
            let n = chunk_nonce(&iv, i);
            assert!(seen.insert(n), "duplicate nonce at index {i}");
        }
    }

    #[test]
    fn test_chunk_aad_distinguishes_final_flag() {
        // A truncation attack would try to reuse an AAD from a non-final
        // chunk but claim it as final (or vice versa). The decrypt-time
        // AAD rebuild must differ to catch this.
        let a = chunk_aad(42, false);
        let b = chunk_aad(42, true);
        assert_ne!(a, b, "AAD must differ when final flag differs");
        assert_eq!(a[8], 0);
        assert_eq!(b[8], 1);
    }

    /// ChunkFramer owns the is_final/index invariant for BOTH the file and the
    /// in-memory chunked paths. Feed it N full windows + an optional tail and
    /// assert: exactly one final frame (the last), monotonic 0..k indices, and a
    /// zero-window input still emits one empty final frame. Decrypting each frame
    /// with the index/is_final it was framed under must succeed (any drift in
    /// index or final-flag placement fails the AEAD auth).
    #[test]
    fn chunk_framer_stamps_final_and_index_correctly() {
        let key = test_key();
        let base_iv = test_base_iv();

        // Drive the framer with `n_full` full windows + a `tail_len`-byte tail
        // (tail_len == 0 means no tail). Returns the emitted frames in order.
        let run = |n_full: usize, tail_len: usize| -> Vec<Vec<u8>> {
            let mut framer = ChunkFramer::new(base_iv);
            let mut frames = Vec::new();
            for i in 0..n_full {
                let w = vec![(i as u8).wrapping_add(1); CHUNK_PLAINTEXT_SIZE];
                if let Some(f) = framer.push_window(&key, w).unwrap() {
                    frames.push(f);
                }
            }
            if tail_len > 0 {
                if let Some(f) = framer.push_window(&key, vec![0xAB; tail_len]).unwrap() {
                    frames.push(f);
                }
            }
            frames.push(framer.finish(&key).unwrap());
            frames
        };

        // Decode a frame body (strip the 4-byte LE length prefix) and verify it
        // decrypts under (index, is_final) — proving the framer stamped both.
        let verify = |frames: &[Vec<u8>]| {
            let last = frames.len() - 1;
            for (idx, frame) in frames.iter().enumerate() {
                let ct = &frame[CHUNK_FRAME_LEN_FIELD..];
                let is_final = idx == last;
                decrypt_chunk(&key, &base_iv, idx as u32, is_final, ct)
                    .unwrap_or_else(|e| panic!("frame {idx} (final={is_final}) failed: {e:?}"));
                // A non-final frame must NOT verify as final and vice-versa.
                assert!(
                    decrypt_chunk(&key, &base_iv, idx as u32, !is_final, ct).is_err(),
                    "frame {idx} verified under the WRONG final-flag — is_final drift"
                );
            }
        };

        // Zero-byte object → exactly one empty final frame.
        let z = run(0, 0);
        assert_eq!(z.len(), 1, "zero-byte → one final frame");
        verify(&z);
        // Sub-window, exact one window, one window + tail, several + tail.
        for (n_full, tail) in [(0, 100), (1, 0), (1, 7), (3, 0), (3, 4096)] {
            let frames = run(n_full, tail);
            let expected = n_full + if tail > 0 { 1 } else { 0 };
            let expected = expected.max(1); // always ≥1 (the final frame)
            assert_eq!(
                frames.len(),
                expected,
                "n_full={n_full} tail={tail} frame count"
            );
            verify(&frames);
        }
    }

    #[test]
    fn test_encrypt_decrypt_chunk_roundtrip() {
        let key = test_key();
        let iv = test_base_iv();
        let pt = b"chunk zero plaintext";
        let frame = encrypt_chunk(&key, &iv, 0, false, pt).unwrap();

        // Frame layout: [4 B length prefix] [ciphertext + tag]
        let declared_len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        let ct = &frame[4..];
        assert_eq!(ct.len(), declared_len);
        assert_eq!(ct.len(), pt.len() + GCM_TAG_LEN);

        let decrypted = decrypt_chunk(&key, &iv, 0, false, ct).unwrap();
        assert_eq!(decrypted, pt);
    }

    #[test]
    fn test_encrypt_decrypt_chunk_final_flag_preserved() {
        // Writer encrypts the final chunk with is_final=true; reader must
        // pass the same flag to decrypt or GCM auth fails (the whole
        // point of binding final into AAD).
        let key = test_key();
        let iv = test_base_iv();
        let pt = b"tail chunk";
        let frame = encrypt_chunk(&key, &iv, 5, true, pt).unwrap();
        let ct = &frame[4..];

        // Honest reader — matches flag.
        assert_eq!(decrypt_chunk(&key, &iv, 5, true, ct).unwrap(), pt);

        // Malicious reader claiming final=false — must fail (truncation guard).
        let bad = decrypt_chunk(&key, &iv, 5, false, ct);
        assert!(bad.is_err(), "AAD mismatch on final flag must reject");
    }

    #[test]
    fn test_chunk_reordering_is_detected() {
        // Simulate an attacker swapping two chunks on disk: their
        // ciphertexts are valid AEAD outputs, but the AAD they were
        // signed with had different chunk_index values. Decrypt with the
        // SWAPPED index (what an out-of-order reader would compute) must
        // fail.
        let key = test_key();
        let iv = test_base_iv();

        let frame0 = encrypt_chunk(&key, &iv, 0, false, b"chunk-zero").unwrap();
        let frame1 = encrypt_chunk(&key, &iv, 1, false, b"chunk-one_").unwrap();
        let ct0 = &frame0[4..];
        let ct1 = &frame1[4..];

        // Honest sequential decrypt works.
        assert_eq!(
            decrypt_chunk(&key, &iv, 0, false, ct0).unwrap(),
            b"chunk-zero"
        );
        assert_eq!(
            decrypt_chunk(&key, &iv, 1, false, ct1).unwrap(),
            b"chunk-one_"
        );

        // Swapped: try to decrypt chunk 0's ciphertext AS IF it were chunk 1.
        assert!(decrypt_chunk(&key, &iv, 1, false, ct0).is_err());
        assert!(decrypt_chunk(&key, &iv, 0, false, ct1).is_err());
    }

    #[test]
    fn test_chunk_oversized_plaintext_rejected() {
        // encrypt_chunk guards against accidental oversized plaintext
        // (would exceed the frame-size ceiling on disk). Writers must
        // re-slice before calling.
        let key = test_key();
        let iv = test_base_iv();
        let too_big = vec![0u8; CHUNK_PLAINTEXT_SIZE + 1];
        let r = encrypt_chunk(&key, &iv, 0, false, &too_big);
        assert!(r.is_err());
        assert!(r
            .unwrap_err()
            .to_string()
            .contains("chunk plaintext too large"));
    }

    #[test]
    fn test_chunk_tampered_ciphertext_rejected() {
        // Standard AEAD property: any single-bit flip in the ciphertext
        // invalidates the tag. We verify it holds for the chunked path.
        let key = test_key();
        let iv = test_base_iv();
        let frame = encrypt_chunk(&key, &iv, 0, false, b"sensitive").unwrap();
        let mut ct = frame[4..].to_vec();
        ct[0] ^= 0xFF;
        assert!(decrypt_chunk(&key, &iv, 0, false, &ct).is_err());
    }

    #[test]
    fn test_chunk_wrong_key_rejected() {
        let iv = test_base_iv();
        let frame = encrypt_chunk(&test_key(), &iv, 0, false, b"secret").unwrap();
        let ct = &frame[4..];
        assert!(decrypt_chunk(&other_key(), &iv, 0, false, ct).is_err());
    }

    #[test]
    fn test_chunk_empty_plaintext_is_legal() {
        // A zero-byte object still gets ONE frame (a zero-length plaintext)
        // with is_final=true. The frame carries just the GCM tag.
        let key = test_key();
        let iv = test_base_iv();
        let frame = encrypt_chunk(&key, &iv, 0, true, b"").unwrap();
        let declared_len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(declared_len, GCM_TAG_LEN);
        let ct = &frame[4..];
        assert_eq!(decrypt_chunk(&key, &iv, 0, true, ct).unwrap(), b"");
    }

    #[test]
    fn test_chunk_index_for_plaintext_offset() {
        // Boundary and mid-chunk math. If this is wrong, range reads will
        // return garbage. Cover: offset 0, mid-chunk-0, exactly-chunk-1,
        // mid-chunk-1, a huge offset.
        assert_eq!(chunk_index_for_plaintext_offset(0), (0, 0));
        assert_eq!(chunk_index_for_plaintext_offset(1), (0, 1));
        assert_eq!(
            chunk_index_for_plaintext_offset(CHUNK_PLAINTEXT_SIZE as u64 - 1),
            (0, CHUNK_PLAINTEXT_SIZE as u32 - 1)
        );
        assert_eq!(
            chunk_index_for_plaintext_offset(CHUNK_PLAINTEXT_SIZE as u64),
            (1, 0)
        );
        assert_eq!(
            chunk_index_for_plaintext_offset(CHUNK_PLAINTEXT_SIZE as u64 + 42),
            (1, 42)
        );
        // 10 GiB at 64 KiB chunks = 163840 chunks; pick a midway offset.
        let offset_10gib = 10u64 * 1024 * 1024 * 1024 + 777;
        let (idx, off) = chunk_index_for_plaintext_offset(offset_10gib);
        assert_eq!(
            idx as u64 * CHUNK_PLAINTEXT_SIZE as u64 + off as u64,
            offset_10gib
        );
    }

    #[test]
    fn test_wire_offset_of_chunk() {
        // Header is 16 bytes (4 magic + 12 iv). Every chunk is 65556 bytes
        // on the wire (except possibly the final one — the helper is only
        // correct for non-final chunks, but that's all the range path needs:
        // it uses this to SEEK to the start of a chunk, then decrypts from
        // there).
        assert_eq!(wire_offset_of_chunk(0), CHUNK_HEADER_LEN as u64);
        assert_eq!(
            wire_offset_of_chunk(1),
            CHUNK_HEADER_LEN as u64 + CHUNK_FRAME_WIRE_LEN as u64
        );
        assert_eq!(
            wire_offset_of_chunk(100),
            CHUNK_HEADER_LEN as u64 + 100 * CHUNK_FRAME_WIRE_LEN as u64
        );
    }

    #[test]
    fn test_chunk_marker_detection() {
        // is_encrypted is true for BOTH formats; is_chunked_encrypted is
        // true only for the chunked format.
        let mut m = FileMetadata::fallback(
            "test".into(),
            100,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        assert!(!is_encrypted(&m));
        assert!(!is_chunked_encrypted(&m));

        mark_encrypted(&mut m, None);
        assert!(is_encrypted(&m));
        assert!(!is_chunked_encrypted(&m));

        let mut m2 = FileMetadata::fallback(
            "test".into(),
            100,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_chunked_encrypted(&mut m2, None);
        assert!(is_encrypted(&m2));
        assert!(is_chunked_encrypted(&m2));
    }

    // ─────────────────────────────────────────────────────────────────
    // Step 3: key_id stamping + mismatch detection
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_mark_encrypted_stamps_key_id_when_present() {
        let mut m = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_encrypted(&mut m, Some("my-kid"));
        assert_eq!(stamped_key_id(&m), Some("my-kid"));
        // Marker and key_id are distinct fields.
        assert!(is_encrypted(&m));
    }

    #[test]
    fn test_mark_encrypted_no_key_id_leaves_field_absent() {
        let mut m = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_encrypted(&mut m, None);
        assert_eq!(stamped_key_id(&m), None);
    }

    #[test]
    fn test_mark_chunked_encrypted_stamps_key_id() {
        let mut m = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_chunked_encrypted(&mut m, Some("chunked-kid"));
        assert_eq!(stamped_key_id(&m), Some("chunked-kid"));
        assert!(is_chunked_encrypted(&m));
    }

    #[test]
    fn test_check_key_id_match_happy_path() {
        assert!(check_key_id_match(Some("same"), Some("same")).is_ok());
    }

    #[test]
    fn test_check_key_id_match_object_has_no_id_is_legacy_ok() {
        // Legacy objects written before Step 3 have no stamp. The
        // wrapper can still decrypt them — the check must NOT fire
        // when only one side has an id.
        assert!(check_key_id_match(None, Some("configured")).is_ok());
    }

    #[test]
    fn test_check_key_id_match_configured_has_no_id_is_ok() {
        // Symmetric. A mode:none-but-somehow-reading-an-encrypted-
        // object path — the OUTER "no key configured" error is the
        // right surface here, not a key_id mismatch.
        assert!(check_key_id_match(Some("obj"), None).is_ok());
    }

    #[test]
    fn test_check_key_id_match_both_absent_is_ok() {
        assert!(check_key_id_match(None, None).is_ok());
    }

    #[test]
    fn test_check_key_id_match_mismatch_errors_with_specifics() {
        let err = check_key_id_match(Some("obj-id"), Some("cfg-id")).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("obj-id") && msg.contains("cfg-id"),
            "error must cite BOTH ids so the operator can reason about \
             the rotation/routing/split-storage cause, got: {msg}"
        );
        // And the hint is present.
        assert!(
            msg.contains("rotated")
                || msg.contains("wrong backend")
                || msg.contains("different keys"),
            "error should explain the typical causes, got: {msg}"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // B3 regression: range reads on chunked-encrypted objects must
    // fetch only the header + the target chunks, NOT the whole file.
    //
    // Before the fix, `wire_start = 0` pulled the header + every chunk
    // from 0 up to the target, then the decoder threw the leading
    // chunks away. For a request like "last 100 bytes of a 10 GiB
    // object" that meant pulling and decrypting all 10 GiB to emit
    // 100 plaintext bytes.
    //
    // We can't easily prove this with an integration test (mocking
    // network I/O is heavy-weight). Instead we use a tiny counting
    // backend that records the `(start, end)` ranges that the
    // encrypting wrapper asks for from its inner layer. The math
    // guarantees hold: the body fetch must start at or after
    // `wire_offset_of_chunk(first_chunk)`, and the separate header
    // fetch must cover `[0, CHUNK_HEADER_LEN)`.
    // ─────────────────────────────────────────────────────────────────

    use bytes::Bytes;
    use chrono::Utc;
    use futures::stream::BoxStream;
    use std::sync::Mutex;

    /// A spy backend that records every `get_passthrough_stream_range`
    /// call so tests can assert on WHERE the wrapper reads from. Holds
    /// one passthrough object in memory; everything else no-ops or
    /// errors.
    struct CountingBackend {
        bytes: Vec<u8>,
        metadata: Mutex<Option<FileMetadata>>,
        ranges_requested: Mutex<Vec<(u64, u64)>>,
        /// Make metadata READS fail (a transient backend error).
        fail_meta_reads: std::sync::atomic::AtomicBool,
        /// Spool whose free budget `put_passthrough_file` records.
        probe_spool: Option<crate::deltaglider::spool::SpoolDir>,
        /// (source path, free spool MiB) per `put_passthrough_file` call.
        file_puts: Mutex<Vec<(std::path::PathBuf, usize)>>,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                metadata: Mutex::new(None),
                ranges_requested: Mutex::new(Vec::new()),
                fail_meta_reads: Default::default(),
                probe_spool: None,
                file_puts: Mutex::new(Vec::new()),
            }
        }

        fn set_contents(&mut self, bytes: Vec<u8>, meta: FileMetadata) {
            self.bytes = bytes;
            *self.metadata.lock().unwrap() = Some(meta);
        }
    }

    fn cb_err() -> StorageError {
        StorageError::Other("CountingBackend: not implemented for this test".into())
    }

    impl StorageBackend for CountingBackend {
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
        async fn get_passthrough_stream_range(
            &self,
            _: &str,
            _: &str,
            _: &str,
            start: u64,
            end: u64,
        ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
            self.ranges_requested.lock().unwrap().push((start, end));
            let end_clamped = std::cmp::min(end, self.bytes.len() as u64 - 1);
            let slice = self.bytes[start as usize..=end_clamped as usize].to_vec();
            let len = slice.len() as u64;
            Ok((
                Box::pin(futures::stream::once(async move { Ok(Bytes::from(slice)) })),
                len,
            ))
        }

        /// Full-stream read. Serves the whole byte vec as a single
        /// Bytes so the chunked decoder's phase-1 header parse hits
        /// the same code path it would over a real network stream.
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
        ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
            let b = Bytes::from(self.bytes.clone());
            Ok(Box::pin(futures::stream::once(async move { Ok(b) })))
        }

        async fn get_passthrough_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            if self
                .fail_meta_reads
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(StorageError::Throttled("transient".into()));
            }
            self.metadata.lock().unwrap().clone().ok_or_else(cb_err)
        }

        async fn put_passthrough_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
            m: &FileMetadata,
        ) -> Result<(), StorageError> {
            *self.metadata.lock().unwrap() = Some(m.clone());
            Ok(())
        }

        async fn put_passthrough_file(
            &self,
            _: &str,
            _: &str,
            _: &str,
            source_path: &std::path::Path,
            _: &FileMetadata,
            _: SpoolBudget<'_>,
        ) -> Result<(), StorageError> {
            let free = self.probe_spool.as_ref().map_or(0, |s| s.free_mib());
            self.file_puts
                .lock()
                .unwrap()
                .push((source_path.to_path_buf(), free));
            Ok(())
        }

        // All other trait methods: not needed for these tests.
        async fn create_bucket(&self, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn delete_bucket(&self, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
            Err(cb_err())
        }
        async fn list_buckets_with_dates(
            &self,
        ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
            Err(cb_err())
        }
        async fn head_bucket(&self, _: &str) -> Result<bool, StorageError> {
            Err(cb_err())
        }
        async fn has_reference(&self, _: &str, _: &str) -> Result<bool, StorageError> {
            Ok(false)
        }
        async fn put_reference(
            &self,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
            _proof: &crate::deltaglider::RefWriteProof,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn get_reference(&self, _: &str, _: &str) -> Result<Vec<u8>, StorageError> {
            Err(cb_err())
        }
        async fn get_reference_metadata(
            &self,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(cb_err())
        }
        async fn put_reference_metadata(
            &self,
            _: &str,
            _: &str,
            _: &FileMetadata,
            _proof: &crate::deltaglider::RefWriteProof,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn delete_reference(
            &self,
            _: &str,
            _: &str,
            _proof: &crate::deltaglider::RefWriteProof,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn flush_pending(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn put_delta(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
            _proof: &crate::deltaglider::RefWriteProof,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn get_delta(&self, _: &str, _: &str, _: &str) -> Result<Vec<u8>, StorageError> {
            Err(cb_err())
        }
        async fn get_delta_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(cb_err())
        }
        async fn delete_delta(&self, _: &str, _: &str, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn put_passthrough(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn get_passthrough(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Vec<u8>, StorageError> {
            Err(cb_err())
        }
        async fn delete_passthrough(&self, _: &str, _: &str, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn scan_deltaspace(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<FileMetadata>, StorageError> {
            Err(cb_err())
        }
        async fn list_deltaspaces(&self, _: &str) -> Result<Vec<String>, StorageError> {
            Err(cb_err())
        }
        async fn list_reference_prefixes(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<String>, StorageError> {
            Err(cb_err())
        }
        async fn total_size(&self, _: Option<&str>) -> Result<u64, StorageError> {
            Err(cb_err())
        }
        async fn put_directory_marker(&self, _: &str, _: &str) -> Result<(), StorageError> {
            Err(cb_err())
        }
        async fn bulk_list_objects(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
            Err(cb_err())
        }
        async fn enrich_list_metadata(
            &self,
            _: &str,
            o: Vec<(String, FileMetadata)>,
        ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
            Ok(o)
        }
    }

    /// Shared helper: encrypt a plaintext blob into a chunked-format
    /// wire stream, return the bytes + the final_chunk_index.
    fn encode_chunked(key: &EncryptionKey, plaintext: &[u8]) -> (Vec<u8>, [u8; IV_LEN], u32) {
        let base_iv: [u8; IV_LEN] = [7u8; IV_LEN]; // fixed for determinism
        let mut out = Vec::new();
        out.extend_from_slice(&CHUNK_MAGIC);
        out.extend_from_slice(&base_iv);
        if plaintext.is_empty() {
            let frame = encrypt_chunk(key, &base_iv, 0, true, &[]).unwrap();
            out.extend_from_slice(&frame);
            return (out, base_iv, 0);
        }
        let final_idx = final_chunk_index_for_plaintext_size(plaintext.len() as u64);
        for (idx, pt_chunk) in plaintext.chunks(CHUNK_PLAINTEXT_SIZE).enumerate() {
            let is_final = idx as u32 == final_idx;
            let frame = encrypt_chunk(key, &base_iv, idx as u32, is_final, pt_chunk).unwrap();
            out.extend_from_slice(&frame);
        }
        (out, base_iv, final_idx)
    }

    /// Construct a CountingBackend pre-loaded with a chunked-encrypted
    /// object, wrap it in an EncryptingBackend, and return the wrapper
    /// plus the plaintext for assertion. Degenerate case of
    /// [`setup_shim_wrapper`] — no key_id on either side, no shim.
    async fn setup_wrapper_with_chunked_object(
        key: EncryptionKey,
        plaintext_size: usize,
    ) -> (EncryptingBackend<CountingBackend>, Vec<u8>) {
        setup_shim_wrapper(ShimSetup {
            primary_key: Some(key.clone()),
            primary_kid: None,
            write_mode: WriteMode::default(),
            legacy_key: None,
            legacy_kid: None,
            stamped_key: key,
            stamped_kid: None,
            plaintext_size,
        })
        .await
    }

    #[tokio::test]
    async fn test_range_read_fetches_only_target_chunks() {
        // 10 × 64 KiB plaintext = 640 KiB, chunks 0..=9. Request bytes
        // covering ONLY chunks 7-8. The wrapper must:
        //   1. Fetch the 16-byte header via a short range [0, 15].
        //   2. Fetch the body covering chunks 7-8 via a widened range
        //      starting at `wire_offset_of_chunk(7)`, NOT at 0.
        // Regression: before the fix, wire_start was hardcoded to 0
        // and the whole file up to last_chunk was pulled.
        let key = test_key();
        let plaintext_size = 10 * CHUNK_PLAINTEXT_SIZE;
        let (wrapper, plaintext) = setup_wrapper_with_chunked_object(key, plaintext_size).await;

        // Bytes covering chunks 7 (pt offset 458752..524287) and 8.
        let start = 7 * CHUNK_PLAINTEXT_SIZE as u64 + 100;
        let end = 8 * CHUNK_PLAINTEXT_SIZE as u64 + 500;
        let (stream, content_length) = wrapper
            .get_passthrough_stream_range("b", "p", "test.bin", start, end)
            .await
            .unwrap();
        assert_eq!(content_length, end - start + 1);

        use futures::TryStreamExt;
        let got: Vec<Bytes> = stream.try_collect().await.unwrap();
        let got: Vec<u8> = got.into_iter().flatten().collect();
        assert_eq!(got, &plaintext[start as usize..=end as usize]);

        // Now the assertion that justifies this test: the wrapper must
        // have made exactly TWO range requests to the inner backend —
        // one tiny one for the header, and one covering chunks 7-8.
        // It must NOT have fetched from wire offset 0 for the body
        // (that would mean pulling chunks 0-6 and throwing them away,
        // the pre-fix behaviour).
        let ranges = wrapper.inner.ranges_requested.lock().unwrap().clone();
        assert_eq!(
            ranges.len(),
            2,
            "expected exactly 2 inner range requests (header + body), got {}: {:?}",
            ranges.len(),
            ranges
        );
        assert_eq!(
            ranges[0],
            (0, CHUNK_HEADER_LEN as u64 - 1),
            "first request must be the 16-byte header fetch"
        );
        let body_wire_start = ranges[1].0;
        let expected_body_start = wire_offset_of_chunk(7);
        assert_eq!(
            body_wire_start, expected_body_start,
            "body fetch must start at wire_offset_of_chunk(first_chunk) = {}, \
             got {} — if this is 0, the wrapper is back to pulling from the \
             file start and the B3 perf fix is broken",
            expected_body_start, body_wire_start
        );
        // Body range must not extend past last_chunk's frame end (chunk
        // 8 is non-final since final is 9).
        let expected_body_end = wire_offset_of_chunk(8) + CHUNK_FRAME_WIRE_LEN as u64 - 1;
        assert_eq!(ranges[1].1, expected_body_end);
    }

    /// B4 regression: `get_passthrough` must return plaintext for BOTH
    /// wire formats. Before the fix, chunked-encrypted objects fell
    /// through to the single-shot `decrypt()` which parsed the 12-byte
    /// segment after the `DGE1` magic as an IV and AEAD-rejected —
    /// breaking any caller who called `get_passthrough` on a chunked
    /// object (latent footgun; no current production caller exercised
    /// the path).
    #[tokio::test]
    async fn test_get_passthrough_handles_chunked_objects() {
        let key = test_key();
        // Cross chunk boundaries — otherwise single-shot might
        // coincidentally look valid.
        let plaintext_size = 3 * CHUNK_PLAINTEXT_SIZE + 123;
        let (wrapper, plaintext) = setup_wrapper_with_chunked_object(key, plaintext_size).await;

        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(got, plaintext);
    }

    /// B9 regression: if xattrs are stripped during backup/restore,
    /// an on-disk chunked-encrypted body loses its `dg-encrypted`
    /// metadata marker. Without defense-in-depth, the wrapper would
    /// serve raw ciphertext as plaintext. The magic-sniff on the
    /// stream's first emission catches it and errors instead.
    #[tokio::test]
    async fn test_stripped_xattr_with_dge1_body_refuses_to_serve() {
        let key = test_key();
        let plaintext: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let (ciphertext, _iv, _final) = encode_chunked(&key, &plaintext);

        // Metadata says plaintext (no encryption marker) — simulates
        // the post-xattr-strip state.
        let meta = FileMetadata::fallback(
            "test.bin".into(),
            plaintext.len() as u64,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );

        let mut backend = CountingBackend::new();
        backend.set_contents(ciphertext, meta);

        let enc_config = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: None,
            key_id: None,
            ..Default::default()
        })));
        let wrapper = EncryptingBackend::new(backend, enc_config);

        let stream = wrapper
            .get_passthrough_stream("b", "p", "test.bin")
            .await
            .expect("stream open should succeed — the error surfaces on first pull");

        use futures::TryStreamExt;
        let res: Result<Vec<Bytes>, _> = stream.try_collect().await;
        let err = res.expect_err(
            "stream must error when body begins with DGE1 but metadata has no marker — \
             otherwise we'd serve ciphertext as plaintext after an xattr strip",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("xattrs") || msg.contains("dg-encrypted"),
            "error must explain the xattr-strip scenario, got: {msg}"
        );
    }

    /// Production bodies are `unfold` streams (s3_body_to_stream), which
    /// panic when polled after they end. An object shorter than the magic
    /// ends inside the sniff; the sniff must not poll it again.
    #[tokio::test]
    async fn dge1_sniff_never_polls_an_ended_stream() {
        use futures::TryStreamExt;
        let body = |data: &'static [u8]| {
            Box::pin(futures::stream::unfold(Some(data), |d| async move {
                d.map(|d| (Ok::<_, StorageError>(Bytes::from_static(d)), None))
            }))
        };
        for data in [&b"v0"[..], b"abc", b"abcd", b"abcdefg"] {
            let out: Vec<Bytes> = sniff_dge1_magic(body(data)).try_collect().await.unwrap();
            assert_eq!(out.concat(), data.to_vec());
        }
    }

    /// The sniff must see the magic even when the backend emits it split
    /// over chunks shorter than 4 bytes; the bytes pass through unchanged.
    #[tokio::test]
    async fn dge1_sniff_joins_short_leading_chunks() {
        use futures::TryStreamExt;
        let split = |parts: &[&'static [u8]]| {
            let items: Vec<Result<Bytes, StorageError>> =
                parts.iter().map(|p| Ok(Bytes::from_static(p))).collect();
            futures::stream::iter(items)
        };
        let res: Result<Vec<Bytes>, _> = sniff_dge1_magic(split(&[b"DG", b"E", b"1rest"]))
            .try_collect()
            .await;
        assert!(res.is_err(), "split magic must be caught");
        let ok: Vec<Bytes> = sniff_dge1_magic(split(&[b"ab", b"c", b"defg"]))
            .try_collect()
            .await
            .unwrap();
        assert_eq!(ok.concat(), b"abcdefg".to_vec());
        let short: Vec<Bytes> = sniff_dge1_magic(split(&[b"DG"]))
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            short.concat(),
            b"DG".to_vec(),
            "a 2-byte object is not ciphertext"
        );
        let empty: Vec<Bytes> = sniff_dge1_magic(split(&[])).try_collect().await.unwrap();
        assert!(empty.is_empty());
    }

    /// Tier 4: the range path must refuse a stripped-marker DGE1 body too.
    /// It delegated straight to the inner backend, so a range GET served
    /// ciphertext where a full GET refused.
    #[tokio::test]
    async fn test_stripped_xattr_with_dge1_body_refuses_range_reads() {
        use futures::TryStreamExt;
        let key = test_key();
        let plaintext: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let (ciphertext, _iv, _final) = encode_chunked(&key, &plaintext);
        let meta = FileMetadata::fallback(
            "test.bin".into(),
            ciphertext.len() as u64,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        for (configured, start) in [
            (None, 0u64),
            (Some(key.clone()), 0),
            (Some(key.clone()), 100),
        ] {
            let mut backend = CountingBackend::new();
            backend.set_contents(ciphertext.clone(), meta.clone());
            let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
                key: configured.clone(),
                key_id: None,
                ..Default::default()
            })));
            let wrapper = EncryptingBackend::new(backend, cfg);
            let res = match wrapper
                .get_passthrough_stream_range("b", "p", "test.bin", start, start + 50)
                .await
            {
                Ok((stream, _)) => stream.try_collect::<Vec<Bytes>>().await.map(|_| ()),
                Err(e) => Err(e),
            };
            assert!(
                res.is_err(),
                "key={} start={start}: range read served ciphertext",
                configured.is_some()
            );
        }
        // A plaintext object is still served by range.
        let mut backend = CountingBackend::new();
        backend.set_contents(plaintext.clone(), meta.clone());
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(key),
            key_id: None,
            ..Default::default()
        })));
        let wrapper = EncryptingBackend::new(backend, cfg);
        let (stream, _) = wrapper
            .get_passthrough_stream_range("b", "p", "test.bin", 100, 150)
            .await
            .unwrap();
        let got: Vec<Bytes> = stream.try_collect().await.unwrap();
        assert_eq!(got.concat(), plaintext[100..=150].to_vec());
    }

    /// Tier 4: a metadata-only rewrite must fail CLOSED when it cannot read
    /// the stored markers. It wrote the caller's metadata without them, so
    /// one transient HEAD error made an encrypted object unreadable.
    #[tokio::test]
    async fn put_passthrough_metadata_fails_closed_without_the_stored_markers() {
        let key = test_key();
        let (ciphertext, _iv, _f) = encode_chunked(&key, b"secret body");
        let mut stored = FileMetadata::fallback(
            "o".into(),
            11,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_chunked_encrypted(&mut stored, None);
        let mut backend = CountingBackend::new();
        backend.set_contents(ciphertext, stored.clone());
        backend
            .fail_meta_reads
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(key),
            key_id: None,
            ..Default::default()
        })));
        let wrapper = EncryptingBackend::new(backend, cfg);
        let mut rewrite = stored.clone();
        strip_encryption_markers(&mut rewrite.user_metadata);
        assert!(wrapper
            .put_passthrough_metadata("b", "p", "o", &rewrite)
            .await
            .is_err());
        let now = wrapper.inner.metadata.lock().unwrap().clone().unwrap();
        assert!(is_chunked_encrypted(&now), "the stored marker must survive");
    }

    #[tokio::test]
    async fn test_range_read_last_bytes_of_large_object_bounded_fetch() {
        // The scenario that motivates B3: "last 100 bytes of a large
        // object" must NOT pull the whole file. Here large=64×64KiB=4
        // MiB; the principle is identical for 10 GiB.
        let key = test_key();
        let plaintext_size = 64 * CHUNK_PLAINTEXT_SIZE;
        let (wrapper, plaintext) = setup_wrapper_with_chunked_object(key, plaintext_size).await;

        let start = (plaintext_size - 100) as u64;
        let end = (plaintext_size - 1) as u64;
        let (stream, content_length) = wrapper
            .get_passthrough_stream_range("b", "p", "test.bin", start, end)
            .await
            .unwrap();
        assert_eq!(content_length, 100);

        use futures::TryStreamExt;
        let got: Vec<Bytes> = stream.try_collect().await.unwrap();
        let got: Vec<u8> = got.into_iter().flatten().collect();
        assert_eq!(got, &plaintext[start as usize..=end as usize]);

        let ranges = wrapper.inner.ranges_requested.lock().unwrap().clone();
        assert_eq!(ranges.len(), 2, "expected header + body = 2 requests");
        let body_start = ranges[1].0;
        let last_chunk = final_chunk_index_for_plaintext_size(plaintext_size as u64);
        let expected = wire_offset_of_chunk(last_chunk);
        assert_eq!(
            body_start, expected,
            "must seek to the last chunk's boundary, not file start. \
             expected {}, got {}",
            expected, body_start
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Step 3: key_id end-to-end — round-trip, mismatch, legacy object
    // ─────────────────────────────────────────────────────────────────

    /// Reconstruct a chunked-encrypted object in memory, wrap with a
    /// given (key, key_id) pair, and return the wrapper + expected
    /// plaintext. Differs from `setup_wrapper_with_chunked_object`:
    /// this variant writes the `dg-encryption-key-id` metadata stamp
    /// directly so we can exercise the READ path's mismatch check
    /// without needing a write cycle. Degenerate case of
    /// [`setup_shim_wrapper`] — no legacy shim, primary-only.
    async fn setup_wrapper_with_stamped_object(
        key: EncryptionKey,
        stamped_kid: Option<&'static str>,
        plaintext_size: usize,
        wrapper_key_id: Option<String>,
    ) -> (EncryptingBackend<CountingBackend>, Vec<u8>) {
        setup_shim_wrapper(ShimSetup {
            primary_key: Some(key.clone()),
            primary_kid: wrapper_key_id,
            write_mode: WriteMode::default(),
            legacy_key: None,
            legacy_kid: None,
            stamped_key: key,
            stamped_kid,
            plaintext_size,
        })
        .await
    }

    #[tokio::test]
    async fn test_read_succeeds_when_key_ids_match() {
        let key = test_key();
        let (wrapper, plaintext) = setup_wrapper_with_stamped_object(
            key,
            Some("matching-id"),
            4096,
            Some("matching-id".to_string()),
        )
        .await;
        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(got, plaintext);
    }

    #[tokio::test]
    async fn test_read_fails_with_specific_error_when_key_ids_mismatch() {
        // Object says "written with key_id A"; wrapper says "I have
        // key_id B". The AEAD would fail with an opaque message —
        // the specific error must fire FIRST.
        let key = test_key();
        let (wrapper, _plaintext) = setup_wrapper_with_stamped_object(
            key,
            Some("object-a"),
            4096,
            Some("wrapper-b".to_string()),
        )
        .await;

        let res = wrapper.get_passthrough("b", "p", "test.bin").await;
        let err = match res {
            Ok(_) => panic!("must error on key_id mismatch"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("object-a") && msg.contains("wrapper-b"),
            "must cite both key ids, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_read_fails_via_streaming_range_when_key_ids_mismatch() {
        // Same mismatch, this time through get_passthrough_stream_range.
        // The check fires at stream-open time — BEFORE any AEAD
        // attempt — so the error surfaces as a failed open, not a
        // mid-stream fail.
        let key = test_key();
        let (wrapper, _plaintext) = setup_wrapper_with_stamped_object(
            key,
            Some("object-c"),
            4096,
            Some("wrapper-d".to_string()),
        )
        .await;

        let res = wrapper
            .get_passthrough_stream_range("b", "p", "test.bin", 0, 99)
            .await;
        let err = match res {
            Ok(_) => panic!("range-read must error on key_id mismatch"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("object-c") && msg.contains("wrapper-d"),
            "range-read mismatch must also cite both ids, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_legacy_object_without_key_id_still_decrypts() {
        // Object written before Step 3 has no `dg-encryption-key-id`
        // stamp. The wrapper has a key_id today. The check is
        // conditional — one-sided absence is legal — so decrypt
        // succeeds as long as the key material matches.
        let key = test_key();
        let (wrapper, plaintext) = setup_wrapper_with_stamped_object(
            key,
            None, // pre-Step-3 object
            4096,
            Some("current-wrapper-id".to_string()),
        )
        .await;
        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(
            got, plaintext,
            "legacy objects must decrypt when the key itself still matches"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Step 5: decrypt-only shim + WriteMode::PassThrough
    // ─────────────────────────────────────────────────────────────────

    /// Inputs for the shim-wrapper fixture. Struct keeps the param
    /// count under clippy's `too_many_arguments` threshold while
    /// still being readable at call sites (field names document
    /// intent better than positional args).
    struct ShimSetup {
        primary_key: Option<EncryptionKey>,
        primary_kid: Option<String>,
        write_mode: WriteMode,
        legacy_key: Option<EncryptionKey>,
        legacy_kid: Option<String>,
        stamped_key: EncryptionKey,
        stamped_kid: Option<&'static str>,
        plaintext_size: usize,
    }

    /// Build a wrapper with a two-key config (primary + legacy shim)
    /// around a CountingBackend pre-loaded with a chunked-encrypted
    /// object encrypted under `stamped_key` + `stamped_kid`.
    async fn setup_shim_wrapper(s: ShimSetup) -> (EncryptingBackend<CountingBackend>, Vec<u8>) {
        let plaintext: Vec<u8> = (0..s.plaintext_size).map(|i| (i & 0xff) as u8).collect();
        let (ciphertext, _iv, _final_idx) = encode_chunked(&s.stamped_key, &plaintext);
        let mut meta = FileMetadata::fallback(
            "test.bin".into(),
            s.plaintext_size as u64,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        mark_chunked_encrypted(&mut meta, s.stamped_kid);
        let mut backend = CountingBackend::new();
        backend.set_contents(ciphertext, meta);
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: s.primary_key,
            key_id: s.primary_kid,
            write_mode: s.write_mode,
            legacy_key: s.legacy_key,
            legacy_key_id: s.legacy_kid,
        })));
        (EncryptingBackend::new(backend, cfg), plaintext)
    }

    #[tokio::test]
    async fn test_shim_decrypts_legacy_stamped_objects() {
        // Scenario: operator migrated from aes256-gcm-proxy (key=K1,
        // id=id-K1) to sse-kms (on S3 side), keeping K1/id-K1 as the
        // legacy shim. A historical object is stamped with id-K1;
        // the wrapper has NO primary key (native mode), write_mode
        // PassThrough. Read must succeed by matching against the
        // legacy shim.
        let k1 = test_key();
        let k1_for_stamp = test_key(); // same bytes, independent allocation
        let (wrapper, plaintext) = setup_shim_wrapper(ShimSetup {
            primary_key: None, // native mode primary
            primary_kid: None,
            write_mode: WriteMode::PassThrough,
            legacy_key: Some(k1),
            legacy_kid: Some("id-K1".to_string()),
            stamped_key: k1_for_stamp,
            stamped_kid: Some("id-K1"),
            plaintext_size: 4096,
        })
        .await;
        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(got, plaintext);
    }

    #[tokio::test]
    async fn test_shim_primary_key_takes_precedence() {
        // Wrapper has BOTH primary and legacy keys configured. An
        // object stamped with the primary's id decrypts under the
        // primary; the legacy is never consulted. Guards against
        // ambiguous routing where both happen to have the same id
        // (the collision-detect path elsewhere prevents this, but
        // this test pins the tie-break in the wrapper itself).
        let k_primary = test_key();
        let k_primary_stamp = test_key();
        let k_legacy = other_key();
        let (wrapper, plaintext) = setup_shim_wrapper(ShimSetup {
            primary_key: Some(k_primary),
            primary_kid: Some("id-primary".to_string()),
            write_mode: WriteMode::Encrypt,
            legacy_key: Some(k_legacy),
            legacy_kid: Some("id-legacy".to_string()),
            stamped_key: k_primary_stamp,
            stamped_kid: Some("id-primary"),
            plaintext_size: 4096,
        })
        .await;
        let got = wrapper.get_passthrough("b", "p", "test.bin").await.unwrap();
        assert_eq!(got, plaintext);
    }

    #[tokio::test]
    async fn test_shim_legacy_id_only_fires_when_object_matches() {
        // Object stamped with an id that matches NEITHER primary nor
        // legacy. Must error with the specific "rotated without
        // legacy_key" message so the operator knows what's missing.
        let k_primary = test_key();
        let k_orphan_stamp = other_key(); // stamped with DIFFERENT key material
        let (wrapper, _plaintext) = setup_shim_wrapper(ShimSetup {
            primary_key: Some(k_primary),
            primary_kid: Some("id-primary".to_string()),
            write_mode: WriteMode::Encrypt,
            legacy_key: None, // NO legacy — expected to fail hard
            legacy_kid: None,
            stamped_key: k_orphan_stamp,
            stamped_kid: Some("id-orphan"),
            plaintext_size: 4096,
        })
        .await;
        let err = match wrapper.get_passthrough("b", "p", "test.bin").await {
            Ok(_) => panic!("must error when object id matches nothing"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("id-orphan") && msg.contains("id-primary"),
            "error must cite BOTH ids so the operator can diagnose, got: {msg}"
        );
        assert!(
            msg.contains("legacy_key"),
            "error must mention legacy_key as the recovery path, got: {msg}"
        );
    }

    #[test]
    fn test_write_mode_passthrough_skips_encryption() {
        // Direct unit test on `encrypt_if_enabled` via a wrapper.
        // WriteMode::PassThrough must return the plaintext verbatim
        // and NOT stamp the `dg-encrypted` marker — even when a
        // primary key is configured. This is the native-SSE
        // transition invariant: writes go through the proxy wrapper
        // unchanged while old objects still decrypt via the shim.
        let key = test_key();
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(key),
            key_id: Some("current".into()),
            write_mode: WriteMode::PassThrough,
            ..Default::default()
        })));
        let wrapper: EncryptingBackend<CountingBackend> =
            EncryptingBackend::new(CountingBackend::new(), cfg);

        let mut meta = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        let plaintext = b"no encrypt for me";
        let out = wrapper.encrypt_if_enabled(plaintext, &mut meta).unwrap();
        assert_eq!(out, plaintext, "PassThrough must return plaintext verbatim");
        assert!(
            !is_encrypted(&meta),
            "PassThrough must NOT stamp dg-encrypted marker"
        );
        assert!(
            stamped_key_id(&meta).is_none(),
            "PassThrough must NOT stamp dg-encryption-key-id"
        );
    }

    #[test]
    fn test_write_mode_encrypt_still_works_normally() {
        // The Encrypt default continues to encrypt + stamp markers.
        // Regression guard that the Default impl on WriteMode is
        // Encrypt (not PassThrough), which the struct-update
        // `..Default::default()` calls in this module rely on.
        let key = test_key();
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(key),
            key_id: Some("id".into()),
            ..Default::default()
        })));
        assert_eq!(cfg.load().write_mode, WriteMode::Encrypt);
        let wrapper: EncryptingBackend<CountingBackend> =
            EncryptingBackend::new(CountingBackend::new(), cfg);

        let mut meta = FileMetadata::fallback(
            "x".into(),
            10,
            "md5".into(),
            Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        let out = wrapper.encrypt_if_enabled(b"secret", &mut meta).unwrap();
        assert_ne!(out, b"secret", "Encrypt must actually encrypt");
        assert!(is_encrypted(&meta), "Encrypt must stamp dg-encrypted");
        assert_eq!(
            stamped_key_id(&meta),
            Some("id"),
            "Encrypt must stamp the key_id"
        );
    }

    #[test]
    fn test_pick_decrypt_key_legacy_object_uses_primary() {
        // Pre-Step-3 objects have no stamp. `pick_decrypt_key`
        // returns the primary key (legacy shim is ignored when the
        // object has no id to match).
        let k_primary = test_key();
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(k_primary),
            key_id: Some("primary-id".into()),
            legacy_key: Some(other_key()),
            legacy_key_id: Some("legacy-id".into()),
            ..Default::default()
        })));
        let wrapper: EncryptingBackend<CountingBackend> =
            EncryptingBackend::new(CountingBackend::new(), cfg);
        // `None` = the object had no `dg-encryption-key-id` stamp.
        let picked = wrapper.pick_decrypt_key(None).unwrap();
        // Assert it's the PRIMARY key by re-encrypting with it and
        // decrypting a round-trip.
        let ct = encrypt(&picked, b"hello").unwrap();
        assert_eq!(decrypt(&test_key(), &ct).unwrap(), b"hello");
    }

    #[test]
    fn test_pick_decrypt_key_no_primary_no_legacy_errors() {
        // Wrapper has no key at all. `pick_decrypt_key` called for
        // an encrypted object must error with the specific "no
        // encryption key configured" message (H6 fix). The earlier
        // text said "no legacy-shim match either", which misled
        // operators whose actual problem was a completely missing
        // key, not a mis-rotation.
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig::default())));
        let wrapper: EncryptingBackend<CountingBackend> =
            EncryptingBackend::new(CountingBackend::new(), cfg);
        let err = match wrapper.pick_decrypt_key(Some("some-id")) {
            Ok(_) => panic!("must error when wrapper has no keys"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("some-id") && msg.contains("NO encryption key"),
            "error must cite the object id and the specific 'no key at all' hint, got: {msg}"
        );
        // Must NOT cite the rotation-shaped hint — this isn't that case.
        assert!(
            !msg.contains("rotated"),
            "no-key-at-all case must not quote the rotation hint (H6): {msg}"
        );
    }

    #[test]
    fn test_pick_decrypt_key_primary_set_but_wrong_id_errors_with_rotation_hint() {
        // Wrapper HAS a primary key, but it doesn't match the object's
        // stamped id. Error must cite the rotation-shaped hint (not
        // the "no key at all" H6 variant).
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(test_key()),
            key_id: Some("configured-id".into()),
            ..Default::default()
        })));
        let wrapper: EncryptingBackend<CountingBackend> =
            EncryptingBackend::new(CountingBackend::new(), cfg);
        let err = match wrapper.pick_decrypt_key(Some("object-id-X")) {
            Ok(_) => panic!("must error on id mismatch"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("object-id-X") && msg.contains("configured-id"),
            "mismatch error must cite both ids, got: {msg}"
        );
        assert!(
            msg.contains("rotated") || msg.contains("legacy-shim"),
            "mismatch error must point to the rotation/routing remedies, got: {msg}"
        );
    }

    /// Every write path must honour `WriteMode::PassThrough` even when a key
    /// is configured (the proxy-AES → native-SSE transition). The chunked
    /// path used to check only for the key.
    #[tokio::test]
    async fn chunked_write_honours_passthrough_mode_with_a_key() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = crate::storage::FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        inner.create_bucket("enc-bkt").await.unwrap();
        let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(test_key()),
            key_id: Some("current".into()),
            write_mode: WriteMode::PassThrough,
            ..Default::default()
        })));
        let wrapper = EncryptingBackend::new(inner, cfg);
        let plaintext = b"chunked plaintext".to_vec();
        let meta = FileMetadata::new_passthrough(
            "obj.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            plaintext.len() as u64,
            None,
        );
        wrapper
            .put_passthrough_chunked(
                "enc-bkt",
                "",
                "obj.bin",
                &[Bytes::from(plaintext.clone())],
                &meta,
            )
            .await
            .unwrap();

        let raw = wrapper
            .inner
            .get_passthrough("enc-bkt", "", "obj.bin")
            .await
            .unwrap();
        assert_eq!(raw, plaintext, "PassThrough must store the plaintext");
        let stored = wrapper
            .inner
            .get_passthrough_metadata("enc-bkt", "", "obj.bin")
            .await
            .unwrap();
        assert!(
            !is_encrypted(&stored),
            "PassThrough must not stamp a marker"
        );
    }

    /// S15: client metadata that names the wrapper's markers must not decide
    /// how the body is read. A client `x-amz-meta-dg-encrypted` (e.g. a sync
    /// tool that copies metadata from a GET into the next PUT) was stored
    /// verbatim, and the object became unreadable.
    #[tokio::test]
    async fn client_supplied_markers_never_reach_storage() {
        for (mode_name, key) in [("no-key", None), ("encrypt", Some(test_key()))] {
            let tmp = tempfile::tempdir().unwrap();
            let inner = crate::storage::FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap();
            inner.create_bucket("b").await.unwrap();
            let cfg = Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
                key,
                key_id: None,
                ..Default::default()
            })));
            let wrapper = EncryptingBackend::new(inner, cfg);
            let body = b"client body".to_vec();
            let mut meta = FileMetadata::new_passthrough(
                "x".into(),
                "0".repeat(64),
                "0".repeat(32),
                body.len() as u64,
                None,
            );
            meta.user_metadata
                .insert(ENCRYPTION_MARKER_KEY.into(), CHUNK_MARKER_VALUE.into());
            meta.user_metadata
                .insert(ENCRYPTION_KEY_ID_KEY.into(), "forged-kid".into());
            meta.user_metadata.insert("team".into(), "ci".into());

            let src = tmp.path().join("src.bin");
            tokio::fs::write(&src, &body).await.unwrap();
            wrapper
                .put_passthrough("b", "p", "a", &body, &meta)
                .await
                .unwrap();
            wrapper
                .put_passthrough_file("b", "p", "c", &src, &meta, unheld(&test_spool(&tmp)))
                .await
                .unwrap();
            wrapper
                .put_passthrough_chunked("b", "p", "d", &[Bytes::from(body.clone())], &meta)
                .await
                .unwrap();
            wrapper
                .put_passthrough_parts(
                    "b",
                    "p",
                    "e",
                    std::slice::from_ref(&src),
                    &meta,
                    unheld(&test_spool(&tmp)),
                )
                .await
                .unwrap();
            wrapper
                .put_delta(
                    "b",
                    "p",
                    "f",
                    &body,
                    &meta,
                    crate::deltaglider::RefWriteProof::for_tests(),
                )
                .await
                .unwrap();
            wrapper
                .put_reference(
                    "b",
                    "p",
                    &body,
                    &meta,
                    crate::deltaglider::RefWriteProof::for_tests(),
                )
                .await
                .unwrap();
            for f in ["a", "c", "d", "e"] {
                assert_eq!(
                    wrapper.get_passthrough("b", "p", f).await.unwrap(),
                    body,
                    "{mode_name}: passthrough {f} must read back"
                );
                let stored = wrapper.get_passthrough_metadata("b", "p", f).await.unwrap();
                assert_ne!(
                    stamped_key_id(&stored),
                    Some("forged-kid"),
                    "{mode_name}: {f}"
                );
                assert_eq!(
                    stored.user_metadata.get("team").map(String::as_str),
                    Some("ci")
                );
            }
            assert_eq!(
                wrapper.get_delta("b", "p", "f").await.unwrap(),
                body,
                "{mode_name}"
            );
            assert_eq!(
                wrapper.get_reference("b", "p").await.unwrap(),
                body,
                "{mode_name}"
            );

            // A metadata-only rewrite must not plant a marker either.
            wrapper
                .put_passthrough_metadata("b", "p", "a", &meta)
                .await
                .unwrap();
            assert_eq!(
                wrapper.get_passthrough("b", "p", "a").await.unwrap(),
                body,
                "{mode_name}: metadata rewrite"
            );
        }
    }

    fn keyed_cfg() -> Arc<ArcSwap<EncryptionConfig>> {
        Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(test_key()),
            key_id: Some("kid-1".to_string()),
            ..Default::default()
        })))
    }

    fn test_spool(dir: &tempfile::TempDir) -> crate::deltaglider::spool::SpoolDir {
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 64 * 1024 * 1024)
            .unwrap()
    }

    fn unheld(sd: &crate::deltaglider::spool::SpoolDir) -> SpoolBudget<'_> {
        SpoolBudget::new(sd, None, None)
    }

    fn probe_wrapper(
        sd: &crate::deltaglider::spool::SpoolDir,
    ) -> EncryptingBackend<CountingBackend> {
        let mut inner = CountingBackend::new();
        inner.probe_spool = Some(sd.clone());
        EncryptingBackend::new(inner, keyed_cfg())
    }

    fn meta_of(len: usize) -> FileMetadata {
        FileMetadata::new_passthrough("o".into(), "0".repeat(64), "0".repeat(32), len as u64, None)
    }

    const MIB: usize = 1024 * 1024;

    /// The ciphertext temp file of an encrypted file PUT must live in the
    /// spool dir and hold spool budget while the inner backend reads it.
    /// It was a `NamedTempFile::new()` in the system temp dir, outside the
    /// budget, so N large encrypted PUTs could fill the disk.
    #[tokio::test]
    async fn encrypted_file_put_temp_is_inside_the_spool_budget() {
        let dir = tempfile::tempdir().unwrap();
        let sd = test_spool(&dir);
        let wrapper = probe_wrapper(&sd);
        let body = vec![7u8; 3 * MIB];
        let src = dir.path().join("src");
        std::fs::write(&src, &body).unwrap();

        wrapper
            .put_passthrough_file("b", "p", "o", &src, &meta_of(body.len()), unheld(&sd))
            .await
            .unwrap();
        let puts = wrapper.inner.file_puts.lock().unwrap().clone();
        assert_eq!(puts.len(), 1);
        assert_eq!(
            puts[0].0.parent(),
            Some(sd.dir()),
            "ciphertext temp must be in the spool dir"
        );
        assert!(
            puts[0].1 <= 64 - 4,
            "ciphertext temp must hold its budget (3 MiB body -> 4 MiB), free {} MiB",
            puts[0].1
        );
        assert_eq!(sd.free_mib(), 64, "budget released after the write");
    }

    /// Same for relayed parts: the joined plaintext AND the ciphertext.
    #[tokio::test]
    async fn encrypted_parts_put_temps_are_inside_the_spool_budget() {
        let dir = tempfile::tempdir().unwrap();
        let sd = test_spool(&dir);
        let wrapper = probe_wrapper(&sd);
        let parts: Vec<std::path::PathBuf> = (0..3)
            .map(|i| {
                let p = dir.path().join(format!("part{i}"));
                std::fs::write(&p, vec![i as u8; MIB]).unwrap();
                p
            })
            .collect();

        // With the engine's up-front reservation: both files share it.
        let need = wrapper
            .file_put_spool_bytes("b", 3 * MIB as u64, true)
            .await;
        assert!(need > 6 * MIB as u64, "joined + ciphertext, got {need}");
        let reserved = sd.reserve_beside(0, need).await.unwrap();
        let free_after_reserve = sd.free_mib();
        wrapper
            .put_passthrough_parts(
                "b",
                "p",
                "o",
                &parts,
                &meta_of(3 * MIB),
                SpoolBudget::new(&sd, None, Some(&reserved)),
            )
            .await
            .unwrap();
        let puts = wrapper.inner.file_puts.lock().unwrap().clone();
        assert_eq!(puts[0].0.parent(), Some(sd.dir()));
        assert_eq!(
            puts[0].1, free_after_reserve,
            "the files use the reservation, not more budget"
        );
        drop(reserved);
        assert_eq!(sd.free_mib(), 64);

        // Without a reservation: the wrapper takes free budget now.
        wrapper
            .put_passthrough_parts("b", "p", "o", &parts, &meta_of(3 * MIB), unheld(&sd))
            .await
            .unwrap();
        let puts = wrapper.inner.file_puts.lock().unwrap().clone();
        assert!(
            puts[1].1 <= 64 - 7,
            "joined 3 + ciphertext 4 MiB, free {}",
            puts[1].1
        );
        assert_eq!(sd.free_mib(), 64);
    }

    /// The review2 rule: a storage write never waits for spool budget. It
    /// runs under the engine's deltaspace lock, and other ops hold budget
    /// while they wait for that lock. With the budget taken, the write fails
    /// retryably (503 SlowDown) at once.
    #[tokio::test]
    async fn encrypted_file_put_never_waits_for_budget() {
        let dir = tempfile::tempdir().unwrap();
        let sd = crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 4 * MIB as u64)
            .unwrap();
        let wrapper = probe_wrapper(&sd);
        let src = dir.path().join("src");
        std::fs::write(&src, vec![1u8; 2 * MIB]).unwrap();

        // Another op holds the whole budget.
        let other = sd.acquire(4 * MIB as u64).await.unwrap();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wrapper.put_passthrough_file("b", "p", "o", &src, &meta_of(2 * MIB), unheld(&sd)),
        )
        .await
        .expect("a storage write must not wait for spool budget");
        assert!(matches!(r, Err(StorageError::Throttled(_))), "got {r:?}");
        drop(other);

        // Two holders, each with half the budget, write at the same time:
        // neither waits for the other's half.
        let a = sd.acquire(2 * MIB as u64).await.unwrap();
        let b = sd.acquire(2 * MIB as u64).await.unwrap();
        let (ra, rb) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            futures::future::join(
                wrapper.put_passthrough_file(
                    "b",
                    "p",
                    "a",
                    &src,
                    &meta_of(2 * MIB),
                    SpoolBudget::new(&sd, Some(&a), None),
                ),
                wrapper.put_passthrough_file(
                    "b",
                    "p",
                    "b",
                    &src,
                    &meta_of(2 * MIB),
                    SpoolBudget::new(&sd, Some(&b), None),
                ),
            ),
        )
        .await
        .expect("two holders deadlocked on each other's budget");
        for r in [ra, rb] {
            assert!(matches!(r, Err(StorageError::Throttled(_))), "got {r:?}");
        }
        drop((a, b));
        assert_eq!(sd.free_mib(), 4);
    }

    #[tokio::test]
    async fn file_put_spool_bytes_is_zero_without_a_write_key() {
        let plain = EncryptingBackend::new(
            CountingBackend::new(),
            Arc::new(ArcSwap::new(Arc::new(EncryptionConfig::default()))),
        );
        assert_eq!(plain.file_put_spool_bytes("b", 1 << 30, true).await, 0);
        let keyed = EncryptingBackend::new(CountingBackend::new(), keyed_cfg());
        assert_eq!(
            keyed.file_put_spool_bytes("b", 0, false).await,
            chunked_wire_len_bound(0)
        );
    }
}

/// storage-7: a read decides encrypted-or-plaintext from the markers of its
/// own backend response, so it sends no separate metadata HEAD.
#[cfg(test)]
mod read_request_tests {
    use super::*;
    use crate::storage::fake_s3;
    use futures::TryStreamExt;

    fn cfg(key: Option<EncryptionKey>) -> Arc<ArcSwap<EncryptionConfig>> {
        let key_id = key.as_ref().map(|_| "kid-1".to_string());
        Arc::new(ArcSwap::new(Arc::new(EncryptionConfig {
            key,
            key_id,
            ..Default::default()
        })))
    }

    fn key() -> EncryptionKey {
        EncryptionKey::from_hex(&"42".repeat(32)).unwrap()
    }

    fn meta(data: &[u8]) -> FileMetadata {
        FileMetadata::new_passthrough(
            "a.bin".into(),
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(data)),
            hex::encode(<md5::Md5 as md5::Digest>::digest(data)),
            data.len() as u64,
            None,
        )
    }

    async fn wrapped(
        key: Option<EncryptionKey>,
    ) -> (
        EncryptingBackend<crate::storage::S3Backend>,
        Arc<fake_s3::FakeS3>,
    ) {
        let (ep, fake) = fake_s3::start().await;
        let s3 = crate::storage::s3::test_support::for_test_endpoint(&ep);
        (EncryptingBackend::new(s3, cfg(key)), fake)
    }

    fn one_get(requests: &[String]) -> bool {
        requests.len() == 1 && requests[0].starts_with("GET ")
    }

    #[tokio::test]
    async fn a_passthrough_stream_read_is_one_get() {
        for k in [None, Some(key())] {
            let (w, fake) = wrapped(k.clone()).await;
            let body = b"hello passthrough".to_vec();
            w.put_passthrough("b", "p", "a.bin", &body, &meta(&body))
                .await
                .unwrap();
            fake.clear();
            let got: Vec<Bytes> = w
                .get_passthrough_stream("b", "p", "a.bin")
                .await
                .unwrap()
                .try_collect()
                .await
                .unwrap();
            assert_eq!(got.concat(), body, "key: {}", k.is_some());
            assert!(one_get(&fake.requests()), "{:?}", fake.requests());
            fake.clear();
            assert_eq!(w.get_passthrough("b", "p", "a.bin").await.unwrap(), body);
            assert!(one_get(&fake.requests()), "{:?}", fake.requests());
        }
    }

    #[tokio::test]
    async fn delta_and_reference_reads_are_one_get() {
        for k in [None, Some(key())] {
            let (w, fake) = wrapped(k.clone()).await;
            let body = b"delta bytes".to_vec();
            w.put_delta(
                "b",
                "p",
                "a.bin",
                &body,
                &meta(&body),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
            w.put_reference(
                "b",
                "p",
                &body,
                &meta(&body),
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
            fake.clear();
            assert_eq!(w.get_delta("b", "p", "a.bin").await.unwrap(), body);
            assert!(one_get(&fake.requests()), "{:?}", fake.requests());
            fake.clear();
            assert_eq!(w.get_reference("b", "p").await.unwrap(), body);
            assert!(one_get(&fake.requests()), "{:?}", fake.requests());
        }
    }
}
