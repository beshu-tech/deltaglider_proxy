// SPDX-License-Identifier: GPL-3.0-only

//! `deltaglider_proxy cp <SRC> <DST>` — AWS-CLI-shaped copy between
//! local paths and S3 with transparent delta compression.
//!
//! Direction is derived from the URL shape of each argument:
//!
//!   `cp local.zip s3://b/k`           → upload (engine.store)
//!   `cp s3://b/k local.zip`           → download (engine.retrieve_stream)
//!   `cp s3://a/k1 s3://b/k2`          → S3-to-S3 (retrieve then store)
//!   `cp local1 local2`                → rejected (use shell `cp`)
//!
//! Recursive mode (`-r`) walks the source side and filters with the
//! include / exclude glob list.

use crate::cli::aws_creds;
use crate::cli::config as cli_exit;
use crate::cli::engine_factory::{build_cli_engine, CliEngineOpts};
use crate::cli::filter::Filter;
use crate::cli::ls::should_allow_local;
use crate::cli::s3_url::{is_s3_url, parse_s3_url, S3Loc};
use crate::deltaglider::DynEngine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Copy a file or directory between a local path and S3.
///
/// SRC and DST may each be a local path or an `s3://bucket/key` URL.
/// `local`→`local` is rejected (use the shell's `cp`). Recursive mode
/// (`-r`) walks every file beneath the source and applies the same
/// `--include` / `--exclude` glob filters that `aws s3 cp` uses.
#[derive(clap::Args, Debug, Clone)]
pub struct CpArgs {
    /// Source (local path or `s3://bucket/key`).
    #[arg(value_name = "SRC")]
    pub src: String,

    /// Destination (local path or `s3://bucket/key`).
    #[arg(value_name = "DST")]
    pub dst: String,

    /// Recurse into directories / prefixes.
    #[arg(short, long)]
    pub recursive: bool,

    /// Include glob pattern (repeatable).
    #[arg(long, value_name = "GLOB")]
    pub include: Vec<String>,

    /// Exclude glob pattern (repeatable; exclude wins over include).
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Preview without performing the copy. AWS-CLI spelling
    /// `--dryrun` (no dash).
    #[arg(long)]
    pub dryrun: bool,

    /// Store as passthrough — skip delta encoding even for files the
    /// router would otherwise compress.
    #[arg(long)]
    pub no_delta: bool,

    /// Override `Config::max_delta_ratio` (engine's "is this delta
    /// small enough" threshold) for this invocation.
    #[arg(long, value_name = "FLOAT")]
    pub max_ratio: Option<f32>,

    /// Override Content-Type metadata on upload.
    #[arg(long, value_name = "TYPE")]
    pub content_type: Option<String>,

    /// User-metadata `K=V` pair (repeatable).
    #[arg(long, value_name = "K=V")]
    pub metadata: Vec<String>,

    /// Suppress per-object progress output.
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

/// Direction the `cp` command resolves from SRC × DST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Direction {
    LocalToS3,
    S3ToLocal,
    S3ToS3,
    /// `local local` — not our job.
    Reject,
}

/// Pure: pick the direction from raw SRC + DST strings.
pub(crate) fn detect_direction(src: &str, dst: &str) -> Direction {
    match (is_s3_url(src), is_s3_url(dst)) {
        (false, true) => Direction::LocalToS3,
        (true, false) => Direction::S3ToLocal,
        (true, true) => Direction::S3ToS3,
        (false, false) => Direction::Reject,
    }
}

