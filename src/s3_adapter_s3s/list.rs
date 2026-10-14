// SPDX-License-Identifier: BUSL-1.1

//! Object listing verbs: ListObjects (V1) and ListObjectsV2, their tokens,
//! key encoding and the `?metadata=true` extension.

use super::*;

#[derive(Debug, Clone)]
pub struct ListMetadataXmlExtensions(
    pub std::collections::HashMap<String, std::collections::HashMap<String, String>>,
);

pub(super) fn escape_list_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

impl ListMetadataXmlExtensions {
    /// Pure: `xml` (a LIST response) with each object's `<UserMetadata>`
    /// inserted before its `</Contents>`. ONE pass over the body: a search
    /// of the whole body per key was quadratic on a 1000-key page.
    pub fn insert_into(&self, xml: &str) -> String {
        let mut by_key: std::collections::HashMap<
            String,
            &std::collections::HashMap<String, String>,
        > = self
            .0
            .iter()
            .filter(|(_, m)| !m.is_empty())
            .map(|(k, m)| (escape_list_xml(k), m))
            .collect();
        let mut out = String::with_capacity(xml.len() + by_key.len() * 64);
        let mut rest = xml;
        while let Some(end) = rest.find("</Contents>") {
            let block = &rest[..end];
            out.push_str(block);
            let key = block
                .rfind("<Contents>")
                .map(|start| &block[start..])
                .and_then(|c| c.split_once("<Key>"))
                .and_then(|(_, after)| after.split_once("</Key>"))
                .map(|(k, _)| k);
            if let Some(metadata) = key.and_then(|k| by_key.remove(k)) {
                out.push_str("<UserMetadata>");
                let mut keys: Vec<_> = metadata.keys().collect();
                keys.sort();
                for k in keys {
                    out.push_str(&format!(
                        "<Items><Key>{}</Key><Value>{}</Value></Items>",
                        escape_list_xml(k),
                        escape_list_xml(&metadata[k])
                    ));
                }
                out.push_str("</UserMetadata>");
            }
            out.push_str("</Contents>");
            rest = &rest[end + "</Contents>".len()..];
        }
        out.push_str(rest);
        out
    }
}

