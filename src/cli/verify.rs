// SPDX-License-Identifier: GPL-3.0-only

//! `deltaglider_proxy verify s3://bucket/key`
//!
//! Pull an object back through the engine (which handles delta
//! reconstruction transparently), recompute SHA256 on the reassembled
//! bytes, and compare against `FileMetadata.file_sha256`. The engine
//! already raises `ChecksumMismatch` during reconstruction —
//! `verify` adds a belt-and-suspenders client-side recompute that
//! catches in-flight corruption between the engine and the user-facing
//! buffer.

use crate::cli::aws_creds;
use crate::cli::config as cli_exit;
use crate::cli::engine_factory::{build_cli_engine, CliEngineOpts};
use crate::cli::ls::should_allow_local;
use crate::cli::s3_url::{is_s3_url, parse_s3_url};
use sha2::{Digest, Sha256};

/// Verify the integrity of an S3 object stored via DeltaGlider.
#[derive(clap::Args, Debug, Clone)]
pub struct VerifyArgs {
    /// S3 URL (`s3://bucket/key`).
    #[arg(value_name = "S3_URL")]
    pub url: String,

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

pub async fn run(args: VerifyArgs) -> i32 {
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
    if loc.key.is_empty() {
        eprintln!("error: verify requires an object key, not a bucket or prefix");
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

    let (data, metadata) = match engine.retrieve(&loc.bucket, &loc.key).await {
        Ok(t) => t,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("NoSuchKey") || msg.contains("not found") {
                eprintln!("error: object not found: {}", args.url);
                return cli_exit::EXIT_NOT_FOUND;
            }
            if msg.contains("ChecksumMismatch") || msg.contains("checksum mismatch") {
                // The engine itself caught the mismatch during
                // reconstruction — surface it as the integrity error.
                eprintln!("MISMATCH: engine reported checksum mismatch: {e}");
                return cli_exit::EXIT_INTEGRITY;
            }
            eprintln!("error: retrieve failed: {e}");
            return cli_exit::EXIT_HTTP;
        }
    };

    let observed = hex_sha256(&data);
    if observed.eq_ignore_ascii_case(&metadata.file_sha256) {
        println!(
            "OK: {} (sha256={observed}, size={size})",
            args.url,
            size = data.len()
        );
        cli_exit::EXIT_OK
    } else {
        eprintln!(
            "MISMATCH: {url}\n  expected sha256: {expected}\n  observed sha256: {observed}\n  size: {size}",
            url = args.url,
            expected = metadata.file_sha256,
            size = data.len()
        );
        cli_exit::EXIT_INTEGRITY
    }
}

/// Pure: hex-encoded SHA256 over the bytes.
fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_sha256_of_known_input() {
        // SHA256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hex_sha256_of_abc() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
