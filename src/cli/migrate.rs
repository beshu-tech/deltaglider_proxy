// SPDX-License-Identifier: GPL-3.0-only

//! `deltaglider_proxy migrate s3://src s3://dst [flags...]`
//!
//! Bulk S3→S3 copy of an entire deltaspace through the engine. Each
//! file is retrieved on the source side (engine reconstructs deltas
//! transparently) and stored on the destination side (engine re-encodes
//! against the destination's reference baseline). The transparency
//! property means the delta layout is recreated correctly even when the
//! destination has a different reference.
//!
//! `--no-preserve-prefix` matches the Python toolchain's spelling: the
//! default is to APPEND the last component of the source prefix to the
//! destination, so `migrate s3://a/path/foo s3://b/dst` writes objects
//! under `s3://b/dst/foo/` unless the flag is set.
//!
//! Resume support: destination is listed first, and any key already
//! present (modulo the `.delta` suffix) is skipped. Re-running after a
//! partial migration picks up where it left off without copying twice.

use crate::cli::aws_creds;
use crate::cli::config as cli_exit;
use crate::cli::engine_factory::{build_cli_engine, CliEngineOpts};
use crate::cli::filter::Filter;
use crate::cli::ls::should_allow_local;
use crate::cli::s3_url::{is_s3_url, parse_s3_url, S3Loc};
use crate::deltaglider::DynEngine;
use std::collections::HashSet;
use std::io::BufRead;

#[derive(clap::Args, Debug, Clone)]
pub struct MigrateArgs {
    /// Source S3 URL (`s3://bucket[/prefix]`).
    #[arg(value_name = "SRC_S3_URL")]
    pub src: String,

    /// Destination S3 URL (`s3://bucket[/prefix]`).
    #[arg(value_name = "DST_S3_URL")]
    pub dst: String,

    /// Include only files matching this glob (repeatable).
    #[arg(long, value_name = "GLOB")]
    pub include: Vec<String>,

    /// Exclude files matching this glob (repeatable). Wins over
    /// `--include` when both match the same key.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Disable prefix preservation. By default we append the last
    /// component of the source prefix to the destination path
    /// (mirroring Python's spec).
    #[arg(long)]
    pub no_preserve_prefix: bool,

    /// Show what would migrate without copying anything. Implies
    /// `--yes`.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the interactive confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Override `max_delta_ratio` on the destination engine.
    #[arg(long, value_name = "FLOAT")]
    pub max_ratio: Option<f32>,

    /// Force passthrough storage on the destination side (no delta
    /// re-encoding).
    #[arg(long)]
    pub no_delta: bool,