/// Parse `K=V[,K=V]...` metadata flags into a HashMap. Repeated
/// `--metadata foo=bar --metadata baz=qux` is what we usually see,
/// but AWS-CLI also accepts a single comma-separated value.
pub(crate) fn parse_metadata_pairs(pairs: &[String]) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for raw in pairs {
        for piece in raw.split(',') {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            let (k, v) = piece
                .split_once('=')
                .ok_or_else(|| format!("metadata flag `{piece}` is not in K=V form"))?;
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Ok(out)
}

pub async fn run(args: CpArgs) -> i32 {
    let direction = detect_direction(&args.src, &args.dst);
    if direction == Direction::Reject {
        eprintln!(
            "error: `cp local local` is not supported; use the shell's cp / mv (got `{}` → `{}`)",
            args.src, args.dst
        );
        return cli_exit::EXIT_USAGE;
    }

    let user_metadata = match parse_metadata_pairs(&args.metadata) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return cli_exit::EXIT_USAGE;
        }
    };
    let filter = match Filter::build(&args.include, &args.exclude) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: invalid include/exclude pattern: {e}");
            return cli_exit::EXIT_USAGE;
        }
    };

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
        max_delta_ratio: args.max_ratio,
        allow_local: should_allow_local(args.endpoint_url.as_deref()),
    };
    let engine = match build_cli_engine(opts).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: failed to initialise S3 client: {e}");
            return cli_exit::EXIT_HTTP;
        }
    };

    match direction {
        Direction::LocalToS3 => {
            let dst_loc = match parse_s3_url(&args.dst) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("error: bad destination URL: {e}");
                    return cli_exit::EXIT_PARSE;
                }
            };
            upload(&engine, &args, &user_metadata, &filter, &dst_loc).await
        }
        Direction::S3ToLocal => {
            let src_loc = match parse_s3_url(&args.src) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("error: bad source URL: {e}");
                    return cli_exit::EXIT_PARSE;
                }
            };
            download(&engine, &args, &filter, &src_loc).await
        }
        Direction::S3ToS3 => {
            let src_loc = match parse_s3_url(&args.src) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("error: bad source URL: {e}");
                    return cli_exit::EXIT_PARSE;
                }
            };
            let dst_loc = match parse_s3_url(&args.dst) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("error: bad destination URL: {e}");
                    return cli_exit::EXIT_PARSE;
                }
            };
            s3_to_s3(&engine, &args, &user_metadata, &filter, &src_loc, &dst_loc).await
        }
        Direction::Reject => unreachable!(), // handled above
    }
}

async fn upload(
    engine: &DynEngine,
    args: &CpArgs,
    user_meta: &HashMap<String, String>,
    filter: &Filter,
    dst: &S3Loc,
) -> i32 {
    let src_path = Path::new(&args.src);

    if !args.recursive {
        if !src_path.is_file() {
            eprintln!(
                "error: source `{}` is not a file (use `-r` for directories)",
                args.src
            );
            return cli_exit::EXIT_USAGE;
        }
        let dst_key = if dst.key.is_empty() || dst.key.ends_with('/') {
            let name = src_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            format!("{}{name}", dst.key)
        } else {
            dst.key.clone()
        };
        upload_one(engine, args, user_meta, &dst.bucket, src_path, &dst_key).await
    } else {
        if !src_path.is_dir() {
            eprintln!("error: source `{}` is not a directory", args.src);
            return cli_exit::EXIT_USAGE;
        }
        let root = src_path.to_path_buf();
        let mut succeeded: u64 = 0;
        let mut failed: u64 = 0;
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("warning: walkdir error: {e}");
                    failed += 1;
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = match entry.path().strip_prefix(&root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if !filter.matches(&rel_str) {
                continue;
            }
            let dst_key = if dst.key.is_empty() || dst.key.ends_with('/') {
                format!("{}{rel_str}", dst.key)
            } else {
                format!("{}/{rel_str}", dst.key)
            };
            match upload_one(engine, args, user_meta, &dst.bucket, entry.path(), &dst_key).await {
                cli_exit::EXIT_OK => succeeded += 1,
                _ => failed += 1,
            }
        }
        partial_or_ok(succeeded, failed)
    }
}

async fn upload_one(
    engine: &DynEngine,
    args: &CpArgs,
    user_meta: &HashMap<String, String>,
    bucket: &str,
    local: &Path,
    key: &str,
) -> i32 {
    if !args.quiet {
        println!("upload: {} to s3://{bucket}/{key}", local.display());
    }
    if args.dryrun {
        return cli_exit::EXIT_OK;
    }
    let data = match tokio::fs::read(local).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: read {} failed: {e}", local.display());
            return cli_exit::EXIT_IO;
        }
    };

    let content_type = args.content_type.clone();
    let meta = if args.no_delta {
        let mut m = user_meta.clone();
        // The engine consults the user-metadata bag for a few hint
        // keys; the documented "store as passthrough" lever for
        // ad-hoc clients is `x-amz-meta-dg-no-delta = true`. The
        // proxy server reads the same key. Keeping the surface
        // uniform avoids a CLI-only feature flag.
        m.insert("dg-no-delta".to_string(), "true".to_string());
        m
    } else {
        user_meta.clone()
    };

    match engine.store(bucket, key, &data, content_type, meta).await {
        Ok(_) => cli_exit::EXIT_OK,
        Err(e) => {
            eprintln!("error: upload {} failed: {e}", key);
            cli_exit::EXIT_HTTP
        }
    }
}