/// ListObjects (V1) — exists primarily for legacy SDKs and
/// hand-rolled SigV4 clients that don't add `?list-type=2`. Real
/// SDKs default to V2, so this code path is rarely hit, but
/// without it `GET /<bucket>` returns 501 on s3s (s3s correctly
/// dispatches to ListObjects when no `list-type` query is
/// present, and there was no impl).
///
/// Implementation is a thin shim over `list_objects_v2`: V1's
/// `marker` becomes V2's `start-after`-equivalent (we pass it
/// through `continuation_token` for the engine which doesn't
/// distinguish, since deltaglider's storage layout uses opaque
/// next-token pagination). V1 output uses `marker` / `next-
/// marker` instead of `continuation-token` / `next-continuation-
/// token`; the engine's response shape matches V2 so we re-map.
pub(super) async fn list_objects(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::ListObjectsInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsOutput>> {
    let list_scope = req.extensions.get::<ListScope>().cloned();
    let input = req.input;
    let max_keys = client_max_keys(input.max_keys)?;
    let enc = ListKeyEncoding::of(input.encoding_type.as_ref());
    let binding = list_token::TokenBinding {
        bucket: &input.bucket,
        prefix: input.prefix.as_deref().unwrap_or(""),
        delimiter: input.delimiter.as_deref(),
    };
    let marker = v1_marker_cursor(input.marker.as_deref(), binding)?;
    let listed = client_list_page(
        &svc.state.engine.load(),
        &input.bucket,
        binding.prefix,
        input.delimiter.as_deref(),
        max_keys,
        list_cursor(marker.as_deref(), None),
        false,
        list_scope.as_ref(),
        svc.list_budget().await,
    )
    .await?;
    // V1 clients send back NextMarker (or, without one, their last <Key>):
    // a visible entry goes in clear, a scan position only sealed.
    let next_marker = client_token(&listed, binding, |key| key.to_string());
    let page = listed.page;
    let is_truncated = next_marker.is_some();
    let contents: Vec<s3s::dto::Object> = page
        .objects
        .iter()
        .map(|(key, meta)| s3s::dto::Object {
            key: Some(enc.apply(key.clone())),
            size: Some(meta.file_size as i64),
            e_tag: parse_s3s_etag(&meta.etag()).ok(),
            last_modified: Some(SystemTime::from(meta.created_at).into()),
            storage_class: Some(s3s::dto::ObjectStorageClass::from_static(
                s3s::dto::ObjectStorageClass::STANDARD,
            )),
            ..Default::default()
        })
        .collect();
    let common_prefixes: Vec<s3s::dto::CommonPrefix> = page
        .common_prefixes
        .iter()
        .map(|p| s3s::dto::CommonPrefix {
            prefix: Some(enc.apply(p.clone())),
        })
        .collect();
    let mut resp = s3s::S3Response::new(s3s::dto::ListObjectsOutput {
        name: Some(input.bucket.clone()),
        prefix: enc.apply_opt(input.prefix.clone()),
        delimiter: enc.apply_opt(input.delimiter.clone()),
        marker: enc.apply_opt(input.marker.clone()),
        next_marker: enc.apply_opt(next_marker),
        max_keys: Some(max_keys as i32),
        is_truncated: Some(is_truncated),
        contents: Some(contents),
        common_prefixes: Some(common_prefixes),
        encoding_type: input.encoding_type,
        ..Default::default()
    });
    add_listing_debug_headers(svc, &mut resp.headers, page.facts_missing_keys.len());
    Ok(resp)
}

pub(super) async fn list_objects_v2(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
    let list_scope = req.extensions.get::<ListScope>().cloned();
    let reader = Reader::of(&req.extensions);
    let include_metadata = query_flag(&req.uri, "metadata", "true");
    let input = req.input;
    let max_keys = client_max_keys(input.max_keys)?;
    let binding = list_token::TokenBinding {
        bucket: &input.bucket,
        prefix: input.prefix.as_deref().unwrap_or(""),
        delimiter: input.delimiter.as_deref(),
    };
    let token_cursor = v2_cursor(input.continuation_token.as_deref(), binding)?;
    let listed = client_list_page(
        &svc.state.engine.load(),
        &input.bucket,
        binding.prefix,
        input.delimiter.as_deref(),
        max_keys,
        list_cursor(token_cursor.as_deref(), input.start_after.as_deref()),
        include_metadata,
        list_scope.as_ref(),
        svc.list_budget().await,
    )
    .await?;
    let next_token = client_token(&listed, binding, encode_v2_token);
    let page = listed.page;
    let enc = ListKeyEncoding::of(input.encoding_type.as_ref());
    let metadata_ext = include_metadata.then(|| {
        ListMetadataXmlExtensions(
            page.objects
                .iter()
                .map(|(key, meta)| {
                    // Matched against the rendered <Key>, so encode alike.
                    (enc.apply(key.clone()), list_metadata_map(meta, reader))
                })
                .collect(),
        )
    });
    let facts_misses = page.facts_missing_keys.len();
    let mut resp = s3s::S3Response::new(list_objects_v2_output_from_page(
        &input, max_keys, page, next_token,
    )?);
    add_listing_debug_headers(svc, &mut resp.headers, facts_misses);
    if let Some(metadata_ext) = metadata_ext {
        resp.extensions.insert(metadata_ext);
    }
    Ok(resp)
}

/// Marks an opaque ListObjectsV2 continuation token (`.` is not in base64url).
pub(super) const V2_TOKEN_PREFIX: &str = "dg1.";

/// Pure: the opaque V2 continuation token for the engine cursor `key`. A raw
/// key in <NextContinuationToken> broke the XML for a key with a control
/// character (encoding-type=url encodes <Key> but not the token).
pub(super) fn encode_v2_token(key: &str) -> String {
    use base64::Engine as _;
    format!(
        "{V2_TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key)
    )
}

/// The engine cursor of a V2 continuation token, and whether it came from
/// the raw-key fallback.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct V2Cursor {
    pub key: String,
    pub legacy: bool,
}

/// Pure: the engine cursor for a V2 continuation token. A token without the
/// prefix (or that does not decode) is the old raw-key form, accepted for
/// one release so a listing that spans the upgrade goes on.
pub(super) fn decode_v2_token(token: Option<&str>) -> Option<V2Cursor> {
    use base64::Engine as _;
    let token = token?;
    let opaque = token
        .strip_prefix(V2_TOKEN_PREFIX)
        .and_then(|b| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(b)
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok());
    Some(match opaque {
        Some(key) => V2Cursor { key, legacy: false },
        None => V2Cursor {
            key: token.to_string(),
            legacy: true,
        },
    })
}

/// Pure: the client token for the next page of `listed`, if any. The last
/// entry of the page (the caller saw it) goes through `visible`; a scan
/// position where the filtered scan budget ran out can name a hidden key,
/// so it goes out sealed.
pub(super) fn client_token(
    listed: &crate::iam::listing::CallerPage,
    binding: list_token::TokenBinding<'_>,
    visible: impl Fn(&str) -> String,
) -> Option<String> {
    let cursor = listed.page.next_continuation_token.as_deref()?;
    Some(if listed.hidden_cursor {
        list_token::seal(cursor, binding)
    } else {
        visible(cursor)
    })
}

