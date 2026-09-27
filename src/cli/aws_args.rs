// SPDX-License-Identifier: BUSL-1.1

//! The AWS connection flags that every `s3` verb takes, and the one
//! path from those flags to credentials, an engine, or a raw client.
//!
//! Each verb flattens [`AwsArgs`] into its own clap struct at the
//! place where the flags show in `--help`. A verb that needs other help
//! text for one of these flags overrides it on its own struct:
//! `#[command(mut_arg(..))]` in `ls.rs`, `#[command(mut_args(..))]` in
//! `migrate.rs`.

use crate::cli::aws_creds::{self, CredsInputs, ResolvedCreds};
use crate::cli::config as cli_exit;
use crate::cli::engine_factory::{build_cli_engine, build_raw_s3_client, CliEngineOpts};
use crate::deltaglider::DynEngine;

#[derive(clap::Args, Debug, Clone, Default)]
pub struct AwsArgs {
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

/// The engine knobs a verb may override (the rest of [`CliEngineOpts`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct EngineLimits {
    pub max_delta_ratio: Option<f32>,
    /// Bytes.
    pub max_object_size: Option<u64>,
}

impl EngineLimits {
    /// Limits from the `--max-ratio` / `--max-object-size-mb` flags.
    pub fn from_flags(max_delta_ratio: Option<f32>, max_object_size_mb: Option<u64>) -> Self {
        Self {
            max_delta_ratio,
            max_object_size: max_object_size_mb.map(|mb| mb * 1024 * 1024),
        }
    }
}

impl AwsArgs {
    /// Pure: the flag half of the credential chain.
    pub(crate) fn creds_inputs(&self) -> CredsInputs<'_> {
        CredsInputs {
            access_key_flag: self.access_key_id.as_deref(),
            secret_key_flag: self.secret_access_key.as_deref(),
            region_flag: self.region.as_deref(),
            profile_flag: self.profile.as_deref(),
            ..Default::default()
        }
    }

    /// Resolve credentials (flag → env → profile file). On failure,
    /// print the reason and return the exit code.
    pub fn resolve(&self) -> Result<ResolvedCreds, i32> {
        aws_creds::resolve(self.creds_inputs()).map_err(|e| {
            eprintln!("error: {e}");
            cli_exit::EXIT_AUTH
        })
    }

    /// Pure: engine options for `endpoint` with these flags and `creds`.
    pub fn engine_opts(
        &self,
        creds: &ResolvedCreds,
        endpoint: Option<&str>,
        limits: EngineLimits,
    ) -> CliEngineOpts {
        CliEngineOpts {
            endpoint: endpoint.map(str::to_string),
            region: creds.region.clone().unwrap_or_else(|| "us-east-1".into()),
            force_path_style: self.force_path_style,
            access_key_id: creds.access_key_id.clone(),
            secret_access_key: creds.secret_access_key.clone(),
            session_token: creds.session_token.clone(),
            max_delta_ratio: limits.max_delta_ratio,
            max_object_size: limits.max_object_size,
            allow_local: should_allow_local(endpoint),
        }
    }

    /// Resolve credentials and build an engine on `--endpoint-url`. On
    /// failure, print the reason and return the exit code.
    pub async fn engine(&self, limits: EngineLimits) -> Result<DynEngine, i32> {
        let creds = self.resolve()?;
        let opts = self.engine_opts(&creds, self.endpoint_url.as_deref(), limits);
        build_cli_engine(opts).await.map_err(|e| {
            eprintln!("error: failed to initialise S3 client: {e}");
            e.exit_code()
        })
    }

    /// Resolve credentials and build a raw SDK client on
    /// `--endpoint-url` (verbs that do not store objects). On failure,
    /// print the reason and return the exit code.
    pub async fn client(&self) -> Result<aws_sdk_s3::Client, i32> {
        let creds = self.resolve()?;
        build_raw_s3_client(&creds, self.endpoint_url.clone(), self.force_path_style)
            .await
            .map_err(|e| {
                eprintln!("error: failed to initialise S3 client: {e}");
                e.exit_code()
            })
    }
}

