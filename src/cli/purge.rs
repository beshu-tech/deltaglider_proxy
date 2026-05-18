// SPDX-License-Identifier: GPL-3.0-only

//! `deltaglider_proxy purge <BUCKET> [--dry-run] [--json]`
//!
//! Clean up expired entries under `.deltaglider/tmp/`. These are
//! produced by the Python toolchain's rehydration cache (when a
//! non-DG-aware client needs to read a delta-compressed object, Python
//! decompresses it once and stages the result under `.deltaglider/tmp/`
//! with a `dg-expires-at` ISO timestamp). The Rust proxy reconstructs
//! deltas on every GET so it never writes here itself — `purge` is
//! shipped purely for interop with mixed Python+Rust fleets.
//!
//! We talk to S3 directly (raw `aws_sdk_s3::Client`) rather than the
//! engine because `dg-expires-at` is Python-internal user metadata
//! that the engine's `FileMetadata` doesn't surface through its
//! `user_metadata` shape (the engine's filter strips known dg-* fields
//! into typed slots, and unknown dg-* fields fall through the
//! passthrough fallback).

use crate::cli::aws_creds;
use crate::cli::config as cli_exit;
use crate::cli::ls::should_allow_local;
use crate::config::BackendConfig;
use crate::storage::S3Backend;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(clap::Args, Debug, Clone)]
pub struct PurgeArgs {
    /// Bucket name (NOT an s3:// URL — matches the Python toolchain's
    /// spelling).
    #[arg(value_name = "BUCKET")]
    pub bucket: String,

    /// Print what would be deleted without performing the deletes.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit the result as a single JSON object on stdout.
    #[arg(long)]
    pub json: bool,

    /// S3 endpoint URL.
    #[arg(long, value_name = "URL")]
    pub endpoint_url: Option<String>,

    /// AWS region.
    #[arg(long, value_name = "NAME")]
    pub region: Option<String>,

    /// AWS profile.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Override `AWS_ACCESS_KEY_ID`.
    #[arg(long, value_name = "ID")]
    pub access_key_id: Option<String>,

    /// Override `AWS_SECRET_ACCESS_KEY`.
    #[arg(long, value_name = "KEY")]
    pub secret_access_key: Option<String>,

    /// Use path-style URLs (MinIO / LocalStack).
    #[arg(long)]
    pub force_path_style: bool,
}

const PURGE_PREFIX: &str = ".deltaglider/tmp/";

#[derive(Debug, Serialize)]
pub struct PurgeResult {
    pub bucket: String,
    pub prefix: String,
    pub scanned_count: u64,
    pub expired_count: u64,
    pub deleted_count: u64,
    pub error_count: u64,
    pub total_size_freed: u64,
    pub duration_seconds: f64,
    pub dry_run: bool,
    pub errors: Vec<String>,
}

/// Pure: parse a `dg-expires-at` value (ISO8601 with optional `Z`) and
/// return whether it lies in the past compared to `now`. Returns
/// `Ok(true)` for expired, `Ok(false)` for still-fresh, and an error
/// string for unparseable values.
pub fn is_expired(expires_at: &str, now: DateTime<Utc>) -> Result<bool, String> {
    let normalized = expires_at.trim().replace('Z', "+00:00");
    let parsed = DateTime::parse_from_rfc3339(&normalized)
        .map_err(|e| format!("parse `{expires_at}`: {e}"))?
        .with_timezone(&Utc);
    Ok(parsed <= now)
}

