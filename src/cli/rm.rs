// SPDX-License-Identifier: BUSL-1.1

//! `deltaglider_proxy s3 rm s3://bucket/key [-r] [--include G]... [--exclude G]...`
//!
//! Single-key delete by default. With `-r` walks the prefix (paginated)
//! and deletes every key that survives the `Filter`. Output mirrors
//! `aws s3 rm`: `delete: s3://bucket/key` per removed (or `--dryrun`'d)
//! object.

use crate::cli::aws_args::{AwsArgs, EngineLimits};
use crate::cli::config as cli_exit;
use crate::cli::filter::Filter;
use crate::cli::keys::{dir_prefix, rel_under};
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

    #[command(flatten)]
    pub aws: AwsArgs,
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

    crate::cli::aws_args::print_live_proxy_notice(!args.dryrun, args.quiet);
    let engine = match args.aws.engine(EngineLimits::default()).await {
        Ok(e) => e,
        Err(code) => return code,
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
        let targets = rm_targets(&keys, &dir, &filter);
        if args.dryrun {
            for key in &targets {
                if !args.quiet {
                    println!("delete: s3://{bucket}/{key}");
                }
            }
            succeeded += targets.len() as u64;
        } else {
            // The page in one batch: one reclaim check per folder, several
            // folders at once (a per-key delete listed the folder after
            // every key). Tokens are keys, so the deletes skip no key.
            let (ok, bad) = delete_keys(engine, bucket, &targets, args.quiet).await;
            succeeded += ok;
            failed += bad;
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

/// Delete `keys` of `bucket` through the engine's batch, printing
/// `delete: s3://…` for each key that went (unless `quiet`) and a warning
/// for each failure. Returns (deleted, failed). A key already gone counts
/// as deleted, as for `aws s3 rm`.
pub(crate) async fn delete_keys(
    engine: &DynEngine,
    bucket: &str,
    keys: &[&str],
    quiet: bool,
) -> (u64, u64) {
    use crate::deltaglider::{DeleteHooks, DeleteItem, DeleteOutcome};
    let items = keys.iter().map(|k| DeleteItem::new(*k)).collect();
    let outcomes = engine
        .delete_batch(bucket, items, DeleteHooks::default())
        .await;
    let (mut ok, mut bad) = (0, 0);
    for (key, outcome) in keys.iter().zip(outcomes) {
        match outcome {
            DeleteOutcome::Failed(e) => {
                eprintln!("warning: delete s3://{bucket}/{key} failed: {e}");
                bad += 1;
            }
            _ => {
                if !quiet {
                    println!("delete: s3://{bucket}/{key}");
                }
                ok += 1;
            }
        }
    }
    (ok, bad)
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

    /// NEW-06, recorded and not fixed (D3: CLI write verbs next to a live
    /// proxy are not supported; the CLI prints the rule instead). `rm`
    /// deletes the last object of a prefix, finds only reference.bin left
    /// and deletes it. A proxy stores a delta against that reference
    /// between the check and the delete, and the delta loses its baseline.
    /// The proxy is another process: its write takes none of this
    /// process's locks, so the test writes its delta file directly (the
    /// same delta, encoded by an engine on another directory against the
    /// same reference).
    #[tokio::test]
    #[ignore = "manual: cargo test --lib -- --ignored a_live_proxy_delta_keeps_its_reference_during_rm (fails by design, D3: the CLI holds no lock that a live proxy sees; see docs/product/reference/cli.md)"]
    async fn a_live_proxy_delta_keeps_its_reference_during_rm() {
        use crate::storage::{DynStorageBackend, FaultPoint, FaultyFs, FilesystemBackend};
        use std::collections::HashMap;
        use std::sync::Arc;
        async fn engine_on(dir: &std::path::Path) -> (DynEngine, crate::storage::Faults) {
            let faulty = FaultyFs::new(FilesystemBackend::new(dir.to_path_buf()).await.unwrap());
            let faults = faulty.faults.clone();
            let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(faulty);
            let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
                Arc::new(backend),
                &crate::config::Config::default(),
                None,
            );
            engine.create_bucket("releases").await.unwrap();
            (engine, faults)
        }
        let base: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut next = base.clone();
        next[1000..1100].fill(9);
        // The proxy's delta of app-2, against the same reference.
        let elsewhere = tempfile::tempdir().unwrap();
        let (proxy, _) = engine_on(elsewhere.path()).await;
        for (key, body) in [("v1/app-1.zip", &base), ("v1/app-2.zip", &next)] {
            proxy
                .store("releases", key, body, None, HashMap::new())
                .await
                .unwrap();
        }
        let delta = "releases/deltaspaces/v1/app-2.zip.delta";
        let peer_delta = elsewhere.path().join(delta);
        assert!(peer_delta.exists(), "fixture: app-2 is a delta");

        let dir = tempfile::tempdir().unwrap();
        let (cli, faults) = engine_on(dir.path()).await;
        cli.store("releases", "v1/app-1.zip", &base, None, HashMap::new())
            .await
            .unwrap();
        let target = dir.path().join(delta);
        faults.hook(
            FaultPoint::DeleteReference,
            "releases/v1/",
            Arc::new(move || {
                let (from, to) = (peer_delta.clone(), target.clone());
                Box::pin(async move {
                    std::fs::copy(&from, &to).unwrap();
                    let name = crate::storage::xattr_meta::XATTR_NAME;
                    let meta = xattr::get(&from, name).unwrap().unwrap();
                    xattr::set(&to, name, &meta).unwrap();
                })
            }),
        );
        let (deleted, failed) = delete_keys(&cli, "releases", &["v1/app-1.zip"], true).await;
        assert_eq!((deleted, failed), (1, 0));
        assert_eq!(faults.fired(FaultPoint::DeleteReference, "releases/v1/"), 1);
        let read = cli.retrieve("releases", "v1/app-2.zip").await;
        assert!(
            matches!(&read, Ok(r) if r.0 == next),
            "the proxy's delta lost its reference: {:?}",
            read.map(|r| r.0.len())
        );
    }

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

    /// jobs-16: `rm -r` deleted key by key, and each delete listed the
    /// key's folder to reclaim its reference.bin: one LIST per key, one key
    /// at a time. A page of keys goes in one batch: one check per folder.
    #[tokio::test]
    async fn rm_r_checks_each_folder_once() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        crate::deltaglider::store_deltas(&engine, "run", 50).await;
        fake.clear();
        let args = RmArgs {
            url: "s3://b/run".into(),
            recursive: true,
            include: vec![],
            exclude: vec![],
            dryrun: false,
            quiet: true,
            aws: Default::default(),
        };
        assert_eq!(
            rm_recursive(&engine, &args, "b", "run").await,
            cli_exit::EXIT_OK
        );
        let checks = fake
            .requests()
            .iter()
            .filter(|r| {
                r.contains("delimiter=%2F") && crate::storage::fake_s3::list_prefix(r) == "run/"
            })
            .count();
        assert_eq!(checks, 1, "{:#?}", fake.requests());
        let left = engine
            .list_objects("b", "run/", None, 10, None, false)
            .await
            .unwrap();
        assert!(left.objects.is_empty(), "{:?}", left.objects);
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
