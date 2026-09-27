// SPDX-License-Identifier: BUSL-1.1

use super::*;

/// X-ray H17: CopyObject source-read auth must honor IP-scoped conditions.
/// The context-free can() ignored aws:SourceIp; can_with_context + the
/// threaded client IP fixes it.
#[test]
fn copy_source_access_honors_source_ip_condition() {
    use crate::iam::permissions::permission_to_iam_policy;
    use crate::iam::types::Permission;
    // Allow * ; Deny read when SourceIp ∈ 10.0.0.0/8.
    let allow = Permission {
        id: 0,
        effect: "Allow".into(),
        actions: vec!["*".into()],
        resources: vec!["*".into()],
        conditions: None,
    };
    let deny_ip = Permission {
        id: 1,
        effect: "Deny".into(),
        actions: vec!["read".into()],
        resources: vec!["*".into()],
        conditions: Some(serde_json::json!({"IpAddress": {"aws:SourceIp": "10.0.0.0/8"}})),
    };
    let user = AuthenticatedUser {
        name: "u".into(),
        access_key_id: "AKIA".into(),
        permissions: vec![],
        iam_policies: [allow, deny_ip]
            .iter()
            .map(permission_to_iam_policy)
            .collect(),
    };
    // From a denied IP → AccessDenied (was silently allowed before the fix).
    assert!(
        check_copy_source_access_s3s(
            Some(&user),
            "src",
            "k",
            Some("10.1.2.3".parse().unwrap()),
            &axum::http::HeaderMap::new()
        )
        .is_err(),
        "IP-scoped Deny on the source must be enforced"
    );
    // From an allowed IP → Ok.
    assert!(
        check_copy_source_access_s3s(
            Some(&user),
            "src",
            "k",
            Some("192.168.1.1".parse().unwrap()),
            &axum::http::HeaderMap::new()
        )
        .is_ok(),
        "an IP outside the Deny CIDR must be allowed"
    );
}

#[test]
fn list_metadata_goes_into_its_own_contents_block() {
    let ext = ListMetadataXmlExtensions(std::collections::HashMap::from([
        (
            "a&b".to_string(),
            std::collections::HashMap::from([("k".to_string(), "<v>".to_string())]),
        ),
        ("z".to_string(), std::collections::HashMap::new()),
        (
            "c".to_string(),
            std::collections::HashMap::from([
                ("y".to_string(), "2".to_string()),
                ("x".to_string(), "1".to_string()),
            ]),
        ),
    ]));
    let xml = "<R><Contents><Key>a&amp;b</Key><Size>1</Size></Contents>\
               <Contents><Key>z</Key></Contents><Contents><Key>c</Key></Contents>\
               <CommonPrefixes><Prefix>p/</Prefix></CommonPrefixes></R>";
    assert_eq!(
        ext.insert_into(xml),
        "<R><Contents><Key>a&amp;b</Key><Size>1</Size><UserMetadata><Items><Key>k</Key>\
         <Value>&lt;v&gt;</Value></Items></UserMetadata></Contents><Contents><Key>z</Key>\
         </Contents><Contents><Key>c</Key><UserMetadata><Items><Key>x</Key><Value>1</Value>\
         </Items><Items><Key>y</Key><Value>2</Value></Items></UserMetadata></Contents>\
         <CommonPrefixes><Prefix>p/</Prefix></CommonPrefixes></R>"
    );
}

#[test]
fn v2_tokens_are_opaque_and_accept_the_raw_form() {
    let cursor = |t: &str| decode_v2_token(Some(t)).map(|c| (c.key, c.legacy));
    for key in ["p/a.txt", "ctl/a\u{1}b.txt", "", "é/<x>&"] {
        let t = encode_v2_token(key);
        assert!(t.bytes().all(|b| b.is_ascii_graphic()), "{t}");
        assert_eq!(cursor(&t), Some((key.to_string(), false)));
    }
    // The raw fallback says so, so the caller can count it.
    assert_eq!(cursor("p/a.txt"), Some(("p/a.txt".into(), true)));
    assert_eq!(cursor("dg1.!!"), Some(("dg1.!!".into(), true)));
    assert!(decode_v2_token(None).is_none());
}