pub async fn run(args: PurgeArgs) -> i32 {
    let started = std::time::Instant::now();

    let client = match build_client(&args).await {
        Ok(c) => c,
        Err(code) => return code,
    };

    let mut result = PurgeResult {
        bucket: args.bucket.clone(),
        prefix: PURGE_PREFIX.into(),
        scanned_count: 0,
        expired_count: 0,
        deleted_count: 0,
        error_count: 0,
        total_size_freed: 0,
        duration_seconds: 0.0,
        dry_run: args.dry_run,
        errors: Vec::new(),
    };

    let now = Utc::now();
    let mut to_delete: Vec<(String, u64)> = Vec::new();

    // List + HEAD loop. We deliberately do one HEAD per listed object;
    // batch HEAD isn't an S3 API. For a few hundred temp objects this
    // is fine; for thousands the operator likely wants to handle the
    // cleanup at the bucket-lifecycle level anyway.
    let mut continuation: Option<String> = None;
    loop {
        let mut req = client
            .list_objects_v2()
            .bucket(&args.bucket)
            .prefix(PURGE_PREFIX)
            .max_keys(1000);
        if let Some(t) = continuation.as_ref() {
            req = req.continuation_token(t);
        }
        let page = match req.send().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: list_objects_v2 failed: {e}");
                return cli_exit::EXIT_HTTP;
            }
        };

        for obj in page.contents() {
            let Some(key) = obj.key() else {
                continue;
            };
            result.scanned_count += 1;

            let head = match client
                .head_object()
                .bucket(&args.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    result.error_count += 1;
                    result
                        .errors
                        .push(format!("HEAD {key} failed: {}", flatten_err(&e)));
                    continue;
                }
            };

            let metadata = head.metadata();
            let expires_at = metadata.and_then(|m| m.get("dg-expires-at"));
            let Some(expires_at) = expires_at else {
                // No expiration metadata → leave it alone (Python's
                // semantics: only objects with `dg-expires-at` are
                // candidates).
                continue;
            };

            match is_expired(expires_at, now) {
                Ok(true) => {
                    result.expired_count += 1;
                    let size = obj.size().unwrap_or(0).max(0) as u64;
                    to_delete.push((key.to_string(), size));
                }
                Ok(false) => {}
                Err(e) => {
                    result.error_count += 1;
                    result
                        .errors
                        .push(format!("parse expires_at on {key}: {e}"));
                }
            }
        }

        if page.is_truncated().unwrap_or(false) {
            continuation = page.next_continuation_token().map(str::to_string);
            if continuation.is_none() {
                break;
            }
        } else {
            break;
        }
    }

    if to_delete.is_empty() {
        result.duration_seconds = started.elapsed().as_secs_f64();
        emit(&result, args.json);
        return cli_exit::EXIT_OK;
    }

    if args.dry_run {
        // Walk the candidate set but don't delete — we do tally sizes
        // so the operator can see total reclamation up front.
        for (key, size) in &to_delete {
            println!("(dry-run) delete: s3://{}/{}", args.bucket, key);
            result.total_size_freed = result.total_size_freed.saturating_add(*size);
        }
        result.duration_seconds = started.elapsed().as_secs_f64();
        emit(&result, args.json);
        return cli_exit::EXIT_OK;
    }

    // Execute. delete_objects supports batches of up to 1000 — we
    // already chunked LIST at 1000, so we get at most one batch per
    // LIST page.
    let chunk_size = 1000;
    for chunk in to_delete.chunks(chunk_size) {
        let identifiers: Vec<ObjectIdentifier> = chunk
            .iter()
            .filter_map(|(k, _)| ObjectIdentifier::builder().key(k).build().ok())
            .collect();
        let delete = match Delete::builder().set_objects(Some(identifiers)).build() {
            Ok(d) => d,
            Err(e) => {
                result.error_count += chunk.len() as u64;
                result.errors.push(format!("build Delete: {e}"));
                continue;
            }
        };
        match client
            .delete_objects()
            .bucket(&args.bucket)
            .delete(delete)
            .send()
            .await
        {
            Ok(resp) => {
                for (k, size) in chunk {
                    let still_failed = resp.errors().iter().any(|e| e.key() == Some(k));
                    if still_failed {
                        result.error_count += 1;
                    } else {
                        result.deleted_count += 1;
                        result.total_size_freed = result.total_size_freed.saturating_add(*size);
                    }
                }
                for err in resp.errors() {
                    result.errors.push(format!(
                        "delete {}: {} ({})",
                        err.key().unwrap_or("?"),
                        err.message().unwrap_or(""),
                        err.code().unwrap_or("?")
                    ));
                }
            }
            Err(e) => {
                result.error_count += chunk.len() as u64;
                result
                    .errors
                    .push(format!("delete_objects: {}", flatten_err(&e)));
            }
        }
    }

    result.duration_seconds = started.elapsed().as_secs_f64();
    emit(&result, args.json);
    if result.error_count > 0 && result.deleted_count > 0 {
        cli_exit::EXIT_PARTIAL
    } else if result.error_count > 0 {
        cli_exit::EXIT_HTTP
    } else {
        cli_exit::EXIT_OK
    }
}

