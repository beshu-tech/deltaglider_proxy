// SPDX-License-Identifier: GPL-3.0-only

//! `deltaglider_proxy sync <SRC> <DST> [flags...]`
//!
//! Bidirectional directory sync, AWS-CLI-shaped. Three directions:
//!
//! - `LocalToS3`: walk a local directory, mirror to a bucket prefix.
//! - `S3ToLocal`: list a bucket prefix, mirror to a local directory.
//! - `S3ToS3`: list both sides, copy missing/stale objects across.
//!   (Extends Python's spec — Python doesn't support this direction.)
//! - `LocalToLocal`: rejected with `EXIT_USAGE`.
//!
//! Per-entry "should this transfer?" decision uses size + mtime by
//! default (with a 1-second tolerance, matching Python). `--size-only`
//! skips the mtime check. `--exact-timestamps` removes the tolerance.
//! `--delete` removes destination entries that are missing from the
//! source. `--dryrun` prints the planned actions without executing.
//!
//! Globs apply against the relative path on the destination side
//! (matching Python's fnmatch behavior). `reference.bin` is always
//! excluded — it's an internal deltaspace artifact.

use crate::cli::aws_creds;
use crate::cli::config as cli_exit;
use crate::cli::engine_factory::{build_cli_engine, CliEngineOpts};
use crate::cli::filter::Filter;
use crate::cli::ls::should_allow_local;
use crate::cli::s3_url::{is_s3_url, parse_s3_url};
use crate::deltaglider::DynEngine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

#[derive(clap::Args, Debug, Clone)]
pub struct SyncArgs {
    /// Source path. Local directory or `s3://bucket[/prefix]`.
    #[arg(value_name = "SRC")]
    pub src: String,

    /// Destination path. Local directory or `s3://bucket[/prefix]`.
    #[arg(value_name = "DST")]
    pub dst: String,

    /// Remove destination entries that are missing from the source.
    #[arg(long)]
    pub delete: bool,

    /// Print what would happen without executing.
    #[arg(long)]
    pub dryrun: bool,

    /// Compare entries by size only — ignore modification times.
    #[arg(long)]
    pub size_only: bool,

    /// Require exact mtime equality (no 1-second tolerance).
    #[arg(long)]
    pub exact_timestamps: bool,

    /// Glob filter — include only keys matching this pattern (repeatable).
    #[arg(long, value_name = "GLOB")]
    pub include: Vec<String>,

    /// Glob filter — exclude keys matching this pattern (repeatable).
    /// Wins over `--include` when both match.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Force passthrough storage on uploads / S3→S3 destinations.
    #[arg(long)]
    pub no_delta: bool,

    /// Suppress per-entry progress output.
    #[arg(short, long)]
    pub quiet: bool,

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    LocalToS3,
    S3ToLocal,
    S3ToS3,
    Reject,
}

/// Pure: classify a `(src, dst)` pair by URL shape.
pub fn detect_direction(src: &str, dst: &str) -> Direction {
    match (is_s3_url(src), is_s3_url(dst)) {
        (false, true) => Direction::LocalToS3,
        (true, false) => Direction::S3ToLocal,
        (true, true) => Direction::S3ToS3,
        (false, false) => Direction::Reject,
    }
}

/// One entry in a sync inventory: a relative path within the namespace
/// being synced, the byte size, and a unix-ms mtime (None when the
/// source can't supply one — e.g. an internal listing without
/// last-modified).
#[derive(Debug, Clone)]
pub struct Entry {
    pub rel: String,
    pub size: u64,
    pub mtime_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct SyncDecisionOpts {
    pub size_only: bool,
    pub exact_timestamps: bool,
}

/// Pure: does this source entry need to be copied to the destination?
/// Returns true if dst is absent or the (size, mtime) comparison says
/// the source is newer / different.
pub fn should_sync(src: &Entry, dst: Option<&Entry>, opts: SyncDecisionOpts) -> bool {
    let dst = match dst {
        Some(d) => d,
        None => return true, // dst absent → copy
    };

    if opts.size_only {
        return src.size != dst.size;
    }

    // Size mismatch is always reason enough.
    if src.size != dst.size {
        return true;
    }

    // mtime comparison. If either side lacks mtime, fall back to "no
    // change needed" — same behavior as Python's `should_sync_file`
    // when mtime is unavailable.
    match (src.mtime_ms, dst.mtime_ms) {
        (Some(s), Some(d)) => {
            let tolerance: i64 = if opts.exact_timestamps { 0 } else { 1000 };
            s > d + tolerance
        }
        _ => false,
    }
}

pub async fn run(args: SyncArgs) -> i32 {
    let direction = detect_direction(&args.src, &args.dst);
    let filter = match Filter::build(&args.include, &args.exclude) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: bad include/exclude pattern: {e}");
            return cli_exit::EXIT_USAGE;
        }
    };
    let opts = SyncDecisionOpts {
        size_only: args.size_only,
        exact_timestamps: args.exact_timestamps,
    };

    match direction {
        Direction::Reject => {
            eprintln!("error: local→local sync is not supported");
            cli_exit::EXIT_USAGE
        }
        Direction::LocalToS3 => sync_local_to_s3(&args, &filter, opts).await,
        Direction::S3ToLocal => sync_s3_to_local(&args, &filter, opts).await,
        Direction::S3ToS3 => sync_s3_to_s3(&args, &filter, opts).await,
    }
}