/// Each raw-token fallback bumps the counter that tells when the
/// fallback can go; an opaque token does not.
#[test]
fn the_raw_token_fallback_is_counted() {
    let counter = &*super::list::LEGACY_V2_TOKENS;
    let before = counter.get();
    assert_eq!(
        super::list::v2_cursor("b", Some("p/a.txt")).as_deref(),
        Some("p/a.txt")
    );
    assert!(counter.get() > before, "the fallback must be counted");
}

#[test]
fn list_cursor_token_wins_over_start_after() {
    assert_eq!(list_cursor(None, None), None);
    assert_eq!(list_cursor(None, Some("b")), Some("b"));
    assert_eq!(list_cursor(Some("c"), Some("b")), Some("c"));
    assert_eq!(list_cursor(Some(""), Some("b")), Some("b"));
    assert_eq!(list_cursor(None, Some("")), None);
}

#[test]
fn url_encoding_matches_s3() {
    assert_eq!(s3_url_encode("a/b-c_d.e~f"), "a/b-c_d.e~f");
    assert_eq!(s3_url_encode("a+b c%41"), "a%2Bb%20c%2541");
    assert_eq!(s3_url_encode("é"), "%C3%A9");
    assert_eq!(s3_url_encode("x&y<z>"), "x%26y%3Cz%3E");
}

#[test]
fn sandbox_applies_to_active_content_types() {
    for active in [
        "text/html",
        "TEXT/HTML; charset=utf-8",
        "image/svg+xml",
        "application/xhtml+xml",
        "text/xml",
        "application/octet-stream",
        "text/plain",
        "",
    ] {
        assert!(content_type_needs_sandbox(active), "{active:?}");
    }
    for inert in [
        "image/png",
        "image/png; q=1",
        "video/mp4",
        "audio/mpeg",
        "application/pdf",
    ] {
        assert!(!content_type_needs_sandbox(inert), "{inert:?}");
    }
}

#[test]
fn s3s_decodes_only_header_signed_sigv4() {
    let mut h = axum::http::HeaderMap::new();
    assert!(!s3s_decoded_aws_chunked(&h, false));
    h.insert(
        axum::http::header::AUTHORIZATION,
        "AWS4-HMAC-SHA256 Credential=a/b".parse().unwrap(),
    );
    assert!(s3s_decoded_aws_chunked(&h, true));
    assert!(
        !s3s_decoded_aws_chunked(&h, false),
        "no verified credentials: s3s did not decode"
    );
    h.insert(
        axum::http::header::AUTHORIZATION,
        "AWS a:b".parse().unwrap(),
    );
    assert!(!s3s_decoded_aws_chunked(&h, true), "SigV2 is not decoded");
}

#[test]
fn anonymous_response_overrides_are_refused() {
    let input = s3s::dto::GetObjectInput {
        bucket: "b".into(),
        key: "k".into(),
        response_content_type: Some("text/html".into()),
        ..Default::default()
    };
    assert!(check_response_overrides_allowed(&input, true).is_err());
    assert!(check_response_overrides_allowed(&input, false).is_ok());
    let plain = s3s::dto::GetObjectInput {
        bucket: "b".into(),
        key: "k".into(),
        ..Default::default()
    };
    assert!(check_response_overrides_allowed(&plain, true).is_ok());
}

fn assert_s3_service<T: s3s::S3>() {}

#[test]
fn adapter_type_implements_s3_trait() {
    assert_s3_service::<DeltaGliderS3Service>();
}

#[test]
fn anonymous_metadata_drops_the_tool_stamp() {
    use crate::types::meta_keys as mk;
    let stamped = std::collections::HashMap::from([
        (mk::TOOL.to_string(), "deltaglider_proxy/9.9.9".to_string()),
        (
            mk::H_TOOL.to_string(),
            "deltaglider_proxy/9.9.9".to_string(),
        ),
        (mk::FILE_SIZE.to_string(), "1".to_string()),
    ]);

    let mut for_anon = stamped.clone();
    strip_fingerprint_metadata(&mut for_anon, Reader::Anonymous);
    assert!(!for_anon.contains_key(mk::TOOL) && !for_anon.contains_key(mk::H_TOOL));
    assert_eq!(for_anon.get(mk::FILE_SIZE).map(String::as_str), Some("1"));

    let mut for_user = stamped;
    strip_fingerprint_metadata(&mut for_user, Reader::Authenticated);
    assert_eq!(
        for_user.get(mk::TOOL).map(String::as_str),
        Some("deltaglider_proxy/9.9.9")
    );
}