fn emit(result: &PurgeResult, json: bool) {
    if json {
        match serde_json::to_string(result) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: serialise purge result: {e}"),
        }
        return;
    }
    println!("Bucket:          {}", result.bucket);
    println!("Prefix:          {}", result.prefix);
    println!("Scanned:         {}", result.scanned_count);
    println!("Expired:         {}", result.expired_count);
    println!("Deleted:         {}", result.deleted_count);
    println!("Bytes freed:     {}", result.total_size_freed);
    println!("Errors:          {}", result.error_count);
    println!("Duration:        {:.3}s", result.duration_seconds);
    if result.dry_run {
        println!("(dry run — no deletes performed)");
    }
    if !result.errors.is_empty() {
        eprintln!("\nFirst {} errors:", result.errors.len().min(10));
        for line in result.errors.iter().take(10) {
            eprintln!("  - {line}");
        }
    }
}

async fn build_client(args: &PurgeArgs) -> Result<aws_sdk_s3::Client, i32> {
    let creds = aws_creds::resolve(aws_creds::CredsInputs {
        access_key_flag: args.access_key_id.as_deref(),
        secret_key_flag: args.secret_access_key.as_deref(),
        region_flag: args.region.as_deref(),
        profile_flag: args.profile.as_deref(),
        ..Default::default()
    })
    .map_err(|e| {
        eprintln!("error: {e}");
        cli_exit::EXIT_AUTH
    })?;

    // `allow_local` flows through the typed `BackendConfig::S3` field
    // instead of via `DGP_BACKEND_ALLOW_LOCAL` env mutation.
    let allow_local = should_allow_local(args.endpoint_url.as_deref());

    let backend = BackendConfig::S3 {
        endpoint: args.endpoint_url.clone(),
        region: creds.region.unwrap_or_else(|| "us-east-1".into()),
        force_path_style: args.force_path_style,
        access_key_id: Some(creds.access_key_id),
        secret_access_key: Some(creds.secret_access_key),
        allow_local,
    };
    S3Backend::build_client(&backend).await.map_err(|e| {
        eprintln!("error: failed to initialise S3 client: {e}");
        cli_exit::EXIT_HTTP
    })
}

fn flatten_err<E: std::error::Error>(e: &E) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        s.push_str(": ");
        s.push_str(&inner.to_string());
        src = inner.source();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    #[test]
    fn rejects_unparseable_timestamps() {
        let err = is_expired("not-a-date", ts(2026, 1, 1)).unwrap_err();
        assert!(err.contains("parse"));
    }

    #[test]
    fn iso_z_form_is_accepted() {
        assert!(is_expired("2020-01-01T00:00:00Z", ts(2026, 1, 1)).unwrap());
    }

    #[test]
    fn explicit_offset_form_is_accepted() {
        assert!(is_expired("2020-01-01T00:00:00+00:00", ts(2026, 1, 1)).unwrap());
    }

    #[test]
    fn future_timestamps_are_not_expired() {
        assert!(!is_expired("2099-12-31T23:59:59Z", ts(2026, 1, 1)).unwrap());
    }

    #[test]
    fn timestamp_equal_to_now_is_treated_as_expired() {
        // Python's `if self.clock.now() >= expires_at:` includes equality.
        let same = "2026-01-01T00:00:00Z";
        assert!(is_expired(same, ts(2026, 1, 1)).unwrap());
    }

    #[test]
    fn purge_prefix_is_stable() {
        assert_eq!(PURGE_PREFIX, ".deltaglider/tmp/");
    }
}
