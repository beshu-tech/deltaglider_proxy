// SPDX-License-Identifier: GPL-3.0-only

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

    /// Include patterns (basename glob OR full-key glob with `/`).
    /// Repeatable.
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
        max_delta_ratio: None,
        max_object_size: None,
        allow_local: should_allow_local(args.endpoint_url.as_deref()),
    };
    let engine = match build_cli_engine(opts).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: failed to initialise S3 client: {e}");
            return cli_exit::EXIT_HTTP;
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
        Ok(()) => cli_exit::EXIT_OK,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("NoSuchKey") || msg.contains("not found") {
                eprintln!("error: object not found: s3://{bucket}/{key}");
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

    let mut continuation: Option<String> = None;
    let mut succeeded: u64 = 0;
    let mut failed: u64 = 0;

    loop {
        let page = match engine
            .list_objects(bucket, prefix, None, 1000, continuation.as_deref(), false)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: list_objects failed: {e}");
                return cli_exit::EXIT_HTTP;
            }
        };

        for (key, _meta) in &page.objects {
            if !filter.matches(key) {
                continue;
            }
            if !args.quiet {
                println!("delete: s3://{bucket}/{key}");
            }
            if args.dryrun {
                succeeded += 1;
                continue;
            }
            match engine.delete(bucket, key).await {
                Ok(()) => succeeded += 1,
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

#[cfg(test)]
mod tests {
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