#[test]
fn reader_identity_from_request_extensions() {
    let anon = AuthenticatedUser {
        name: crate::iam::types::ANONYMOUS_USER_NAME.into(),
        access_key_id: String::new(),
        permissions: vec![],
        iam_policies: vec![],
    };
    let alice = AuthenticatedUser {
        name: "alice".into(),
        ..anon.clone()
    };

    let mut ext = axum::http::Extensions::new();
    assert_eq!(
        Reader::of(&ext),
        Reader::Authenticated,
        "open-access mode has no principal and keeps provenance"
    );
    ext.insert(anon);
    assert_eq!(Reader::of(&ext), Reader::Anonymous);

    let mut ext = axum::http::Extensions::new();
    ext.insert(alice.clone());
    assert_eq!(Reader::of(&ext), Reader::Authenticated);
    ext.insert(crate::api::auth::PresignedRequest);
    assert_eq!(
        Reader::of(&ext),
        Reader::Anonymous,
        "a presigned link holder is anonymous even though the signer is not"
    );
}

#[test]
fn head_output_preserves_visible_metadata() {
    let mut meta = FileMetadata::new_passthrough(
        "file.bin".to_string(),
        "sha".to_string(),
        "0123456789abcdef0123456789abcdef".to_string(),
        42,
        Some("application/octet-stream".to_string()),
    );
    meta.user_metadata
        .insert("owner".to_string(), "alice".to_string());
    meta.multipart_etag = Some("\"abc-2\"".to_string());

    let out = head_object_output_from_metadata(&meta, Reader::Authenticated).expect("head output");
    assert_eq!(out.content_length, Some(42));
    assert_eq!(
        out.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(out.e_tag.as_ref().map(s3s::dto::ETag::value), Some("abc-2"));
    let metadata = out.metadata.expect("metadata map");
    assert_eq!(metadata.get("owner").map(String::as_str), Some("alice"));
    assert_eq!(
        metadata.get("dg-multipart-etag").map(String::as_str),
        Some("\"abc-2\"")
    );
}

#[test]
fn head_output_blank_content_type_falls_back_to_octet_stream() {
    // A backend returning `content-type:` present-but-empty arrives as
    // Some(""); it must not be emitted verbatim (empty type + nosniff makes
    // browsers render raw bytes). Blank and whitespace-only both fall back.
    for blank in ["", "   "] {
        let meta = FileMetadata::new_passthrough(
            "file.zip".to_string(),
            "sha".to_string(),
            "0123456789abcdef0123456789abcdef".to_string(),
            7,
            Some(blank.to_string()),
        );
        let out =
            head_object_output_from_metadata(&meta, Reader::Authenticated).expect("head output");
        assert_eq!(
            out.content_type.as_deref(),
            Some("application/octet-stream")
        );
    }
}

#[test]
fn get_output_reuses_head_metadata_and_sets_body() {
    let meta = FileMetadata::new_passthrough(
        "file.bin".to_string(),
        "sha".to_string(),
        "0123456789abcdef0123456789abcdef".to_string(),
        3,
        Some("application/octet-stream".to_string()),
    );

    let out = get_object_output_from_metadata(
        &meta,
        s3s::dto::StreamingBlob::from(s3s::Body::from(bytes::Bytes::from_static(b"abc"))),
        Reader::Authenticated,
    )
    .unwrap();
    assert!(out.body.is_some());
    assert_eq!(out.content_length, Some(3));
    assert_eq!(
        out.e_tag.as_ref().map(s3s::dto::ETag::value),
        Some("0123456789abcdef0123456789abcdef")
    );
}

#[test]
fn list_output_maps_objects_and_common_prefixes() {
    let meta = FileMetadata::new_passthrough(
        "a.txt".to_string(),
        "sha".to_string(),
        "0123456789abcdef0123456789abcdef".to_string(),
        10,
        None,
    );
    let input = s3s::dto::ListObjectsV2Input {
        bucket: "bucket".to_string(),
        prefix: Some("p/".to_string()),
        delimiter: Some("/".to_string()),
        max_keys: Some(100),
        ..Default::default()
    };
    let page = crate::deltaglider::ListObjectsPage {
        objects: vec![("p/a.txt".to_string(), meta)],
        common_prefixes: vec!["p/sub/".to_string()],
        is_truncated: true,
        next_continuation_token: Some("p/a.txt".to_string()),
        facts_missing_keys: Vec::new(),
    };

    let out = list_objects_v2_output_from_page(&input, 100, page).unwrap();
    assert_eq!(out.name.as_deref(), Some("bucket"));
    assert_eq!(out.key_count, Some(2));
    assert_eq!(out.is_truncated, Some(true));
    assert_eq!(
        out.next_continuation_token.as_deref(),
        Some(encode_v2_token("p/a.txt").as_str())
    );
    assert_eq!(out.contents.as_ref().map(Vec::len), Some(1));
    assert_eq!(out.common_prefixes.as_ref().map(Vec::len), Some(1));
}

#[test]
fn list_buckets_output_filters_sorts_and_paginates() {
    let ts = chrono::Utc::now();
    let out = list_buckets_output_from_rows(
        vec![
            ("beta".to_string(), ts),
            ("alpha".to_string(), ts),
            ("archive".to_string(), ts),
        ],
        Some("a"),
        Some(1),
        None,
    );
    let buckets = out.buckets.expect("buckets");
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0].name.as_deref(), Some("alpha"));
    assert_eq!(out.continuation_token.as_deref(), Some("alpha"));
    assert_eq!(out.prefix.as_deref(), Some("a"));
}