    /// Suppress per-file progress output.
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

/// Pure: compute the effective destination prefix given source prefix +
/// destination prefix + preserve setting. Matches Python's algorithm.
///
/// - `preserve_prefix=true, src=path/to/foo/, dst=archive/` →
///   `archive/foo/`
/// - `preserve_prefix=true, src="", dst=archive/` → `archive/`
/// - `preserve_prefix=false, src=anything, dst=archive/` → `archive/`
pub(crate) fn effective_dest_prefix(
    src_prefix: &str,
    dst_prefix: &str,
    preserve_prefix: bool,
) -> String {
    if !preserve_prefix || src_prefix.is_empty() {
        return dst_prefix.to_string();
    }
    let trimmed = src_prefix.trim_end_matches('/');
    let last_component = trimmed.rsplit('/').next().unwrap_or("");
    if last_component.is_empty() {
        return dst_prefix.to_string();
    }
    // Ensure dst_prefix ends with `/` before appending.
    let base = if dst_prefix.is_empty() || dst_prefix.ends_with('/') {
        dst_prefix.to_string()
    } else {
        format!("{dst_prefix}/")
    };
    format!("{base}{last_component}/")
}

pub async fn run(args: MigrateArgs) -> i32 {
    let src_loc = match parse_url(&args.src) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let dst_loc = match parse_url(&args.dst) {
        Ok(l) => l,
        Err(code) => return code,
    };

    let filter = match Filter::build(&args.include, &args.exclude) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: bad include/exclude pattern: {e}");
            return cli_exit::EXIT_USAGE;
        }
    };

    let engine = match build_engine_from_args(&args).await {
        Ok(e) => e,
        Err(code) => return code,
    };

    let preserve_prefix = !args.no_preserve_prefix;
    let dest_prefix_eff = effective_dest_prefix(&src_loc.key, &dst_loc.key, preserve_prefix);

    // List source.
    let src_keys = match list_prefix(&engine, &src_loc.bucket, &src_loc.key).await {
        Ok(k) => k,
        Err(code) => return code,
    };

    // List destination (for resume support — skip already-migrated keys).
    let dst_keys = match list_prefix(&engine, &dst_loc.bucket, &dest_prefix_eff).await {
        Ok(k) => k,
        Err(code) => return code,
    };
    let dst_index: HashSet<String> = dst_keys
        .into_iter()
        .map(|k| strip_prefix(&k, &dest_prefix_eff))
        .collect();

    // Compute the migration plan.
    let plan = build_plan(&src_keys, &src_loc.key, &dst_index, &filter);

    if plan.is_empty() {
        if !args.quiet {
            println!("Nothing to migrate (destination already has all source keys).");
        }
        return cli_exit::EXIT_OK;
    }

    if !args.quiet {
        println!(
            "Files to migrate: {}\nFrom: s3://{}/{}\nTo:   s3://{}/{}",
            plan.len(),
            src_loc.bucket,
            src_loc.key,
            dst_loc.bucket,
            dest_prefix_eff,
        );
        if !dst_index.is_empty() {
            println!("Already migrated: {} (will be skipped)", dst_index.len());
        }
    }

    if args.dry_run {
        if !args.quiet {
            println!("\n--- DRY RUN ---");
            for (rel, _) in plan.iter().take(10) {
                println!("  Would migrate: {rel}");
            }
            if plan.len() > 10 {
                println!("  ... and {} more", plan.len() - 10);
            }
        }
        return cli_exit::EXIT_OK;
    }

    if !args.yes && !confirm_prompt() {
        if !args.quiet {
            println!("Migration cancelled.");
        }
        return cli_exit::EXIT_OK;
    }

    // Execute.
    let mut succeeded = 0u64;
    let mut failed = 0u64;
    for (rel_key, src_full) in &plan {
        let dst_full = if dest_prefix_eff.is_empty() {
            rel_key.clone()
        } else if dest_prefix_eff.ends_with('/') {
            format!("{dest_prefix_eff}{rel_key}")
        } else {
            format!("{dest_prefix_eff}/{rel_key}")
        };
        if !args.quiet {
            println!(
                "copy: s3://{}/{} to s3://{}/{}",
                src_loc.bucket, src_full, dst_loc.bucket, dst_full
            );
        }
        match copy_one(
            &engine,
            &src_loc.bucket,
            src_full,
            &dst_loc.bucket,
            &dst_full,
            args.no_delta,
        )
        .await
        {
            cli_exit::EXIT_OK => succeeded += 1,
            _ => failed += 1,
        }
    }

    if !args.quiet {
        println!("Migration complete: {} ok, {} failed", succeeded, failed);
    }
    if failed > 0 && succeeded > 0 {
        cli_exit::EXIT_PARTIAL
    } else if failed > 0 {
        cli_exit::EXIT_HTTP
    } else {
        cli_exit::EXIT_OK
    }
}

/// Pure: relative-key + source-full-key tuples we plan to copy. Skips
/// keys whose relative form already exists at the destination.
pub(crate) fn build_plan(
    src_keys: &[String],
    src_prefix: &str,
    dst_index: &HashSet<String>,
    filter: &Filter,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for k in src_keys {
        let rel = strip_prefix(k, src_prefix);
        if !filter.matches(&rel) {
            continue;
        }
        if dst_index.contains(&rel) {
            continue;
        }
        out.push((rel, k.clone()));
    }
    out
}

fn strip_prefix(key: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return key.to_string();
    }
    key.strip_prefix(prefix)
        .map(str::to_string)
        .unwrap_or_else(|| key.to_string())
}

