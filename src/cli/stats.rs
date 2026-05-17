// SPDX-License-Identifier: GPL-3.0-only

//! `deltaglider_proxy stats s3://bucket [--json]`
//!
//! Bucket-scoped compression metrics. Walks every object in the bucket,
//! tallies `original_bytes` / `stored_bytes`, and rolls deltaspace
//! health verdicts via `api::admin::delta_efficiency::classify_deltaspace`.
//! Skips the per-prefix variants the Python spec lists for follow-ups
//! (`--sampled`, `--detailed`, `--refresh`, `--no-cache`).

use crate::api::admin::{classify_deltaspace, Efficiency};
use crate::cli::aws_creds;
use crate::cli::config as cli_exit;
use crate::cli::engine_factory::{build_cli_engine, CliEngineOpts};
use crate::cli::ls::should_allow_local;
use crate::cli::s3_url::{is_s3_url, parse_s3_url};
use crate::deltaglider::DynEngine;
use crate::types::{FileMetadata, StorageInfo};
use std::collections::HashMap;

/// Bucket statistics: object counts, original-vs-stored bytes,
/// savings %, and a per-deltaspace health roll-up.
#[derive(clap::Args, Debug, Clone)]
pub struct StatsArgs {
    /// S3 URL (`s3://bucket` — bucket-scoped only in MVP).
    #[arg(value_name = "S3_URL")]
    pub url: String,

    /// Emit the results as a single JSON object on stdout (no human
    /// preamble). Shape matches the admin `bucket-scan` endpoint's
    /// `ScanResult` so cross-tool downstream tooling can consume both.
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

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct DeltaspaceHealth {
    pub excellent: u64,
    pub good: u64,
    pub fair: u64,
    pub poor: u64,
    pub no_reference: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct StatsResult {
    pub bucket: String,
    pub total_objects: u64,
    pub total_original_bytes: u64,
    pub total_stored_bytes: u64,
    pub savings_percentage: f64,
    pub deltaspace_health: DeltaspaceHealth,
}

/// Pure accumulator. One pass through every `(key, metadata)`
/// the engine returns; the caller rolls deltaspace verdicts at the
/// end via `into_result`.
#[derive(Debug, Default)]
pub(crate) struct StatsAcc {
    pub total_objects: u64,
    pub total_original_bytes: u64,
    pub total_stored_bytes: u64,
    /// deltaspace prefix → (reference_size, delta_size_list)
    pub spaces: HashMap<String, (Option<u64>, Vec<u64>)>,
}

impl StatsAcc {
    pub fn record(&mut self, key: &str, meta: &FileMetadata) {
        self.total_objects += 1;
        self.total_original_bytes = self.total_original_bytes.saturating_add(meta.file_size);
        let stored = stored_size_of(meta);
        self.total_stored_bytes = self.total_stored_bytes.saturating_add(stored);

        let deltaspace = deltaspace_id_for_key(key);
        match &meta.storage_info {
            StorageInfo::Reference { .. } => {
                let entry = self.spaces.entry(deltaspace).or_default();
                entry.0 = Some(meta.file_size);
            }
            StorageInfo::Delta { delta_size, .. } => {
                let entry = self.spaces.entry(deltaspace).or_default();
                entry.1.push(*delta_size);
            }
            StorageInfo::Passthrough => {
                // Doesn't contribute to a deltaspace verdict. Still
                // counted in total_objects / bytes above.
            }
        }
    }

    pub fn into_result(self, bucket: &str) -> StatsResult {
        let mut health = DeltaspaceHealth::default();
        // `min_deltas = 1` matches the admin-API default — a single
        // delta is enough signal for the CLI's bucket-wide roll-up.
        for (ref_size, deltas) in self.spaces.values() {
            if let Some(eff) = classify_deltaspace(*ref_size, deltas, 1) {
                match eff {
                    Efficiency::Excellent => health.excellent += 1,
                    Efficiency::Good => health.good += 1,
                    Efficiency::Fair => health.fair += 1,
                    Efficiency::Poor => health.poor += 1,
                    Efficiency::NoReference => health.no_reference += 1,
                }
            }
        }
        let savings = if self.total_original_bytes == 0 {
            0.0
        } else {
            let saved = self
                .total_original_bytes
                .saturating_sub(self.total_stored_bytes) as f64;
            (saved / self.total_original_bytes as f64) * 100.0
        };
        StatsResult {
            bucket: bucket.to_string(),
            total_objects: self.total_objects,
            total_original_bytes: self.total_original_bytes,
            total_stored_bytes: self.total_stored_bytes,
            savings_percentage: savings,
            deltaspace_health: health,
        }
    }
}

/// Pure: pick the deltaspace identifier from an object key. We use
/// the parent prefix (everything up to and including the last `/`);
/// a bare key (no slash) lives in the bucket's root deltaspace.
pub(crate) fn deltaspace_id_for_key(key: &str) -> String {
    match key.rfind('/') {
        Some(i) => key[..=i].to_string(),
        None => String::new(),
    }
}

/// Pure: stored-on-disk bytes for one object's `FileMetadata`.
pub(crate) fn stored_size_of(meta: &FileMetadata) -> u64 {
    match &meta.storage_info {
        // Reference + Passthrough: stored bytes == file bytes
        StorageInfo::Reference { .. } | StorageInfo::Passthrough => meta.file_size,
        StorageInfo::Delta { delta_size, .. } => *delta_size,
    }
}

pub async fn run(args: StatsArgs) -> i32 {
    if !is_s3_url(&args.url) {
        eprintln!("error: expected an `s3://bucket` URL, got `{}`", args.url);
        return cli_exit::EXIT_USAGE;
    }
    let loc = match parse_s3_url(&args.url) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: bad S3 URL: {e}");
            return cli_exit::EXIT_PARSE;
        }
    };
    if !loc.key.is_empty() {
        eprintln!(
            "error: stats is bucket-scoped (no prefix); got s3://{}/{}",
            loc.bucket, loc.key
        );
        return cli_exit::EXIT_USAGE;
    }