async fn download(engine: &DynEngine, args: &CpArgs, filter: &Filter, src: &S3Loc) -> i32 {
    if !args.recursive {
        if src.key.is_empty() || src.key.ends_with('/') {
            eprintln!("error: source must be an object (not a prefix); use `-r` to copy a prefix");
            return cli_exit::EXIT_USAGE;
        }
        let dst_path = resolve_local_dst(&args.dst, &src.key);
        download_one(engine, args, &src.bucket, &src.key, &dst_path).await
    } else {
        let dst_root = PathBuf::from(&args.dst);
        if dst_root.exists() && !dst_root.is_dir() {
            eprintln!(
                "error: destination `{}` exists and is not a directory",
                args.dst
            );
            return cli_exit::EXIT_USAGE;
        }
        if let Err(e) = tokio::fs::create_dir_all(&dst_root).await {
            eprintln!("error: mkdir {} failed: {e}", dst_root.display());
            return cli_exit::EXIT_IO;
        }
        let mut continuation: Option<String> = None;
        let mut succeeded: u64 = 0;
        let mut failed: u64 = 0;
        loop {
            let page = match engine
                .list_objects(
                    &src.bucket,
                    &src.key,
                    None,
                    1000,
                    continuation.as_deref(),
                    false,
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: list_objects failed: {e}");
                    return cli_exit::EXIT_HTTP;
                }
            };
            for (k, _meta) in &page.objects {
                let rel = k.strip_prefix(&src.key).unwrap_or(k);
                if !filter.matches(rel) {
                    continue;
                }
                let dst_path = dst_root.join(rel);
                if let Some(parent) = dst_path.parent() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        eprintln!("error: mkdir {} failed: {e}", parent.display());
                        failed += 1;
                        continue;
                    }
                }
                match download_one(engine, args, &src.bucket, k, &dst_path).await {
                    cli_exit::EXIT_OK => succeeded += 1,
                    _ => failed += 1,
                }
            }
            if !page.is_truncated {
                break;
            }
            continuation = page.next_continuation_token;
            if continuation.is_none() {
                break;
            }
        }
        partial_or_ok(succeeded, failed)
    }
}

async fn download_one(
    engine: &DynEngine,
    args: &CpArgs,
    bucket: &str,
    key: &str,
    dst: &Path,
) -> i32 {
    if !args.quiet {
        println!("download: s3://{bucket}/{key} to {}", dst.display());
    }
    if args.dryrun {
        return cli_exit::EXIT_OK;
    }
    let (data, _meta) = match engine.retrieve(bucket, key).await {
        Ok(t) => t,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("NoSuchKey") || msg.contains("not found") {
                eprintln!("error: object not found: s3://{bucket}/{key}");
                return cli_exit::EXIT_NOT_FOUND;
            }
            eprintln!("error: retrieve {} failed: {e}", key);
            return cli_exit::EXIT_HTTP;
        }
    };
    if let Err(e) = tokio::fs::write(dst, &data).await {
        eprintln!("error: write {} failed: {e}", dst.display());
        return cli_exit::EXIT_IO;
    }
    cli_exit::EXIT_OK
}

async fn s3_to_s3(
    engine: &DynEngine,
    args: &CpArgs,
    user_meta: &HashMap<String, String>,
    filter: &Filter,
    src: &S3Loc,
    dst: &S3Loc,
) -> i32 {
    if !args.recursive {
        if src.key.is_empty() || src.key.ends_with('/') {
            eprintln!("error: source must be an object (use `-r` for prefixes)");
            return cli_exit::EXIT_USAGE;
        }
        let dst_key = if dst.key.is_empty() || dst.key.ends_with('/') {
            let basename = src.key.rsplit('/').next().unwrap_or(src.key.as_str());
            format!("{}{basename}", dst.key)
        } else {
            dst.key.clone()
        };
        copy_one(
            engine,
            args,
            user_meta,
            &src.bucket,
            &src.key,
            &dst.bucket,
            &dst_key,
        )
        .await
    } else {
        let mut continuation: Option<String> = None;
        let mut succeeded: u64 = 0;
        let mut failed: u64 = 0;
        loop {
            let page = match engine
                .list_objects(
                    &src.bucket,
                    &src.key,
                    None,
                    1000,
                    continuation.as_deref(),
                    false,
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: list_objects failed: {e}");
                    return cli_exit::EXIT_HTTP;
                }
            };
            for (k, _meta) in &page.objects {
                let rel = k.strip_prefix(&src.key).unwrap_or(k);
                if !filter.matches(rel) {
                    continue;
                }
                let dst_key = if dst.key.is_empty() || dst.key.ends_with('/') {
                    format!("{}{rel}", dst.key)
                } else {
                    format!("{}/{rel}", dst.key)
                };
                match copy_one(
                    engine,
                    args,
                    user_meta,
                    &src.bucket,
                    k,
                    &dst.bucket,
                    &dst_key,
                )
                .await
                {
                    cli_exit::EXIT_OK => succeeded += 1,
                    _ => failed += 1,
                }
            }
            if !page.is_truncated {
                break;
            }
            continuation = page.next_continuation_token;
            if continuation.is_none() {
                break;
            }
        }
        partial_or_ok(succeeded, failed)
    }
}

