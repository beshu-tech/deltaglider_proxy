// SPDX-License-Identifier: BUSL-1.1

//! `deltaglider_proxy s3 cp <SRC> <DST>` — AWS-CLI-shaped copy between
//! local paths and S3 with transparent delta compression.
//!
//! Direction is derived from the URL shape of each argument:
//!
//!   `cp local.zip s3://b/k`           → upload
//!   `cp s3://b/k local.zip`           → download
//!   `cp s3://a/k1 s3://b/k2`          → S3-to-S3 copy
//!   `cp local1 local2`                → rejected (use shell `cp`)
//!
//! Recursive mode (`-r`) walks the source side and filters with the
//! include / exclude glob list. Every body moves through
//! `transfer_io` (bounded memory: spool files and streams).

use crate::cli::aws_args::{AwsArgs, EngineLimits};
use crate::cli::config as cli_exit;
use crate::cli::engine_factory::copy_user_metadata;
use crate::cli::filter::Filter;
use crate::cli::keys::{dir_prefix, local_path_for_key, rel_under, LocalPathError};
use crate::cli::s3_url::{is_s3_url, parse_s3_url, S3Loc};
use crate::cli::transfer_io::{self, TransferError};
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

    #[command(flatten)]
    pub aws: AwsArgs,

    /// Override the engine's size ceiling for delta-eligible objects
    /// (MiB). The default is 100 MiB, because xdelta3 memory scales with
    /// the object size. Raise it for large artifacts (release ZIPs, disk
    /// images). Other files are not bound by it. Affects this CLI
    /// invocation only.
    #[arg(long, value_name = "MIB")]
    pub max_object_size_mb: Option<u64>,
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

    let engine = match args
        .aws
        .engine(EngineLimits::from_flags(
            args.max_ratio,
            args.max_object_size_mb,
        ))
        .await
    {
        Ok(e) => e,
        Err(code) => return code,
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

    match transfer_io::upload_file(engine, bucket, key, local, content_type, meta).await {
        Ok(_) => cli_exit::EXIT_OK,
        Err(e @ TransferError::LocalRead(_)) => {
            eprintln!("error: {} {e}", local.display());
            cli_exit::EXIT_IO
        }
        Err(e) => {
            eprintln!("error: upload {key} failed: {e}");
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
        let dst_path = match resolve_local_dst(&args.dst, &src.key) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "error: refusing to download s3://{}/{}: {e}",
                    src.bucket, src.key
                );
                return cli_exit::EXIT_USAGE;
            }
        };
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
        // Directory semantics: `releases` means `releases/`.
        let src_dir = dir_prefix(&src.key);
        let mut continuation: Option<String> = None;
        let mut succeeded: u64 = 0;
        let mut failed: u64 = 0;
        loop {
            let page = match engine
                .list_objects(
                    &src.bucket,
                    &src_dir,
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
                let Some(rel) = rel_under(k, &src_dir) else {
                    continue;
                };
                if !filter.matches(rel) {
                    continue;
                }
                let dst_path = match local_path_for_key(&dst_root, rel) {
                    Ok(p) => p,
                    Err(LocalPathError::DirectoryMarker) => continue,
                    Err(e) => {
                        eprintln!("warning: skipping s3://{}/{k}: {e}", src.bucket);
                        failed += 1;
                        continue;
                    }
                };
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
    match transfer_io::download_file(engine, bucket, key, dst).await {
        Ok(_) => cli_exit::EXIT_OK,
        Err(TransferError::Source(e)) => {
            if let Some(what) = super::missing(&e) {
                eprintln!("error: {what} not found: s3://{bucket}/{key}");
                return cli_exit::EXIT_NOT_FOUND;
            }
            eprintln!("error: retrieve {key} failed: {e}");
            cli_exit::EXIT_HTTP
        }
        Err(e @ TransferError::LocalWrite(_)) => {
            eprintln!("error: {} {e}", dst.display());
            cli_exit::EXIT_IO
        }
        Err(e) => {
            eprintln!("error: download {key} failed: {e}");
            cli_exit::EXIT_HTTP
        }
    }
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
        // Directory semantics: `releases` means `releases/`.
        let src_dir = dir_prefix(&src.key);
        let mut continuation: Option<String> = None;
        let mut succeeded: u64 = 0;
        let mut failed: u64 = 0;
        loop {
            let page = match engine
                .list_objects(
                    &src.bucket,
                    &src_dir,
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
                let Some(rel) = rel_under(k, &src_dir) else {
                    continue;
                };
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
    let dest_attrs = |source: &crate::types::FileMetadata| {
        (
            args.content_type.clone().or(source.content_type.clone()),
            copy_user_metadata(&source.user_metadata, user_meta, args.no_delta),
        )
    };
    match transfer_io::copy_object(
        engine, src_bucket, src_key, engine, dst_bucket, dst_key, dest_attrs,
    )
    .await
    {
        Ok(_) => cli_exit::EXIT_OK,
        Err(e) => {
            eprintln!("error: {e}");
            cli_exit::EXIT_HTTP
        }
    }
}

/// Pick a local destination path from `dst` flag + the source key.
/// `dst` may be a file path (used verbatim) or a directory path (key's
/// basename is appended, through the same safety gate as `-r`).
fn resolve_local_dst(dst: &str, src_key: &str) -> Result<PathBuf, LocalPathError> {
    let dst_path = PathBuf::from(dst);
    if dst_path.is_dir() || dst.ends_with('/') {
        let basename = src_key.rsplit('/').next().unwrap_or(src_key);
        local_path_for_key(&dst_path, basename)
    } else {
        Ok(dst_path)
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
    use std::sync::Arc;

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
        let dst = resolve_local_dst(dir.path().to_str().unwrap(), "releases/v1.zip").unwrap();
        assert_eq!(dst.file_name().and_then(|s| s.to_str()), Some("v1.zip"));
    }

    #[test]
    fn resolve_local_dst_with_explicit_file_path_keeps_it() {
        let dst = resolve_local_dst("./output.bin", "releases/v1.zip").unwrap();
        assert_eq!(dst.to_str(), Some("./output.bin"));
    }

    // ── Exit-code helper coverage ──────────────────────────────────
    // `partial_or_ok` distinguishes three outcomes: all-clean, partial
    // failure (some succeeded, some didn't), and total failure. The
    // distinction matters operationally: a CI job that sees EXIT_PARTIAL
    // knows to retry only the failed keys, while EXIT_HTTP suggests
    // a backend-wide outage that won't benefit from a per-key retry.

    #[test]
    fn partial_or_ok_all_clean_is_exit_ok() {
        assert_eq!(partial_or_ok(5, 0), cli_exit::EXIT_OK);
    }

    #[test]
    fn partial_or_ok_some_failed_is_exit_partial() {
        assert_eq!(partial_or_ok(3, 2), cli_exit::EXIT_PARTIAL);
    }

    #[test]
    fn partial_or_ok_all_failed_is_exit_http() {
        assert_eq!(partial_or_ok(0, 5), cli_exit::EXIT_HTTP);
        // Zero-of-zero is the empty-prefix case — treated as success.
        assert_eq!(partial_or_ok(0, 0), cli_exit::EXIT_OK);
    }

    // ── parse_metadata_pairs edge cases ────────────────────────────
    // The parser must tolerate human-supplied formatting (empty values,
    // leading/trailing whitespace, redundant commas) without surprises;
    // AWS-CLI's `--metadata` syntax is forgiving and we need parity to
    // not surprise migrators from the Python tool.

    #[test]
    fn parse_metadata_pairs_accepts_empty_value() {
        // `K=` is a legitimate way to set an empty metadata value;
        // S3 itself accepts it and Python deltaglider passes it through.
        let raw = vec!["empty=".into()];
        let parsed = parse_metadata_pairs(&raw).unwrap();
        assert_eq!(parsed.get("empty").map(String::as_str), Some(""));
    }

    #[test]
    fn parse_metadata_pairs_trims_whitespace_and_skips_blanks() {
        // Comma-separated lists with stray whitespace come from shell
        // aliases / docs templates; we don't want those to produce
        // surprising empty keys.
        let raw = vec!["  foo = bar ,, baz=qux  ".into()];
        let parsed = parse_metadata_pairs(&raw).unwrap();
        assert_eq!(parsed.get("foo").map(String::as_str), Some("bar"));
        assert_eq!(parsed.get("baz").map(String::as_str), Some("qux"));
        assert_eq!(parsed.len(), 2);
    }

    // ── detect_direction adversarial inputs ────────────────────────
    // is_s3_url's contract is "starts with s3://"; we want to ensure
    // close-but-not-equal prefixes (s3:/, s3:, S3://) don't get coerced
    // into Direction::S3*, which would dispatch to the engine layer
    // and produce confusing errors.

    #[test]
    fn detect_direction_rejects_close_but_invalid_s3_prefixes() {
        // Missing the second slash — not a URL.
        assert_eq!(
            detect_direction("s3:/bucket/key", "local.zip"),
            Direction::Reject
        );
        // Uppercase scheme — the AWS CLI is case-sensitive and so are we.
        assert_eq!(
            detect_direction("S3://bucket/key", "local.zip"),
            Direction::Reject
        );
        // Path-like that just happens to contain "s3" — must not match.
        assert_eq!(
            detect_direction("./s3/file.zip", "out.zip"),
            Direction::Reject
        );
    }

    // ── resolve_local_dst on bucket-root-only sources ──────────────
    // When the source key is bucketless (download s3://b → ./), the
    // basename derivation falls back on the literal file name "/".
    // Verify we don't produce a path that walks out of the dst dir.

    #[test]
    fn resolve_local_dst_with_root_key_keeps_dst_within_dir() {
        let dir = tempfile::tempdir().unwrap();
        // src_key = "single.bin" (no slashes) → file goes directly in dir.
        let dst = resolve_local_dst(dir.path().to_str().unwrap(), "single.bin").unwrap();
        assert_eq!(
            dst.parent().map(|p| p.to_path_buf()),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(dst.file_name().and_then(|s| s.to_str()), Some("single.bin"));
    }

    #[test]
    fn resolve_local_dst_refuses_a_dot_dot_basename() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_local_dst(dir.path().to_str().unwrap(), "a/..").is_err());
    }

    /// Filesystem-backed engine with a 1 MiB delta ceiling.
    async fn fs_engine(dir: &Path) -> DynEngine {
        let backend: Box<dyn crate::storage::StorageBackend> = Box::new(
            crate::storage::FilesystemBackend::new(dir.to_path_buf())
                .await
                .unwrap(),
        );
        let config = crate::config::Config {
            max_object_size: 1024 * 1024,
            ..Default::default()
        };
        let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &config,
            None,
        );
        engine.create_bucket("b").await.unwrap();
        engine
    }

    fn args(src: &str, dst: &str) -> CpArgs {
        CpArgs {
            src: src.into(),
            dst: dst.into(),
            recursive: false,
            include: vec![],
            exclude: vec![],
            dryrun: false,
            no_delta: false,
            max_ratio: None,
            content_type: None,
            metadata: vec![],
            quiet: true,
            aws: Default::default(),
            max_object_size_mb: None,
        }
    }

    /// `cp` read the whole file into RAM and stored it with the buffered
    /// `engine.store`, which refuses any body above `max_object_size`. A
    /// passthrough file streams, so only the passthrough ceiling applies:
    /// upload, S3-to-S3 copy and download of a 12 MiB `.jpg` under a
    /// 1 MiB delta ceiling all succeed and keep the bytes.
    #[tokio::test]
    async fn cp_streams_a_passthrough_file_above_the_delta_ceiling() {
        let store = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let engine = fs_engine(store.path()).await;
        let body: Vec<u8> = (0..12 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        let src = local.path().join("photo.jpg");
        std::fs::write(&src, &body).unwrap();
        let a = args(src.to_str().unwrap(), "s3://b/in/photo.jpg");
        let meta = HashMap::new();

        let code = upload_one(&engine, &a, &meta, "b", &src, "in/photo.jpg").await;
        assert_eq!(code, cli_exit::EXIT_OK, "upload");
        let code = copy_one(
            &engine,
            &a,
            &meta,
            "b",
            "in/photo.jpg",
            "b",
            "out/photo.jpg",
        )
        .await;
        assert_eq!(code, cli_exit::EXIT_OK, "s3-to-s3 copy");
        let dst = local.path().join("back.jpg");
        let code = download_one(&engine, &a, "b", "out/photo.jpg", &dst).await;
        assert_eq!(code, cli_exit::EXIT_OK, "download");
        assert!(std::fs::read(&dst).unwrap() == body, "bytes differ");
    }

    /// Guard for the class: every download path must build local paths
    /// through `keys::local_path_for_key`, never `Path::join` on a key.
    #[test]
    fn download_paths_never_join_a_raw_key() {
        for (name, src) in [
            ("cp.rs", include_str!("cp.rs")),
            ("sync.rs", include_str!("sync.rs")),
        ] {
            let code = &crate::source_scan::prod_text(src);
            for bad in ["dst_root.join(", "dst_dir.join(", "dst_path.join("] {
                assert!(!code.contains(bad), "{name} joins a raw key: `{bad}`");
            }
        }
    }
}