/// ListObjectsV2 requests whose continuation token took the raw-key
/// fallback. When it stays at zero, a later release can refuse such tokens.
pub static LEGACY_V2_TOKENS: std::sync::LazyLock<prometheus::IntCounter> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounter::new(
            "deltaglider_list_legacy_continuation_tokens_total",
            "ListObjectsV2 continuation tokens accepted in the old raw-key form",
        )
        .expect("valid metric")
    });

/// The engine cursor of a V2 continuation token. A sealed token (`dg2.`)
/// must open for this listing, else `InvalidArgument`: it never falls back
/// to another start. Otherwise [`decode_v2_token`], with a log line and a
/// count for the raw-key fallback.
pub(super) fn v2_cursor(
    token: Option<&str>,
    binding: list_token::TokenBinding<'_>,
) -> s3s::S3Result<Option<String>> {
    let Some(token) = token else {
        return Ok(None);
    };
    if token.starts_with(list_token::SEALED_TOKEN_PREFIX) {
        return list_token::open(token, binding).map(Some);
    }
    let Some(cursor) = decode_v2_token(Some(token)) else {
        return Ok(None);
    };
    if cursor.legacy {
        LEGACY_V2_TOKENS.inc();
        tracing::info!(
            bucket = binding.bucket,
            "ListObjectsV2 continuation token in the old raw-key form (not `{V2_TOKEN_PREFIX}`); \
             accepted in this release only"
        );
    }
    Ok(Some(cursor.key))
}

/// The engine cursor of a V1 `marker`. A marker is a key, except one with
/// the shape of a sealed token (the NextMarker of a page where the filtered
/// scan budget ran out): that one must open for this listing, else
/// `InvalidArgument`.
pub(super) fn v1_marker_cursor(
    marker: Option<&str>,
    binding: list_token::TokenBinding<'_>,
) -> s3s::S3Result<Option<String>> {
    match marker {
        Some(m) if list_token::looks_sealed(m) => list_token::open(m, binding).map(Some),
        other => Ok(other.map(str::to_string)),
    }
}

/// The engine cursor for a LIST. S3 rule: a continuation token wins, and
/// `start-after` applies only on the first request (no token). Both mean
/// "entries strictly after this string", which is what the engine takes.
pub(super) fn list_cursor<'a>(
    continuation_token: Option<&'a str>,
    start_after: Option<&'a str>,
) -> Option<&'a str> {
    continuation_token
        .filter(|t| !t.is_empty())
        .or(start_after.filter(|s| !s.is_empty()))
}

/// `encoding-type=url` (review C2): S3 then URL-encodes every key-shaped
/// field it returns, and aws-cli sets it by default and decodes with
/// `unquote_plus`. Echoing the flag over raw keys turns `+` into a space and
/// breaks `%`. Unreserved bytes and `/` stay literal, like S3.
pub(super) fn s3_url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Key-field encoder for one LIST response: identity unless the client asked
/// for `encoding-type=url`.
#[derive(Clone, Copy)]
pub(super) struct ListKeyEncoding {
    url: bool,
}

impl ListKeyEncoding {
    pub(super) fn of(encoding_type: Option<&s3s::dto::EncodingType>) -> Self {
        Self {
            url: encoding_type.is_some_and(|e| e.as_str().eq_ignore_ascii_case("url")),
        }
    }

    pub(super) fn apply(self, s: String) -> String {
        if self.url {
            s3_url_encode(&s)
        } else {
            s
        }
    }

    pub(super) fn apply_opt(self, s: Option<String>) -> Option<String> {
        s.map(|s| self.apply(s))
    }
}

/// Pure: the page size of a client LIST. S3 takes `max-keys` from 0 to 1000
/// (a larger value lists 1000); 0 is an empty page, which the engine (at
/// least one key) cannot give. A negative value is `InvalidArgument`, as on
/// S3: an empty page said that the listing ended.
pub(super) fn client_max_keys(max_keys: Option<i32>) -> s3s::S3Result<u32> {
    let max_keys = max_keys.unwrap_or(1000);
    if max_keys < 0 {
        return Err(s3s::s3_error!(
            InvalidArgument,
            "Argument maxKeys must be an integer between 0 and 2147483647"
        ));
    }
    Ok(max_keys.min(1000) as u32)
}