/// X-ray H25: the continuation token from a prior page must be honored, or
/// pagination re-serves page 1 forever and later buckets are unreachable.
#[test]
fn list_buckets_output_resumes_past_continuation_token() {
    let ts = chrono::Utc::now();
    let rows = || {
        vec![
            ("a".to_string(), ts),
            ("b".to_string(), ts),
            ("c".to_string(), ts),
        ]
    };
    // Page 1: cap 1, no token → "a", truncated, token="a".
    let p1 = list_buckets_output_from_rows(rows(), None, Some(1), None);
    assert_eq!(p1.buckets.as_ref().unwrap()[0].name.as_deref(), Some("a"));
    assert_eq!(p1.continuation_token.as_deref(), Some("a"));
    // Page 2: same rows, token="a" → must SKIP "a" and return "b".
    let p2 = list_buckets_output_from_rows(rows(), None, Some(1), Some("a"));
    assert_eq!(
        p2.buckets.as_ref().unwrap()[0].name.as_deref(),
        Some("b"),
        "page 2 must resume past the token, not re-serve page 1"
    );
    assert_eq!(p2.continuation_token.as_deref(), Some("b"));
    // Page 3: token="b" → "c", not truncated, no token.
    let p3 = list_buckets_output_from_rows(rows(), None, Some(1), Some("b"));
    assert_eq!(p3.buckets.as_ref().unwrap()[0].name.as_deref(), Some("c"));
    assert_eq!(p3.continuation_token, None);
}

#[test]
fn delete_objects_count_limit_matches_s3_cap() {
    assert!(validate_delete_objects_count(1000).is_ok());
    assert_eq!(
        validate_delete_objects_count(1001).unwrap_err().code(),
        &s3s::S3ErrorCode::InvalidArgument
    );
}

#[test]
fn engine_error_maps_to_s3s_code() {
    use crate::api::S3Error;
    // The mappings the wire tests used to prove end-to-end (missing
    // key/bucket → the right S3 error code the client sees).
    assert_eq!(
        s3s::S3Error::from(S3Error::NoSuchKey("k".into())).code(),
        &s3s::S3ErrorCode::NoSuchKey
    );
    assert_eq!(
        s3s::S3Error::from(crate::deltaglider::EngineError::NotFound("k".into())).code(),
        &s3s::S3ErrorCode::NoSuchKey
    );
    // A backend that did not answer is a 503 the client retries, and the
    // message names it.
    let e = s3s::S3Error::from(crate::storage::StorageError::Unavailable(
        "backend 'hetzner-fsn1': head_object timed out".into(),
    ));
    assert_eq!(e.code(), &s3s::S3ErrorCode::ServiceUnavailable);
    assert!(e.message().unwrap_or("").contains("hetzner-fsn1"));
    // A 500 carries only its code, never the cause.
    let e = s3s::S3Error::from(S3Error::InternalError("/secret/path".into()));
    assert_eq!(e.code(), &s3s::S3ErrorCode::InternalError);
    assert_eq!(e.message(), Some("InternalError"));
}