async fn sync_local_to_s3(args: &SyncArgs, filter: &Filter, opts: SyncDecisionOpts) -> i32 {
    let src_dir = PathBuf::from(&args.src);
    if !src_dir.is_dir() {
        eprintln!("error: source `{}` is not a directory", args.src);
        return cli_exit::EXIT_USAGE;
    }
    let dst_loc = match parse_s3_url(&args.dst) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: bad S3 URL: {e}");
            return cli_exit::EXIT_PARSE;
        }
    };

    let engine = match build_engine_from_args(args).await {
        Ok(e) => e,
        Err(code) => return code,
    };

    let local_entries = collect_local_entries(&src_dir, filter);
    let s3_entries = match collect_s3_entries(&engine, &dst_loc.bucket, &dst_loc.key, filter).await
    {
        Ok(m) => m,
        Err(code) => return code,
    };

    let mut succeeded = 0u64;
    let mut failed = 0u64;
    let mut deleted = 0u64;

    for (rel, src) in &local_entries {
        if !should_sync(src, s3_entries.get(rel), opts) {
            continue;
        }
        let local_path = src_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let s3_key = join_prefix(&dst_loc.key, rel);
        if !args.quiet {
            println!(
                "upload: {} to s3://{}/{}",
                local_path.display(),
                dst_loc.bucket,
                s3_key
            );
        }
        if args.dryrun {
            succeeded += 1;
            continue;
        }
        match upload_one(
            &engine,
            &dst_loc.bucket,
            &s3_key,
            &local_path,
            args.no_delta,
        )
        .await
        {
            cli_exit::EXIT_OK => succeeded += 1,
            _ => failed += 1,
        }
    }

    if args.delete {
        for rel in s3_entries.keys() {
            if local_entries.contains_key(rel) {
                continue;
            }
            let key = join_prefix(&dst_loc.key, rel);
            if !args.quiet {
                println!("delete: s3://{}/{}", dst_loc.bucket, key);
            }
            if args.dryrun {
                deleted += 1;
                continue;
            }
            match engine.delete(&dst_loc.bucket, &key).await {
                Ok(_) => deleted += 1,
                Err(e) => {
                    eprintln!("error: delete s3://{}/{} failed: {e}", dst_loc.bucket, key);
                    failed += 1;
                }
            }
        }
    }

    summarize(args, succeeded, failed, deleted)
}

async fn sync_s3_to_local(args: &SyncArgs, filter: &Filter, opts: SyncDecisionOpts) -> i32 {
    let src_loc = match parse_s3_url(&args.src) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: bad S3 URL: {e}");
            return cli_exit::EXIT_PARSE;
        }
    };
    let dst_dir = PathBuf::from(&args.dst);
    if !args.dryrun {
        if let Err(e) = std::fs::create_dir_all(&dst_dir) {
            eprintln!(
                "error: create destination directory `{}` failed: {e}",
                dst_dir.display()
            );
            return cli_exit::EXIT_IO;
        }
    }

    let engine = match build_engine_from_args(args).await {
        Ok(e) => e,
        Err(code) => return code,
    };

    let s3_entries = match collect_s3_entries(&engine, &src_loc.bucket, &src_loc.key, filter).await
    {
        Ok(m) => m,
        Err(code) => return code,
    };
    let local_entries = collect_local_entries(&dst_dir, filter);

    let mut succeeded = 0u64;
    let mut failed = 0u64;
    let mut deleted = 0u64;

    for (rel, s3_entry) in &s3_entries {
        if !should_sync(s3_entry, local_entries.get(rel), opts) {
            continue;
        }
        let s3_key = join_prefix(&src_loc.key, rel);
        let local_path = dst_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !args.quiet {
            println!(
                "download: s3://{}/{} to {}",
                src_loc.bucket,
                s3_key,
                local_path.display(),
            );
        }
        if args.dryrun {
            succeeded += 1;
            continue;
        }
        match download_one(&engine, &src_loc.bucket, &s3_key, &local_path).await {
            cli_exit::EXIT_OK => succeeded += 1,
            _ => failed += 1,
        }
    }

    if args.delete {
        for rel in local_entries.keys() {
            if s3_entries.contains_key(rel) {
                continue;
            }
            let local_path = dst_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            if !args.quiet {
                println!("delete: {}", local_path.display());
            }
            if args.dryrun {
                deleted += 1;
                continue;
            }
            match std::fs::remove_file(&local_path) {
                Ok(_) => deleted += 1,
                Err(e) => {
                    eprintln!("error: delete {} failed: {e}", local_path.display());
                    failed += 1;
                }
            }
        }
    }

    summarize(args, succeeded, failed, deleted)
}