    let creds = match aws_creds::resolve(aws_creds::CredsInputs {
        access_key_flag: args.access_key_id.as_deref(),
        secret_key_flag: args.secret_access_key.as_deref(),
        region_flag: args.region.as_deref(),
        profile_flag: args.profile.as_deref(),
        ..Default::default()
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return cli_exit::EXIT_AUTH;
        }
    };

    let opts = CliEngineOpts {
        endpoint: args.endpoint_url.clone(),
        region: creds.region.unwrap_or_else(|| "us-east-1".into()),
        force_path_style: args.force_path_style,
        access_key_id: creds.access_key_id,
        secret_access_key: creds.secret_access_key,
        max_delta_ratio: None,
        allow_local: should_allow_local(args.endpoint_url.as_deref()),
    };
    let engine = match build_cli_engine(opts).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: failed to initialise S3 client: {e}");
            return cli_exit::EXIT_HTTP;
        }
    };

    match scan_bucket(&engine, &loc.bucket).await {
        Ok(result) => {
            emit(&result, args.json);
            cli_exit::EXIT_OK
        }
        Err(code) => code,
    }
}

async fn scan_bucket(engine: &DynEngine, bucket: &str) -> Result<StatsResult, i32> {
    let mut acc = StatsAcc::default();
    let mut continuation: Option<String> = None;
    loop {
        let page = engine
            .list_objects(bucket, "", None, 1000, continuation.as_deref(), true)
            .await
            .map_err(|e| {
                eprintln!("error: list_objects failed: {e}");
                cli_exit::EXIT_HTTP
            })?;
        for (key, meta) in &page.objects {
            acc.record(key, meta);
        }
        if !page.is_truncated {
            break;
        }
        continuation = page.next_continuation_token;
        if continuation.is_none() {
            break;
        }
    }
    Ok(acc.into_result(bucket))
}