#[tokio::test]
async fn collect_blob_limited_rejects_oversize_body() {
    let blob = s3s::dto::StreamingBlob::from(s3s::Body::from(bytes::Bytes::from_static(b"abcd")));
    let err = collect_blob_limited(Some(blob), 3, None).await.unwrap_err();
    assert_eq!(err.code(), &s3s::S3ErrorCode::EntityTooLarge);
}

#[test]
fn content_md5_validation_detects_mismatch() {
    use base64::Engine as _;
    use md5::Digest as _;
    let good = base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(b"abc"));
    assert!(validate_content_md5_s3s(Some(&good), b"abc").is_ok());
    assert_eq!(
        validate_content_md5_s3s(Some(&good), b"xyz")
            .unwrap_err()
            .code(),
        &s3s::S3ErrorCode::BadDigest
    );
}

/// An object stored at 12:00:05.700, and an HTTP date `delta` whole
/// seconds from its `Last-Modified` (12:00:05).
fn meta_at_subsecond() -> FileMetadata {
    let mut meta = FileMetadata::new_passthrough(
        "k".to_string(),
        "sha".to_string(),
        "0123456789abcdef0123456789abcdef".to_string(),
        3,
        None,
    );
    meta.created_at = chrono::DateTime::from_timestamp(1_700_000_005, 700_000_000).unwrap();
    meta
}

fn http_date(delta: i64) -> s3s::dto::Timestamp {
    SystemTime::from(chrono::DateTime::from_timestamp(1_700_000_005 + delta, 0).unwrap()).into()
}

#[test]
fn date_conditionals_compare_at_http_date_precision() {
    let meta = meta_at_subsecond();
    let code = |r: s3s::S3Result<()>| r.err().map(|e| e.code().as_str().to_string());
    // (delta, If-Modified-Since result, If-Unmodified-Since result)
    let read_cases: &[(i64, Option<&str>, Option<&str>)] = &[
        (-1, None, Some("PreconditionFailed")),
        (0, Some("NotModified"), None),
        (1, Some("NotModified"), None),
    ];
    for &(delta, ims, ius) in read_cases {
        let d = http_date(delta);
        let got = code(evaluate_read_conditionals_s3s(
            &meta,
            None,
            None,
            Some(&d),
            None,
        ));
        assert_eq!(got.as_deref(), ims, "read If-Modified-Since, delta {delta}");
        let got = code(evaluate_read_conditionals_s3s(
            &meta,
            None,
            None,
            None,
            Some(&d),
        ));
        assert_eq!(
            got.as_deref(),
            ius,
            "read If-Unmodified-Since, delta {delta}"
        );
    }
    let copy_cases: &[(i64, Option<&str>, Option<&str>)] = &[
        (-1, None, Some("PreconditionFailed")),
        (0, Some("PreconditionFailed"), None),
        (1, Some("PreconditionFailed"), None),
    ];
    for &(delta, ims, ius) in copy_cases {
        let d = http_date(delta);
        let got = code(evaluate_copy_source_conditionals_s3s(
            &meta,
            None,
            None,
            Some(&d),
            None,
        ));
        assert_eq!(got.as_deref(), ims, "copy if-modified-since, delta {delta}");
        let got = code(evaluate_copy_source_conditionals_s3s(
            &meta,
            None,
            None,
            None,
            Some(&d),
        ));
        assert_eq!(
            got.as_deref(),
            ius,
            "copy if-unmodified-since, delta {delta}"
        );
    }
}

