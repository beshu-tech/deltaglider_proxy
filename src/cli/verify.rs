// SPDX-License-Identifier: BUSL-1.1

//! `deltaglider_proxy s3 verify s3://bucket/key`
//!
//! Pull an object back through the engine (which handles delta
//! reconstruction transparently), recompute SHA256 on the reassembled
//! bytes, and compare against `FileMetadata.file_sha256`. The engine
//! already raises `ChecksumMismatch` during reconstruction —
//! `verify` adds a belt-and-suspenders client-side recompute that
//! catches in-flight corruption between the engine and the user-facing
//! buffer.

use crate::cli::aws_args::{AwsArgs, EngineLimits};
use crate::cli::config as cli_exit;
use crate::cli::s3_url::{is_s3_url, parse_s3_url};
use crate::cli::transfer_io;

/// Verify the integrity of an S3 object stored via DeltaGlider.
#[derive(clap::Args, Debug, Clone)]
pub struct VerifyArgs {
    /// S3 URL (`s3://bucket/key`).
    #[arg(value_name = "S3_URL")]
    pub url: String,

    #[command(flatten)]
    pub aws: AwsArgs,
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

    let engine = match args.aws.engine(EngineLimits::default()).await {
        Ok(e) => e,
        Err(code) => return code,
    };

    // Streamed: the object is hashed chunk by chunk, never held whole.
    let (observed, size, metadata) =
        match transfer_io::hash_object(&engine, &loc.bucket, &loc.key).await {
            Ok(t) => t,
            Err(e) => {
                if let Some(what) = super::missing(&e) {
                    eprintln!("error: {what} not found: {}", args.url);
                    return cli_exit::EXIT_NOT_FOUND;
                }
                if matches!(e, crate::deltaglider::EngineError::ChecksumMismatch { .. }) {
                    // The engine itself caught the mismatch during
                    // reconstruction — surface it as the integrity error.
                    eprintln!("MISMATCH: engine reported checksum mismatch: {e}");
                    return cli_exit::EXIT_INTEGRITY;
                }
                eprintln!("error: retrieve failed: {e}");
                return cli_exit::EXIT_HTTP;
            }
        };

    match verdict(&metadata.file_sha256, &observed) {
        Verdict::Ok => {
            println!("OK: {} (sha256={observed}, size={size})", args.url);
            cli_exit::EXIT_OK
        }
        // Not an integrity failure: the read succeeded, there is just
        // no stored checksum. Exit 0 so a sweep over a mixed bucket
        // does not flag objects written by other tools.
        Verdict::Unverifiable => {
            println!(
                "UNVERIFIABLE: {} has no DeltaGlider checksum (not written through DeltaGlider) \
                 (sha256={observed}, size={size})",
                args.url
            );
            cli_exit::EXIT_OK
        }
        Verdict::Mismatch => {
            eprintln!(
            "MISMATCH: {url}\n  expected sha256: {expected}\n  observed sha256: {observed}\n  size: {size}",
            url = args.url,
            expected = metadata.file_sha256,
        );
            cli_exit::EXIT_INTEGRITY
        }
    }
}

/// Outcome of comparing the recomputed hash with the stored one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Ok,
    Mismatch,
    /// The object carries no DeltaGlider checksum (it was not written
    /// through DeltaGlider), so there is nothing to compare against.
    Unverifiable,
}

/// Pure: compare the stored `dg-file-sha256` with the observed hash.
pub(crate) fn verdict(expected: &str, observed: &str) -> Verdict {
    if expected.is_empty() {
        Verdict::Unverifiable
    } else if observed.eq_ignore_ascii_case(expected) {
        Verdict::Ok
    } else {
        Verdict::Mismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn hex_sha256(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    #[test]
    fn verdict_table() {
        let h = hex_sha256(b"abc");
        assert_eq!(verdict(&h, &h), Verdict::Ok);
        assert_eq!(verdict(&h.to_uppercase(), &h), Verdict::Ok);
        assert_eq!(verdict(&hex_sha256(b"abd"), &h), Verdict::Mismatch);
        // Foreign object: no stored checksum is not a mismatch.
        assert_eq!(verdict("", &h), Verdict::Unverifiable);
    }
}
