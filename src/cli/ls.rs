// SPDX-License-Identifier: BUSL-1.1

//! `deltaglider_proxy s3 ls` — list buckets or objects with AWS-CLI-shaped
//! output. The command shape mirrors `aws s3 ls`:
//!
//! ```text
//! deltaglider_proxy s3 ls                      # list buckets
//! deltaglider_proxy s3 ls s3://bucket          # list top-level keys + common prefixes
//! deltaglider_proxy s3 ls s3://bucket/pfx/ -r  # recursive
//! deltaglider_proxy s3 ls s3://bucket -h --summarize
//! ```
//!
//! No URL → bucket list. With URL → object list under that prefix.
//! Non-recursive uses `/` as the delimiter; `CommonPrefixes` show as
//! `                           PRE prefix/` rows.

use crate::cli::aws_args::{AwsArgs, EngineLimits};
use crate::cli::config as cli_exit;
use crate::cli::s3_url::{is_s3_url, parse_s3_url};
use chrono::{DateTime, Utc};

/// List S3 buckets or objects.
///
/// With no URL, lists all buckets the credentials can see (with creation
/// dates). With an `s3://bucket[/prefix]` URL, lists objects at that
/// location.
#[derive(clap::Args, Debug, Clone)]
#[command(
    mut_arg("endpoint_url", |a| a.help("S3 endpoint URL (defaults to AWS S3)")),
    mut_arg("region", |a| a.help("AWS region (default chain: flag → env → profile)")),
    mut_arg("profile", |a| a.help("AWS profile (default chain: flag → `AWS_PROFILE` → \"default\")")),
    mut_arg("force_path_style", |a| a.help("Use path-style URLs (required for MinIO / LocalStack)")),
)]
pub struct LsArgs {
    /// S3 URL (`s3://bucket[/prefix]`). Omit to list buckets.
    #[arg(value_name = "S3_URL")]
    pub url: Option<String>,

    /// Recurse into sub-prefixes (no delimiter collapsing).
    #[arg(short, long)]
    pub recursive: bool,

    /// Format sizes in human-readable units (KiB / MiB / GiB / TiB).
    ///
    /// Long form only — clap reserves `-h` for `--help`. Spec lists
    /// `-h/--human-readable` but the short collides with clap's
    /// auto-generated help short; we keep `--help` standard.
    #[arg(long = "human-readable")]
    pub human_readable: bool,

    /// Append `Total Objects: N` / `Total Size: X` footer.
    #[arg(long)]
    pub summarize: bool,

    /// Max objects per page (S3 caps at 1000).
    #[arg(long, value_name = "N", default_value_t = 1000)]
    pub page_size: u32,

    #[command(flatten)]
    pub aws: AwsArgs,
}

/// Run the `ls` command. Returns a CLI exit code (see
/// `crate::cli::config::EXIT_*`).
pub async fn run(args: LsArgs) -> i32 {
    let engine = match args.aws.engine(EngineLimits::default()).await {
        Ok(e) => e,
        Err(code) => return code,
    };

    match args.url.as_deref() {
        None => list_buckets(&engine).await,
        Some(url) if is_s3_url(url) => {
            let loc = match parse_s3_url(url) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("error: bad S3 URL: {e}");
                    return cli_exit::EXIT_PARSE;
                }
            };
            list_objects(
                &engine,
                &loc.bucket,
                &loc.key,
                args.recursive,
                args.human_readable,
                args.summarize,
                args.page_size,
            )
            .await
        }
        Some(other) => {
            eprintln!("error: expected an `s3://` URL, got `{other}`");
            cli_exit::EXIT_USAGE
        }
    }
}

async fn list_buckets(engine: &crate::deltaglider::DynEngine) -> i32 {
    match engine.list_buckets_with_dates().await {
        Ok(buckets) => {
            for (name, created_at) in buckets {
                println!("{} {}", format_timestamp(created_at), name);
            }
            cli_exit::EXIT_OK
        }
        Err(e) => {
            eprintln!("error: failed to list buckets: {e}");
            cli_exit::EXIT_HTTP
        }
    }
}

async fn list_objects(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    prefix: &str,
    recursive: bool,
    human: bool,
    summarize: bool,
    page_size: u32,
) -> i32 {
    let delimiter = if recursive { None } else { Some("/") };
    let mut total_objects: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut continuation: Option<String> = None;

    loop {
        let page = match engine
            .list_objects(
                bucket,
                prefix,
                delimiter,
                page_size,
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

        for cp in &page.common_prefixes {
            // AWS-CLI prefix row: 27-col blank then "PRE " + name.
            println!("                           PRE {cp}");
        }
        for (key, meta) in &page.objects {
            total_objects += 1;
            total_bytes = total_bytes.saturating_add(meta.file_size);
            println!(
                "{}",
                format_ls_row(meta.created_at, meta.file_size, key, human)
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

    if summarize {
        println!();
        println!("Total Objects: {total_objects}");
        println!("Total Size: {}", format_size(total_bytes, human));
    }

    cli_exit::EXIT_OK
}

/// Pure: format one row of `ls` output.
pub(crate) fn format_ls_row(modified: DateTime<Utc>, size: u64, key: &str, human: bool) -> String {
    // AWS-CLI canonical shape: "YYYY-MM-DD HH:MM:SS {size:>10} {key}".
    let ts = format_timestamp(modified);
    let size_str = format_size(size, human);
    format!("{ts} {size_str:>10} {key}")
}

/// Pure: render a UTC timestamp in the AWS-CLI canonical form.
pub(crate) fn format_timestamp(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Pure: render an object size as decimal bytes or binary-prefixed
/// human-readable (KiB / MiB / GiB / TiB).
pub(crate) fn format_size(bytes: u64, human: bool) -> String {
    if !human {
        return bytes.to_string();
    }
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;
    if bytes >= TIB {
        format!("{:.1} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 17, 14, 23, 45).unwrap()
    }

    #[test]
    fn format_ls_row_aws_canonical_shape() {
        let row = format_ls_row(t(), 1024, "releases/v1.zip", false);
        assert_eq!(row, "2026-05-17 14:23:45       1024 releases/v1.zip");
    }

    #[test]
    fn format_ls_row_human_readable_padded() {
        let row = format_ls_row(t(), 1024, "x.zip", true);
        // "1.0 KiB" is 7 chars; padded to width 10.
        assert_eq!(row, "2026-05-17 14:23:45    1.0 KiB x.zip");
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0, false), "0");
        assert_eq!(format_size(0, true), "0 B");
        assert_eq!(format_size(512, true), "512 B");
        assert_eq!(format_size(1024, true), "1.0 KiB");
        assert_eq!(format_size(1024 * 1024, true), "1.0 MiB");
        assert_eq!(format_size(1024_u64.pow(3), true), "1.0 GiB");
        assert_eq!(format_size(1024_u64.pow(4), true), "1.0 TiB");
        assert_eq!(format_size(1536, true), "1.5 KiB");
    }

    #[test]
    fn format_timestamp_is_iso_minus_t() {
        assert_eq!(format_timestamp(t()), "2026-05-17 14:23:45");
    }
}