async fn list_prefix(engine: &DynEngine, bucket: &str, prefix: &str) -> Result<Vec<String>, i32> {
    let mut out = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let page = engine
            .list_objects(bucket, prefix, None, 1000, continuation.as_deref(), false)
            .await
            .map_err(|e| {
                eprintln!("error: list_objects on s3://{bucket}/{prefix} failed: {e}");
                cli_exit::EXIT_HTTP
            })?;
        for (key, _) in page.objects {
            out.push(key);
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
    let mut user_meta = std::collections::HashMap::new();
    if no_delta {
        user_meta.insert("dg-no-delta".to_string(), "true".to_string());
    }
    let ct = metadata.content_type;
    match engine
        .store(dst_bucket, dst_key, &data, ct, user_meta)
        .await
    {
        Ok(_) => cli_exit::EXIT_OK,
        Err(e) => {
            eprintln!("error: store {dst_key} failed: {e}");
            cli_exit::EXIT_HTTP
        }
    }
}

fn parse_url(url: &str) -> Result<S3Loc, i32> {
    if !is_s3_url(url) {
        eprintln!("error: expected an `s3://` URL, got `{url}`");
        return Err(cli_exit::EXIT_USAGE);
    }
    parse_s3_url(url).map_err(|e| {
        eprintln!("error: bad S3 URL: {e}");
        cli_exit::EXIT_PARSE
    })
}

async fn build_engine_from_args(args: &MigrateArgs) -> Result<DynEngine, i32> {
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
        max_delta_ratio: args.max_ratio,
        allow_local: should_allow_local(args.endpoint_url.as_deref()),
    };
    build_cli_engine(opts).await.map_err(|e| {
        eprintln!("error: failed to initialise S3 client: {e}");
        cli_exit::EXIT_HTTP
    })
}

/// Block on a yes/no confirmation. Returns true on `y` / `yes`.
fn confirm_prompt() -> bool {
    eprint!("Proceed with migration? [y/N] ");
    use std::io::Write;
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserve_prefix_appends_last_source_component() {
        assert_eq!(
            effective_dest_prefix("path/to/foo/", "archive/", true),
            "archive/foo/"
        );
        assert_eq!(
            effective_dest_prefix("path/to/foo", "archive/", true),
            "archive/foo/"
        );
        // Empty dst — last component still gets appended.
        assert_eq!(effective_dest_prefix("foo/", "", true), "foo/");
    }

    #[test]
    fn no_preserve_prefix_returns_dst_verbatim() {
        assert_eq!(
            effective_dest_prefix("path/to/foo/", "archive/", false),
            "archive/"
        );
        assert_eq!(effective_dest_prefix("foo/", "", false), "");
    }

    #[test]
    fn empty_src_prefix_skips_preservation() {
        assert_eq!(effective_dest_prefix("", "archive/", true), "archive/");
    }

    #[test]
    fn dst_without_trailing_slash_gets_one() {
        assert_eq!(
            effective_dest_prefix("releases/", "backup", true),
            "backup/releases/"
        );
    }

    #[test]
    fn plan_skips_keys_already_at_dest() {
        let src_keys = vec![
            "releases/v1.zip".to_string(),
            "releases/v2.zip".to_string(),
            "releases/v3.zip".to_string(),
        ];
        let dst_index: HashSet<String> = ["v2.zip".to_string()].into_iter().collect();
        let filter = Filter::build(&[], &[]).unwrap();
        let plan = build_plan(&src_keys, "releases/", &dst_index, &filter);
        let rel_keys: Vec<_> = plan.iter().map(|(rel, _)| rel.clone()).collect();
        assert_eq!(rel_keys, vec!["v1.zip", "v3.zip"]);
    }

    #[test]
    fn plan_respects_include_exclude_filter() {
        let src_keys = vec![
            "v1.zip".to_string(),
            "v1.txt".to_string(),
            "v2.zip".to_string(),
        ];
        let dst_index: HashSet<String> = HashSet::new();
        let filter = Filter::build(&["*.zip".to_string()], &[]).unwrap();
        let plan = build_plan(&src_keys, "", &dst_index, &filter);
        let rel_keys: Vec<_> = plan.iter().map(|(rel, _)| rel.clone()).collect();
        assert_eq!(rel_keys, vec!["v1.zip", "v2.zip"]);
    }

    #[test]
    fn strip_prefix_handles_empty_prefix() {
        assert_eq!(strip_prefix("releases/v1.zip", ""), "releases/v1.zip");
        assert_eq!(strip_prefix("releases/v1.zip", "releases/"), "v1.zip");
        // Prefix doesn't match → return original (we use this for paths
        // that fell outside the listed prefix, which shouldn't happen in
        // practice but we don't want to panic).
        assert_eq!(strip_prefix("other/v1.zip", "releases/"), "other/v1.zip");
    }
}