/// Set `DGP_BACKEND_ALLOW_LOCAL` automatically when the user
/// explicitly points us at a local endpoint. Heuristic: `http://`
/// scheme OR a `localhost` / loopback host. Server-process equivalent
/// stays config-driven; this is the documented CLI ergonomic.
///
/// Shared with every other S3-talking subcommand (`rm`, `cp`, `stats`,
/// `verify`) so they all auto-detect dev / MinIO endpoints the same
/// way.
pub(crate) fn should_allow_local(endpoint: Option<&str>) -> bool {
    let Some(ep) = endpoint else {
        return false;
    };
    if ep.starts_with("http://") {
        return true;
    }
    ep.contains("localhost") || ep.contains("127.0.0.1") || ep.contains("[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::aws_creds::CredsSource;

    fn creds(region: Option<&str>) -> ResolvedCreds {
        ResolvedCreds {
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            session_token: Some("TOK".into()),
            region: region.map(str::to_string),
            source: CredsSource::Flag,
        }
    }

    #[test]
    fn every_flag_reaches_the_credential_chain() {
        let args = AwsArgs {
            endpoint_url: None,
            region: Some("r".into()),
            profile: Some("p".into()),
            access_key_id: Some("ak".into()),
            secret_access_key: Some("sk".into()),
            force_path_style: false,
        };
        let i = args.creds_inputs();
        assert_eq!(i.access_key_flag, Some("ak"));
        assert_eq!(i.secret_key_flag, Some("sk"));
        assert_eq!(i.region_flag, Some("r"));
        assert_eq!(i.profile_flag, Some("p"));
        // No verb has a session-token flag: the token comes from env
        // or the profile file only.
        assert_eq!(i.session_token_flag, None);
    }

    #[test]
    fn engine_opts_carry_creds_limits_and_the_endpoint() {
        let args = AwsArgs {
            force_path_style: true,
            ..Default::default()
        };
        let o = args.engine_opts(
            &creds(Some("eu-west-1")),
            Some("http://127.0.0.1:9000"),
            EngineLimits::from_flags(Some(0.5), Some(3)),
        );
        assert_eq!(o.endpoint.as_deref(), Some("http://127.0.0.1:9000"));
        assert_eq!(o.region, "eu-west-1");
        assert!(o.force_path_style);
        assert_eq!(o.access_key_id, "AK");
        assert_eq!(o.secret_access_key, "SK");
        assert_eq!(o.session_token.as_deref(), Some("TOK"));
        assert_eq!(o.max_delta_ratio, Some(0.5));
        assert_eq!(o.max_object_size, Some(3 * 1024 * 1024));
        assert!(o.allow_local, "a local endpoint lifts the SSRF guard");
    }

    #[test]
    fn engine_opts_default_the_region_and_keep_the_ssrf_guard_for_aws() {
        let o = AwsArgs::default().engine_opts(&creds(None), None, EngineLimits::default());
        assert_eq!(o.region, "us-east-1");
        assert_eq!(o.endpoint, None);
        assert!(!o.allow_local);
        assert_eq!(o.max_delta_ratio, None);
        assert_eq!(o.max_object_size, None);
    }

    /// Every `s3` verb takes the six connection flags.
    #[test]
    fn every_verb_flattens_the_connection_flags() {
        use crate::cli::*;
        use clap::Args;
        let cmds = [
            ls::LsArgs::augment_args(clap::Command::new("ls")),
            rm::RmArgs::augment_args(clap::Command::new("rm")),
            cp::CpArgs::augment_args(clap::Command::new("cp")),
            stats::StatsArgs::augment_args(clap::Command::new("stats")),
            verify::VerifyArgs::augment_args(clap::Command::new("verify")),
            bucket_acl::GetArgs::augment_args(clap::Command::new("get-bucket-acl")),
            bucket_acl::PutArgs::augment_args(clap::Command::new("put-bucket-acl")),
            migrate::MigrateArgs::augment_args(clap::Command::new("migrate")),
            sync::SyncArgs::augment_args(clap::Command::new("sync")),
            purge::PurgeArgs::augment_args(clap::Command::new("purge")),
        ];
        for cmd in cmds {
            let longs: Vec<_> = cmd.get_arguments().filter_map(|a| a.get_long()).collect();
            for flag in [
                "endpoint-url",
                "region",
                "profile",
                "access-key-id",
                "secret-access-key",
                "force-path-style",
            ] {
                assert!(longs.contains(&flag), "{} lacks --{flag}", cmd.get_name());
            }
        }
    }

    /// `migrate --help` lists `--source-endpoint-url` right after
    /// `--endpoint-url`, as before the flags moved into `AwsArgs`.
    #[test]
    fn migrate_help_keeps_the_source_endpoint_next_to_the_endpoint() {
        use clap::Args;
        let mut cmd = crate::cli::migrate::MigrateArgs::augment_args(clap::Command::new("m"));
        let help = cmd.render_help().to_string();
        let at = |f: &str| help.find(f).unwrap_or_else(|| panic!("{f} missing"));
        assert!(at("--endpoint-url <URL>") < at("--source-endpoint-url"));
        assert!(at("--source-endpoint-url") < at("--region"));
        assert!(help.contains("S3 endpoint URL for the destination side"));
    }

    #[test]
    fn should_allow_local_recognises_dev_endpoints() {
        assert!(should_allow_local(Some("http://localhost:9000")));
        assert!(should_allow_local(Some("http://127.0.0.1:9000")));
        assert!(should_allow_local(Some("https://localhost:9000")));
        assert!(should_allow_local(Some("https://[::1]:9000")));
        assert!(should_allow_local(Some("http://10.0.0.5")));
        assert!(!should_allow_local(Some("https://s3.amazonaws.com")));
        assert!(!should_allow_local(None));
    }
}