async fn copy_one(
    engine: &DynEngine,
    args: &CpArgs,
    user_meta: &HashMap<String, String>,
    src_bucket: &str,
    src_key: &str,
    dst_bucket: &str,
    dst_key: &str,
) -> i32 {
    if !args.quiet {
        println!("copy: s3://{src_bucket}/{src_key} to s3://{dst_bucket}/{dst_key}");
    }
    if args.dryrun {
        return cli_exit::EXIT_OK;
    }
    let (data, metadata) = match engine.retrieve(src_bucket, src_key).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: source fetch failed: {e}");
            return cli_exit::EXIT_HTTP;
        }
    };
    let ct = args.content_type.clone().or(metadata.content_type);
    let mut meta = user_meta.clone();
    if args.no_delta {
        meta.insert("dg-no-delta".to_string(), "true".to_string());
    }
    match engine.store(dst_bucket, dst_key, &data, ct, meta).await {
        Ok(_) => cli_exit::EXIT_OK,
        Err(e) => {
            eprintln!("error: destination put failed: {e}");
            cli_exit::EXIT_HTTP
        }
    }
}

/// Pure: pick a local destination path from `dst` flag + the source key.
/// `dst` may be a file path (used verbatim) or a directory path (key's
/// basename is appended).
fn resolve_local_dst(dst: &str, src_key: &str) -> PathBuf {
    let dst_path = PathBuf::from(dst);
    if dst_path.is_dir() || dst.ends_with('/') {
        let basename = src_key.rsplit('/').next().unwrap_or(src_key);
        dst_path.join(basename)
    } else {
        dst_path
    }
}

fn partial_or_ok(succeeded: u64, failed: u64) -> i32 {
    if failed > 0 && succeeded > 0 {
        cli_exit::EXIT_PARTIAL
    } else if failed > 0 {
        cli_exit::EXIT_HTTP
    } else {
        cli_exit::EXIT_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_table() {
        assert_eq!(
            detect_direction("local.zip", "s3://b/k"),
            Direction::LocalToS3
        );
        assert_eq!(
            detect_direction("s3://b/k", "local.zip"),
            Direction::S3ToLocal
        );
        assert_eq!(
            detect_direction("s3://a/k1", "s3://b/k2"),
            Direction::S3ToS3
        );
        assert_eq!(detect_direction("foo.zip", "bar.zip"), Direction::Reject);
    }

    #[test]
    fn parse_metadata_pairs_handles_repeats_and_commas() {
        let raw = vec!["foo=bar".into(), "baz=qux,zap=zop".into()];
        let parsed = parse_metadata_pairs(&raw).unwrap();
        assert_eq!(parsed.get("foo").map(String::as_str), Some("bar"));
        assert_eq!(parsed.get("baz").map(String::as_str), Some("qux"));
        assert_eq!(parsed.get("zap").map(String::as_str), Some("zop"));
        assert_eq!(parsed.len(), 3);
    }

    #[test]
    fn parse_metadata_pairs_rejects_non_kv() {
        let raw = vec!["badformat".into()];
        let err = parse_metadata_pairs(&raw).expect_err("must reject");
        assert!(err.contains("K=V"));
    }

    #[test]
    fn resolve_local_dst_with_directory_appends_basename() {
        // Build a directory under tempdir so .is_dir() is true.
        let dir = tempfile::tempdir().unwrap();
        let dst = resolve_local_dst(dir.path().to_str().unwrap(), "releases/v1.zip");
        assert_eq!(dst.file_name().and_then(|s| s.to_str()), Some("v1.zip"));
    }

    #[test]
    fn resolve_local_dst_with_explicit_file_path_keeps_it() {
        let dst = resolve_local_dst("./output.bin", "releases/v1.zip");
        assert_eq!(dst.to_str(), Some("./output.bin"));
    }
}
