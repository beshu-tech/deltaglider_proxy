// SPDX-License-Identifier: BUSL-1.1

//! `deltaglider_proxy s3 rm s3://bucket/key [-r] [--include G]... [--exclude G]...`
//!
//! Single-key delete by default. With `-r` walks the prefix (paginated)
//! and deletes every key that survives the `Filter`. Output mirrors
//! `aws s3 rm`: `delete: s3://bucket/key` per removed (or `--dryrun`'d)
//! object.

use crate::cli::aws_creds;
use crate::cli::config as cli_exit;
use crate::cli::engine_factory::{build_cli_engine, CliEngineOpts};
use crate::cli::filter::Filter;
use crate::cli::keys::{dir_prefix, rel_under};
use crate::cli::ls::should_allow_local;
use crate::cli::s3_url::{is_s3_url, parse_s3_url};
use crate::deltaglider::DynEngine;

/// Remove S3 objects (AWS-CLI-shaped).
#[derive(clap::Args, Debug, Clone)]
pub struct RmArgs {
    /// S3 URL to remove (`s3://bucket/key` or `s3://bucket/prefix/` with `-r`).
    #[arg(value_name = "S3_URL")]
    pub url: String,

    /// Recursively delete every key under the prefix.
    #[arg(short, long)]
    pub recursive: bool,

    /// Include patterns (basename glob, OR a glob with `/` matched
    /// against the key relative to the prefix, like `cp`). Repeatable.
    #[arg(long, value_name = "GLOB")]
    pub include: Vec<String>,

    /// Exclude patterns. Exclude wins over include. Repeatable.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Print what would be deleted without actually deleting. AWS-CLI
    /// spelling `--dryrun` (no dash).
    #[arg(long)]
    pub dryrun: bool,

    /// Suppress per-object output (`delete: …` lines).
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

pub async fn run(args: RmArgs) -> i32 {
    if !is_s3_url(&args.url) {
        eprintln!("error: expected an `s3://` URL, got `{}`", args.url);
        return cli_exit::EXIT_USAGE;
    }
    let loc = match parse_s3_url(&args.url) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: bad S3 URL: {e}");
            return cli_exit::EXIT_PARSE;
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
        session_token: creds.session_token,
        max_delta_ratio: None,
        max_object_size: None,
        allow_local: should_allow_local(args.endpoint_url.as_deref()),
    };
    let engine = match build_cli_engine(opts).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: failed to initialise S3 client: {e}");
            return e.exit_code();
        }
    };

    if args.recursive {
        rm_recursive(&engine, &args, &loc.bucket, &loc.key).await
    } else {
        if loc.key.is_empty() {
            eprintln!("error: cannot rm a bucket prefix without `--recursive`");
            return cli_exit::EXIT_USAGE;
        }
        rm_one(&engine, &args, &loc.bucket, &loc.key).await
    }
}

async fn rm_one(engine: &DynEngine, args: &RmArgs, bucket: &str, key: &str) -> i32 {
    if !args.quiet {
        println!("delete: s3://{bucket}/{key}");
    }
    if args.dryrun {
        return cli_exit::EXIT_OK;
    }
    match engine.delete(bucket, key).await {
        Ok(_) => cli_exit::EXIT_OK,
        Err(e) => {
            if let Some(what) = super::missing(&e) {
                eprintln!("error: {what} not found: s3://{bucket}/{key}");
                cli_exit::EXIT_NOT_FOUND
            } else {
                eprintln!("error: delete failed: {e}");
                cli_exit::EXIT_HTTP
            }
        }
    }
}