#[test]
fn served_range_truth_table() {
    let object = |size: u64| {
        let mut meta = meta_at_subsecond();
        meta.file_size = size;
        meta
    };
    let range = |h: &str| s3s::dto::Range::parse(h).unwrap();
    let hv = |v: &str| axum::http::HeaderValue::from_str(v).unwrap();
    let four = object(4);
    let etag = four.etag();
    let lm = "Tue, 14 Nov 2023 22:13:25 GMT"; // 1_700_000_005, whole seconds
    assert_eq!(http_last_modified(&four), http_date(0));
    type Case<'a> = (
        u64,
        Option<&'a str>,
        Option<String>,
        Result<Option<(u64, u64)>, &'a str>,
    );
    let cases: &[Case] = &[
        (4, None, None, Ok(None)),
        (4, Some("bytes=1-"), None, Ok(Some((1, 4)))),
        (4, Some("bytes=-2"), None, Ok(Some((2, 4)))),
        (4, Some("bytes=9-"), None, Err("InvalidRange")),
        // Empty object: no byte can be served.
        (0, Some("bytes=-5"), None, Err("InvalidRange")),
        (0, Some("bytes=0-"), None, Err("InvalidRange")),
        // If-Range that names this version keeps the range.
        (4, Some("bytes=1-"), Some(etag.clone()), Ok(Some((1, 4)))),
        (4, Some("bytes=1-"), Some(lm.to_string()), Ok(Some((1, 4)))),
        // Any other validator serves the whole object.
        (4, Some("bytes=1-"), Some("\"deadbeef\"".into()), Ok(None)),
        (4, Some("bytes=1-"), Some(format!("W/{etag}")), Ok(None)),
        (
            4,
            Some("bytes=1-"),
            Some("Tue, 14 Nov 2023 22:13:24 GMT".into()),
            Ok(None),
        ),
        (
            4,
            Some("bytes=1-"),
            Some("Tue, 14 Nov 2023 22:13:26 GMT".into()),
            Ok(None),
        ),
        (4, Some("bytes=1-"), Some("garbage".into()), Ok(None)),
        // If-Range without Range is ignored.
        (4, None, Some("\"deadbeef\"".into()), Ok(None)),
        // A stale validator on an empty object: full (empty) body, no 416.
        (0, Some("bytes=-5"), Some("\"deadbeef\"".into()), Ok(None)),
    ];
    for (size, r, ir, want) in cases {
        let meta = object(*size);
        let r = r.map(range);
        let ir = ir.as_deref().map(hv);
        let got = served_range(r.as_ref(), ir.as_ref(), &meta)
            .map(|o| o.map(|x| (x.start, x.end)))
            .map_err(|e| e.code().as_str().to_string());
        let want = want.map_err(str::to_string);
        assert_eq!(got, want, "size {size}, range {r:?}, if-range {ir:?}");
    }
}

#[test]
fn not_modified_carries_the_validators() {
    let meta = meta_at_subsecond();
    let d = http_date(0);
    let err = evaluate_read_conditionals_s3s(&meta, None, None, Some(&d), None).unwrap_err();
    assert_eq!(err.code().as_str(), "NotModified");
    let headers = err.headers().expect("304 headers");
    assert_eq!(headers.get("etag").unwrap(), meta.etag().as_str());
    assert_eq!(
        headers.get("last-modified").unwrap(),
        "Tue, 14 Nov 2023 22:13:25 GMT"
    );
    assert!(headers.get("content-type").is_none());
}

#[test]
fn list_metadata_names_user_metadata_as_head_does() {
    let mut meta = meta_at_subsecond();
    meta.content_type = Some("text/plain".to_string());
    meta.user_metadata
        .insert("foo".to_string(), "bar".to_string());
    let list = list_metadata_map(&meta, Reader::Anonymous);
    let head = response_metadata_map(&meta, Reader::Anonymous);
    assert_eq!(list.get("x-amz-meta-foo").map(String::as_str), Some("bar"));
    assert!(!list.contains_key("x-amz-meta-user-foo"));
    assert_eq!(
        list.get("content-type").map(String::as_str),
        Some("text/plain")
    );
    assert!(!list.contains_key("x-amz-meta-dg-tool"), "anonymous strip");
    for key in head.keys() {
        assert!(list.contains_key(&format!("x-amz-meta-{key}")), "{key}");
    }
    assert_eq!(list.len(), head.len() + 1);
}