async fn sync_s3_to_s3(args: &SyncArgs, filter: &Filter, opts: SyncDecisionOpts) -> i32 {
    let src_loc = match parse_s3_url(&args.src) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: bad S3 URL: {e}");
            return cli_exit::EXIT_PARSE;
        }
    };
    let dst_loc = match parse_s3_url(&args.dst) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: bad S3 URL: {e}");
            return cli_exit::EXIT_PARSE;
        }
    };

    let engine = match build_engine_from_args(args).await {
        Ok(e) => e,
        Err(code) => return code,
    };

    let src_entries = match collect_s3_entries(&engine, &src_loc.bucket, &src_loc.key, filter).await
    {
        Ok(m) => m,
        Err(code) => return code,
    };
    let dst_entries = match collect_s3_entries(&engine, &dst_loc.bucket, &dst_loc.key, filter).await
    {
        Ok(m) => m,
        Err(code) => return code,
    };

    let mut succeeded = 0u64;
    let mut failed = 0u64;
    let mut deleted = 0u64;

    for (rel, src_entry) in &src_entries {
        if !should_sync(src_entry, dst_entries.get(rel), opts) {
            continue;
        }
        let src_key = join_prefix(&src_loc.key, rel);
        let dst_key = join_prefix(&dst_loc.key, rel);
        if !args.quiet {
            println!(
                "copy: s3://{}/{} to s3://{}/{}",
                src_loc.bucket, src_key, dst_loc.bucket, dst_key
            );
        }
        if args.dryrun {
            succeeded += 1;
            continue;
        }
        match copy_one(
            &engine,
            &src_loc.bucket,
            &src_key,
            &dst_loc.bucket,
            &dst_key,
            args.no_delta,
        )
        .await
        {
            cli_exit::EXIT_OK => succeeded += 1,
            _ => failed += 1,
        }
    }

    if args.delete {
        for rel in dst_entries.keys() {
            if src_entries.contains_key(rel) {
                continue;
            }
            let key = join_prefix(&dst_loc.key, rel);
            if !args.quiet {
                println!("delete: s3://{}/{}", dst_loc.bucket, key);
            }
            if args.dryrun {
                deleted += 1;
                continue;
            }
            match engine.delete(&dst_loc.bucket, &key).await {
                Ok(_) => deleted += 1,
                Err(e) => {
                    eprintln!("error: delete s3://{}/{} failed: {e}", dst_loc.bucket, key);
                    failed += 1;
                }
            }
        }
    }

    summarize(args, succeeded, failed, deleted)
}

fn collect_local_entries(root: &Path, filter: &Filter) -> HashMap<String, Entry> {
    let mut out = HashMap::new();
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if !filter.matches(&rel) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        out.insert(
            rel,
            Entry {
                rel: entry.path().to_string_lossy().to_string(),
                size: metadata.len(),
                mtime_ms,
            },
        );
    }
    out
}