/// A client LIST page: `iam::listing::list_page_for_caller` with the S3 edges. `max-keys=0`
/// answers an empty page (it answered one key, s3surface-14), and an empty
/// answer from a missing bucket is `NoSuchBucket`, not an empty 200
/// (s3surface-5).
#[allow(clippy::too_many_arguments)]
pub(super) async fn client_list_page(
    engine: &std::sync::Arc<crate::deltaglider::DynEngine>,
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    max_keys: u32,
    cursor: Option<&str>,
    metadata: bool,
    scope: Option<&ListScope>,
    budget: usize,
) -> s3s::S3Result<crate::iam::listing::CallerPage> {
    if max_keys == 0 {
        ensure_listed_bucket(engine, bucket).await?;
        return Ok(crate::iam::listing::CallerPage {
            page: crate::deltaglider::ListObjectsPage {
                objects: Vec::new(),
                common_prefixes: Vec::new(),
                is_truncated: false,
                next_continuation_token: None,
                facts_missing_keys: Vec::new(),
            },
            hidden_cursor: false,
        });
    }
    let listed = crate::iam::listing::list_page_for_caller(
        &**engine, bucket, prefix, delimiter, max_keys, cursor, metadata, scope, budget,
    )
    .await?;
    if listed.page.objects.is_empty() && listed.page.common_prefixes.is_empty() {
        ensure_listed_bucket(engine, bucket).await?;
    }
    Ok(listed)
}

/// The bucket check of a LIST that finds nothing: the write paths' cached
/// one. The file browser lists a folder again after it deletes it, so a
/// HeadBucket on every empty list came at the end of every folder delete.
async fn ensure_listed_bucket(
    engine: &std::sync::Arc<crate::deltaglider::DynEngine>,
    bucket: &str,
) -> s3s::S3Result<()> {
    crate::api::handlers::ensure_bucket_exists_on(engine, bucket)
        .await
        .map_err(s3s::S3Error::from)
}

/// The `metadata=true` LIST map of one entry (D1): its storage facts,
/// named as HeadObject names them, `x-amz-meta-dg-note` (`delta`,
/// `passthrough` or `reference`) and, for a delta, `x-amz-meta-dg-delta-size`
/// (its stored size). Taken from the HEAD map (`response_metadata_map`), so
/// a value never differs from what HeadObject shows. The object's user
/// metadata and content type are not listed: HeadObject returns them. An
/// anonymous reader (a public prefix, a presigned link) gets no storage
/// facts, as it gets no `dg-tool` (`strip_fingerprint_metadata`).
pub(super) fn list_metadata_map(
    meta: &FileMetadata,
    reader: Reader,
) -> std::collections::HashMap<String, String> {
    use crate::types::meta_keys as mk;
    if reader == Reader::Anonymous {
        return std::collections::HashMap::new();
    }
    response_metadata_map(meta, reader)
        .into_iter()
        .filter(|(k, _)| k == mk::NOTE || k == mk::DELTA_SIZE)
        .map(|(k, v)| (format!("{}{k}", mk::AMZ_META_PREFIX), v))
        .collect()
}

pub(super) fn list_objects_v2_output_from_page(
    input: &s3s::dto::ListObjectsV2Input,
    max_keys: u32,
    page: crate::deltaglider::ListObjectsPage,
    next_token: Option<String>,
) -> s3s::S3Result<s3s::dto::ListObjectsV2Output> {
    let enc = ListKeyEncoding::of(input.encoding_type.as_ref());
    let contents: s3s::dto::ObjectList = page
        .objects
        .into_iter()
        .map(|(key, meta)| object_from_metadata(enc.apply(key), &meta))
        .collect::<s3s::S3Result<_>>()?;
    let common_prefixes: s3s::dto::CommonPrefixList = page
        .common_prefixes
        .into_iter()
        .map(|prefix| s3s::dto::CommonPrefix {
            prefix: Some(enc.apply(prefix)),
        })
        .collect();
    let key_count = contents.len().saturating_add(common_prefixes.len());
    Ok(s3s::dto::ListObjectsV2Output {
        name: Some(input.bucket.clone()),
        prefix: enc.apply_opt(input.prefix.clone()),
        delimiter: enc.apply_opt(input.delimiter.clone()),
        max_keys: Some(max_keys as i32),
        key_count: Some(i32::try_from(key_count).unwrap_or(i32::MAX)),
        continuation_token: input.continuation_token.clone(),
        is_truncated: Some(page.is_truncated),
        next_continuation_token: next_token,
        contents: Some(contents),
        common_prefixes: Some(common_prefixes),
        encoding_type: input.encoding_type.clone(),
        start_after: enc.apply_opt(input.start_after.clone()),
        ..Default::default()
    })
}

pub(super) fn object_from_metadata(
    key: String,
    meta: &FileMetadata,
) -> s3s::S3Result<s3s::dto::Object> {
    Ok(s3s::dto::Object {
        key: Some(key),
        e_tag: Some(parse_s3s_etag(&meta.etag())?),
        last_modified: Some(SystemTime::from(meta.created_at).into()),
        size: Some(i64::try_from(meta.file_size).unwrap_or(i64::MAX)),
        ..Default::default()
    })
}