fn emit(result: &StatsResult, json: bool) {
    if json {
        // Stable shape — serde flattening would be tighter but we
        // want a tested JSON contract. `serde_json::to_string` cannot
        // fail for our types.
        match serde_json::to_string(result) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: serialising stats failed: {e}"),
        }
        return;
    }
    println!("Bucket:                 {}", result.bucket);
    println!("Total Objects:          {}", result.total_objects);
    println!("Total Original Bytes:   {}", result.total_original_bytes);
    println!("Total Stored Bytes:     {}", result.total_stored_bytes);
    println!("Savings:                {:.2}%", result.savings_percentage);
    let h = &result.deltaspace_health;
    println!(
        "Deltaspace Health:      excellent={} good={} fair={} poor={} no_reference={}",
        h.excellent, h.good, h.fair, h.poor, h.no_reference
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileMetadata, StorageInfo};
    use chrono::Utc;

    fn meta(file_size: u64, info: StorageInfo) -> FileMetadata {
        FileMetadata {
            tool: "deltaglider/test".into(),
            original_name: "x.bin".into(),
            file_sha256: "0".into(),
            md5: "0".into(),
            file_size,
            multipart_etag: None,
            created_at: Utc::now(),
            content_type: None,
            user_metadata: Default::default(),
            storage_info: info,
        }
    }

    #[test]
    fn deltaspace_id_is_parent_prefix() {
        assert_eq!(deltaspace_id_for_key("releases/v1.zip"), "releases/");
        assert_eq!(deltaspace_id_for_key("a/b/c.zip"), "a/b/");
        assert_eq!(deltaspace_id_for_key("bare.zip"), "");
    }

    #[test]
    fn stored_size_picks_delta_size_for_delta_variant() {
        let m = meta(
            1024,
            StorageInfo::Delta {
                ref_path: "reference.bin".into(),
                ref_sha256: "abc".into(),
                delta_size: 64,
                delta_cmd: "xdelta3 …".into(),
            },
        );
        assert_eq!(stored_size_of(&m), 64);
    }

    #[test]
    fn stored_size_for_passthrough_is_file_size() {
        assert_eq!(stored_size_of(&meta(1024, StorageInfo::Passthrough)), 1024);
    }

    #[test]
    fn stored_size_for_reference_is_file_size() {
        let m = meta(
            1024,
            StorageInfo::Reference {
                source_name: "v0.zip".into(),
            },
        );
        assert_eq!(stored_size_of(&m), 1024);
    }

    #[test]
    fn acc_classifies_excellent_deltaspace() {
        let mut acc = StatsAcc::default();
        // Reference = 200 KiB, two deltas at 1 KiB each → median ratio 0.5%.
        acc.record(
            "releases/v0.zip",
            &meta(
                200_000,
                StorageInfo::Reference {
                    source_name: "v0.zip".into(),
                },
            ),
        );
        for i in 1..=2 {
            acc.record(
                &format!("releases/v{i}.zip"),
                &meta(
                    200_000,
                    StorageInfo::Delta {
                        ref_path: "reference.bin".into(),
                        ref_sha256: "abc".into(),
                        delta_size: 1_000,
                        delta_cmd: "xdelta3 …".into(),
                    },
                ),
            );
        }
        let r = acc.into_result("test");
        assert_eq!(r.total_objects, 3);
        assert_eq!(r.deltaspace_health.excellent, 1);
        assert!(r.savings_percentage > 50.0, "got {}%", r.savings_percentage);
    }

    #[test]
    fn empty_bucket_returns_zero_savings_without_division_by_zero() {
        let acc = StatsAcc::default();
        let r = acc.into_result("empty");
        assert_eq!(r.total_objects, 0);
        assert_eq!(r.total_original_bytes, 0);
        assert_eq!(r.total_stored_bytes, 0);
        assert_eq!(r.savings_percentage, 0.0);
    }
}