async fn collect_s3_entries(
    engine: &DynEngine,
    bucket: &str,
    prefix: &str,
    filter: &Filter,
) -> Result<HashMap<String, Entry>, i32> {
    let mut out = HashMap::new();
    let mut continuation: Option<String> = None;
    loop {
        let page = engine
            .list_objects(bucket, prefix, None, 1000, continuation.as_deref(), true)
            .await
            .map_err(|e| {
                eprintln!("error: list_objects on s3://{bucket}/{prefix} failed: {e}");
                cli_exit::EXIT_HTTP
            })?;
        for (key, meta) in page.objects {
            // Always skip internal deltaspace artifacts — they'd be
            // double-counted otherwise. `.deltaglider/` namespace,
            // `reference.bin` sentinel.
            if key.starts_with(".deltaglider/") || key.ends_with("reference.bin") {
                continue;
            }
            let rel = strip_prefix(&key, prefix);
            if rel.is_empty() {
                continue;
            }
            if !filter.matches(&rel) {
                continue;
            }
            out.insert(
                rel,
                Entry {
                    rel: key,
                    size: meta.file_size,
                    mtime_ms: Some(meta.created_at.timestamp_millis()),
                },
            );
        }
        if !page.is_truncated {
            break;
        }
        continuation = page.next_continuation_token;
        if continuation.is_none() {
            break;
        }
    }
    Ok(out)
}

async fn upload_one(
    engine: &DynEngine,
    bucket: &str,
    key: &str,
    local: &Path,
    no_delta: bool,
) -> i32 {
    let data = match tokio::fs::read(local).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: read {} failed: {e}", local.display());
            return cli_exit::EXIT_IO;
        }
    };
    let mut user_meta = HashMap::new();
    if no_delta {
        user_meta.insert("dg-no-delta".to_string(), "true".to_string());
    }
    match engine.store(bucket, key, &data, None, user_meta).await {
        Ok(_) => cli_exit::EXIT_OK,
        Err(e) => {
            eprintln!("error: upload {key} failed: {e}");
            cli_exit::EXIT_HTTP
        }
    }
}

async fn download_one(engine: &DynEngine, bucket: &str, key: &str, local: &Path) -> i32 {
    let (data, _meta) = match engine.retrieve(bucket, key).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: retrieve {key} failed: {e}");
            return cli_exit::EXIT_HTTP;
        }
    };
    if let Some(parent) = local.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("error: create parent dir {} failed: {e}", parent.display());
            return cli_exit::EXIT_IO;
        }
    }
    if let Err(e) = std::fs::write(local, &data) {
        eprintln!("error: write {} failed: {e}", local.display());
        return cli_exit::EXIT_IO;
    }
    cli_exit::EXIT_OK
}

async fn copy_one(
    engine: &DynEngine,
    src_bucket: &str,
    src_key: &str,
    dst_bucket: &str,
    dst_key: &str,
    no_delta: bool,
) -> i32 {
    let (data, metadata) = match engine.retrieve(src_bucket, src_key).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: retrieve {src_key} failed: {e}");
            return cli_exit::EXIT_HTTP;
        }
    };
    let mut user_meta = HashMap::new();
    if no_delta {
        user_meta.insert("dg-no-delta".to_string(), "true".to_string());
    }
    match engine
        .store(dst_bucket, dst_key, &data, metadata.content_type, user_meta)
        .await
    {
        Ok(_) => cli_exit::EXIT_OK,
        Err(e) => {
            eprintln!("error: store {dst_key} failed: {e}");
            cli_exit::EXIT_HTTP
        }
    }
}

/// Pure: join a prefix and a relative key, handling trailing slashes.
fn join_prefix(prefix: &str, rel: &str) -> String {
    if prefix.is_empty() {
        return rel.to_string();
    }
    if prefix.ends_with('/') {
        format!("{prefix}{rel}")
    } else {
        format!("{prefix}/{rel}")
    }
}

/// Pure: strip the prefix from a full key, returning the relative tail.
fn strip_prefix(key: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return key.to_string();
    }
    let normalized = if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    };
    key.strip_prefix(&normalized)
        .or_else(|| key.strip_prefix(prefix))
        .map(str::to_string)
        .unwrap_or_else(String::new)
}

fn summarize(args: &SyncArgs, succeeded: u64, failed: u64, deleted: u64) -> i32 {
    let touched = succeeded + deleted;
    if !args.quiet {
        if args.dryrun {
            println!("Sync (dryrun): would transfer {succeeded}, delete {deleted}");
        } else if touched == 0 && failed == 0 {
            println!("Sync complete: already up to date");
        } else {
            println!("Sync complete: {succeeded} transferred, {deleted} deleted, {failed} failed");
        }
    }
    if failed > 0 && succeeded + deleted > 0 {
        cli_exit::EXIT_PARTIAL
    } else if failed > 0 {
        cli_exit::EXIT_HTTP
    } else {
        cli_exit::EXIT_OK
    }
}