#[test]
fn client_max_keys_keeps_zero() {
    assert_eq!(client_max_keys(None), 1000);
    assert_eq!(client_max_keys(Some(0)), 0);
    assert_eq!(client_max_keys(Some(-3)), 0);
    assert_eq!(client_max_keys(Some(7)), 7);
    assert_eq!(client_max_keys(Some(5000)), 1000);
}

#[test]
fn self_copy_is_illegal_only_when_it_changes_nothing() {
    assert!(is_illegal_self_copy(("b", "k"), ("b", "k"), false));
    assert!(is_illegal_self_copy(("b", "k"), ("b", "/k"), false));
    assert!(!is_illegal_self_copy(("b", "k"), ("b", "k"), true));
    assert!(!is_illegal_self_copy(("b", "k"), ("b", "k2"), false));
    assert!(!is_illegal_self_copy(("b", "k"), ("c", "k"), false));
}

#[test]
fn parse_copy_range_truth_table() {
    use s3s::S3ErrorCode::{InvalidArgument, InvalidRange};
    // Valid inclusive ranges.
    assert_eq!(parse_copy_range("bytes=0-9", 10).unwrap(), (0, 9));
    assert_eq!(parse_copy_range("bytes=0-0", 10).unwrap(), (0, 0)); // single byte
    assert_eq!(parse_copy_range("bytes=3-7", 10).unwrap(), (3, 7));
    assert_eq!(parse_copy_range("bytes=9-9", 10).unwrap(), (9, 9)); // last byte
                                                                    // Out-of-bounds → InvalidRange.
    assert_eq!(
        parse_copy_range("bytes=5-3", 10).unwrap_err().code(),
        &InvalidRange // start > end
    );
    assert_eq!(
        parse_copy_range("bytes=0-10", 10).unwrap_err().code(),
        &InvalidRange // end == len (end >= len)
    );
    assert_eq!(
        parse_copy_range("bytes=0-0", 0).unwrap_err().code(),
        &InvalidRange // empty object: end >= len for any range
    );
    // Malformed → InvalidArgument.
    for bad in [
        "0-9",
        "bytes=",
        "bytes=5",
        "bytes=a-9",
        "bytes=0-b",
        "bytes=-",
    ] {
        assert_eq!(
            parse_copy_range(bad, 10).unwrap_err().code(),
            &InvalidArgument,
            "expected InvalidArgument for {bad:?}"
        );
    }
}

proptest::proptest! {

    #[test]
    fn parse_copy_range_accepts_iff_in_bounds(
        start in 0usize..200, end in 0usize..200, len in 0usize..200,
    ) {
        let header = format!("bytes={start}-{end}");
        let r = parse_copy_range(&header, len);
        if start <= end && end < len {
            proptest::prop_assert_eq!(r.unwrap(), (start, end));
        } else {
            // start > end OR end >= len → always InvalidRange (never Ok).
            let is_range_err =
                matches!(r.as_ref().map_err(|e| e.code()), Err(&s3s::S3ErrorCode::InvalidRange));
            proptest::prop_assert!(is_range_err, "expected InvalidRange, got {:?}", r);
        }
    }
}

#[test]
fn signed_payload_hash_validation_detects_mismatch() {
    use sha2::Digest as _;
    let good = hex::encode(sha2::Sha256::digest(b"abc"));
    assert!(verify_signed_payload_hash_s3s(
        Some(&crate::api::auth::SignedPayloadHash(good)),
        b"abc"
    )
    .is_ok());
    let bad = "0".repeat(64);
    assert_eq!(
        verify_signed_payload_hash_s3s(Some(&crate::api::auth::SignedPayloadHash(bad)), b"abc")
            .unwrap_err()
            .code(),
        &s3s::S3ErrorCode::BadDigest
    );
}

#[test]
fn completed_parts_conversion_requires_etags() {
    let upload = s3s::dto::CompletedMultipartUpload {
        parts: Some(vec![s3s::dto::CompletedPart {
            part_number: Some(1),
            e_tag: Some("\"abc\"".parse().unwrap()),
            ..Default::default()
        }]),
    };
    assert_eq!(
        completed_parts_to_request(Some(&upload)).unwrap(),
        vec![(1, "\"abc\"".to_string())]
    );
    let bad = s3s::dto::CompletedMultipartUpload {
        parts: Some(vec![s3s::dto::CompletedPart {
            part_number: Some(1),
            e_tag: None,
            ..Default::default()
        }]),
    };
    assert_eq!(
        completed_parts_to_request(Some(&bad)).unwrap_err().code(),
        &s3s::S3ErrorCode::InvalidPart
    );
}