async fn rm_recursive(engine: &DynEngine, args: &RmArgs, bucket: &str, prefix: &str) -> i32 {
    let filter = match Filter::build(&args.include, &args.exclude) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: invalid include/exclude pattern: {e}");
            return cli_exit::EXIT_USAGE;
        }
    };

    // Directory semantics: `releases` means `releases/`, never
    // `releases-old/`.
    let dir = dir_prefix(prefix);
    let mut continuation: Option<String> = None;
    let mut succeeded: u64 = 0;
    let mut failed: u64 = 0;

    loop {
        let page = match engine
            .list_objects(bucket, &dir, None, 1000, continuation.as_deref(), false)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: list_objects failed: {e}");
                return cli_exit::EXIT_HTTP;
            }
        };

        let keys: Vec<&str> = page.objects.iter().map(|(k, _)| k.as_str()).collect();
        for key in rm_targets(&keys, &dir, &filter) {
            if !args.quiet {
                println!("delete: s3://{bucket}/{key}");
            }
            if args.dryrun {
                succeeded += 1;
                continue;
            }
            match engine.delete(bucket, key).await {
                Ok(_) => succeeded += 1,
                Err(e) => {
                    eprintln!("warning: delete s3://{bucket}/{key} failed: {e}");
                    failed += 1;
                }
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

    if failed > 0 && succeeded > 0 {
        cli_exit::EXIT_PARTIAL
    } else if failed > 0 {
        cli_exit::EXIT_HTTP
    } else {
        cli_exit::EXIT_OK
    }
}

/// Pure: the keys of one listing page that `rm -r` deletes. Globs see
/// the key relative to the prefix, the same as `cp -r`. `prefix`
/// is normalised with [`dir_prefix`] here too, so a caller cannot
/// forget it. The folder marker of the directory itself (the key
/// `releases/`) goes too, as with `aws s3 rm --recursive`, unless a glob
/// narrows the delete: then the folder stays.
pub(crate) fn rm_targets<'a>(keys: &[&'a str], prefix: &str, filter: &Filter) -> Vec<&'a str> {
    let dir = dir_prefix(prefix);
    keys.iter()
        .copied()
        .filter(|k| {
            rel_under(k, &dir).is_some_and(|rel| filter.matches(rel))
                || (!dir.is_empty() && *k == dir && filter.accepts_all())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(inc: &[&str], exc: &[&str]) -> Filter {
        let inc: Vec<String> = inc.iter().map(|s| s.to_string()).collect();
        let exc: Vec<String> = exc.iter().map(|s| s.to_string()).collect();
        Filter::build(&inc, &exc).unwrap()
    }

    const KEYS: &[&str] = &[
        "releases",
        "releases/v1.zip",
        "releases/tmp/scratch.zip",
        "releases-old/v0.zip",
    ];

    /// `rm -r s3://b/releases` means the `releases/` directory, never
    /// the sibling `releases-old/`.
    #[test]
    fn prefix_without_slash_is_a_directory() {
        assert_eq!(
            rm_targets(KEYS, "releases", &filter(&[], &[])),
            vec!["releases/v1.zip", "releases/tmp/scratch.zip"]
        );
    }

    /// Globs match the key relative to the prefix, as in `cp`, `sync`
    /// and `aws s3 rm`.
    #[test]
    fn globs_match_relative_to_the_prefix() {
        assert_eq!(
            rm_targets(KEYS, "releases/", &filter(&[], &["tmp/*"])),
            vec!["releases/v1.zip"]
        );
        assert_eq!(
            rm_targets(KEYS, "releases/", &filter(&["tmp/*"], &[])),
            vec!["releases/tmp/scratch.zip"]
        );
    }

    /// Review D3: `rm -r` deletes folder markers (keys ending in `/`) by
    /// key, the directory's own marker included, and never relies on a
    /// server-side prefix sweep.
    #[test]
    fn folder_markers_are_deleted_by_key() {
        let keys = &[
            "releases/",
            "releases/tmp/",
            "releases/v1.zip",
            "releases-old/",
        ];
        assert_eq!(
            rm_targets(keys, "releases", &filter(&[], &[])),
            vec!["releases/", "releases/tmp/", "releases/v1.zip"]
        );
        assert_eq!(
            rm_targets(keys, "releases/", &filter(&["*.zip"], &[])),
            vec!["releases/v1.zip"]
        );
        assert_eq!(
            rm_targets(keys, "releases/", &filter(&[], &["tmp/"])),
            vec!["releases/v1.zip"]
        );
    }

    /// Single-key rm without `--recursive` requires a non-empty key —
    /// "s3://bucket" alone is a programming error (would be a bucket
    /// delete, which we leave to a future explicit subcommand). This
    /// is a shape test against the URL parser; the live behaviour is
    /// covered by the integration tests.
    #[test]
    fn empty_key_without_recursive_is_usage_error() {
        let url = "s3://bucket"; // would parse to key = ""
        let loc = crate::cli::s3_url::parse_s3_url(url).unwrap();
        assert!(loc.key.is_empty());
    }
}