async fn build_engine_from_args(args: &SyncArgs) -> Result<DynEngine, i32> {
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
    let opts = CliEngineOpts {
        endpoint: args.endpoint_url.clone(),
        region: creds.region.unwrap_or_else(|| "us-east-1".into()),
        force_path_style: args.force_path_style,
        access_key_id: creds.access_key_id,
        secret_access_key: creds.secret_access_key,
        max_delta_ratio: None,
        allow_local: should_allow_local(args.endpoint_url.as_deref()),
    };
    build_cli_engine(opts).await.map_err(|e| {
        eprintln!("error: failed to initialise S3 client: {e}");
        cli_exit::EXIT_HTTP
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_dispatch_handles_all_four_quadrants() {
        assert_eq!(detect_direction("/local", "s3://b/p"), Direction::LocalToS3);
        assert_eq!(detect_direction("s3://b/p", "/local"), Direction::S3ToLocal);
        assert_eq!(detect_direction("s3://a", "s3://b"), Direction::S3ToS3);
        assert_eq!(detect_direction("/a", "/b"), Direction::Reject);
    }

    fn entry(rel: &str, size: u64, mtime_ms: Option<i64>) -> Entry {
        Entry {
            rel: rel.into(),
            size,
            mtime_ms,
        }
    }

    #[test]
    fn should_sync_returns_true_when_dst_absent() {
        assert!(should_sync(
            &entry("a", 10, Some(1000)),
            None,
            SyncDecisionOpts {
                size_only: false,
                exact_timestamps: false
            }
        ));
    }

    #[test]
    fn should_sync_skips_when_identical_size_and_mtime() {
        let src = entry("a", 10, Some(1000));
        let dst = entry("a", 10, Some(1000));
        assert!(!should_sync(
            &src,
            Some(&dst),
            SyncDecisionOpts {
                size_only: false,
                exact_timestamps: false
            }
        ));
    }

    #[test]
    fn should_sync_triggers_on_size_mismatch() {
        let src = entry("a", 11, Some(1000));
        let dst = entry("a", 10, Some(1000));
        assert!(should_sync(
            &src,
            Some(&dst),
            SyncDecisionOpts {
                size_only: false,
                exact_timestamps: false
            }
        ));
    }

    #[test]
    fn should_sync_triggers_on_newer_src_outside_tolerance() {
        let src = entry("a", 10, Some(2_100));
        let dst = entry("a", 10, Some(1_000));
        assert!(should_sync(
            &src,
            Some(&dst),
            SyncDecisionOpts {
                size_only: false,
                exact_timestamps: false
            }
        ));
    }

    #[test]
    fn should_sync_within_tolerance_is_skipped() {
        // Source 500ms newer — inside the 1s default tolerance.
        let src = entry("a", 10, Some(1_500));
        let dst = entry("a", 10, Some(1_000));
        assert!(!should_sync(
            &src,
            Some(&dst),
            SyncDecisionOpts {
                size_only: false,
                exact_timestamps: false
            }
        ));
    }

    #[test]
    fn exact_timestamps_makes_500ms_drift_significant() {
        let src = entry("a", 10, Some(1_500));
        let dst = entry("a", 10, Some(1_000));
        assert!(should_sync(
            &src,
            Some(&dst),
            SyncDecisionOpts {
                size_only: false,
                exact_timestamps: true
            }
        ));
    }

    #[test]
    fn size_only_ignores_mtime() {
        let src = entry("a", 10, Some(9_999));
        let dst = entry("a", 10, Some(1_000));
        assert!(!should_sync(
            &src,
            Some(&dst),
            SyncDecisionOpts {
                size_only: true,
                exact_timestamps: false
            }
        ));
    }

    #[test]
    fn join_prefix_handles_trailing_slash() {
        assert_eq!(join_prefix("p", "a/b"), "p/a/b");
        assert_eq!(join_prefix("p/", "a/b"), "p/a/b");
        assert_eq!(join_prefix("", "a/b"), "a/b");
    }

    #[test]
    fn strip_prefix_removes_normalized_slash() {
        assert_eq!(strip_prefix("p/a/b", "p"), "a/b");
        assert_eq!(strip_prefix("p/a/b", "p/"), "a/b");
        assert_eq!(strip_prefix("a/b", ""), "a/b");
        // Prefix doesn't appear at the start → empty (caller filters out).
        assert_eq!(strip_prefix("other/a", "p"), "");
    }
}