/// X2: CopyObject read its whole source into memory (`engine.retrieve`)
/// whatever its size. Above the spool threshold the source must stream
/// into a spool file instead; at or below it stays the buffer it was.
#[tokio::test]
async fn copy_source_above_the_spool_threshold_streams_to_a_spool() {
    use crate::deltaglider::{DeltaGliderEngine, DynEngine};
    use crate::storage::StorageBackend;
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<dyn StorageBackend> = Box::new(
        crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap(),
    );
    let engine: DynEngine = DeltaGliderEngine::new_with_backend(
        Arc::new(backend),
        &crate::config::Config::default(),
        None,
    );
    engine.create_bucket("src").await.unwrap();
    let body: Vec<u8> = (0..200 * 1024u32).map(|i| (i * 31 % 251) as u8).collect();
    for key in ["a/blob.bin", "a/app.zip"] {
        engine
            .store("src", key, &body, None, Default::default())
            .await
            .unwrap();
        let size = engine.head("src", key).await.unwrap().file_size;
        match read_copy_source(&engine, "src", key, size, 64 * 1024)
            .await
            .unwrap()
        {
            CopySourceBody::Spooled { spool, size, .. } => {
                assert_eq!(size, body.len() as u64, "{key}");
                assert_eq!(std::fs::read(spool.path()).unwrap(), body, "{key}");
            }
            // A delta source is reconstructed by `retrieve_stream`, which
            // spools only above the ENGINE's threshold (the same value in
            // production; the default here): its buffer is bounded by it.
            CopySourceBody::Buffered(data, _) if key.ends_with(".zip") => {
                assert_eq!(data, body, "{key}")
            }
            CopySourceBody::Buffered(..) => panic!("{key}: a large source was buffered"),
        }
        match read_copy_source(&engine, "src", key, size, size)
            .await
            .unwrap()
        {
            CopySourceBody::Buffered(data, _) => assert_eq!(data, body, "{key}"),
            CopySourceBody::Spooled { .. } => panic!("{key}: a small source was spooled"),
        }
    }
    // A source that grew after the HEAD does not overrun its reservation.
    let grown = read_copy_source(&engine, "src", "a/blob.bin", 100 * 1024, 64 * 1024).await;
    assert_eq!(
        grown.err().map(|e| e.code().as_str().to_string()),
        Some("SlowDown".to_string())
    );
}

/// N10: a multipart completion that tries no delta (a non-eligible key, the
/// `dg-no-delta` hint, a bucket without compression) stores the parts as
/// they are; only a delta candidate within the rebuild limit is assembled.
#[tokio::test]
async fn completion_assembles_only_a_delta_candidate() {
    use crate::deltaglider::{DeltaGliderEngine, DynEngine};
    use crate::storage::StorageBackend;
    use std::collections::HashMap;
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<dyn StorageBackend> = Box::new(
        crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap(),
    );
    let mut config = crate::config::Config::default();
    config.buckets.insert(
        "plain".to_string(),
        crate::bucket_policy::BucketPolicyConfig {
            compression: Some(false),
            ..Default::default()
        },
    );
    let engine: DynEngine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, None);
    let limit = engine.tuning().mpu_delta_reconstruct_max_bytes;
    let none = HashMap::new();
    let hint = HashMap::from([("dg-no-delta".to_string(), "true".to_string())]);
    let parts =
        |bucket, key, meta, size| completion_stores_from_parts(&engine, bucket, key, meta, size);
    assert!(!parts("b", "a/app.zip", &none, limit), "a delta candidate");
    assert!(
        parts("b", "a/app.zip", &none, limit + 1),
        "too large to rebuild"
    );
    assert!(parts("b", "a/photo.jpg", &none, 1), "not delta-eligible");
    assert!(parts("b", "a/app.zip", &hint, 1), "the hint");
    assert!(parts("plain", "a/app.zip", &none, 1), "compression off");
}
