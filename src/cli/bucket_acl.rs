// SPDX-License-Identifier: GPL-3.0-only

//! `deltaglider_proxy get-bucket-acl s3://bucket`
//! `deltaglider_proxy put-bucket-acl s3://bucket [--acl …|--grant-… …]`
//!
//! Both commands are thin pass-throughs to the underlying AWS SDK —
//! ACL semantics aren't deltaglider-specific. We share the same
//! credential resolution / endpoint plumbing as the rest of the CLI,
//! but build a one-shot `aws_sdk_s3::Client` directly rather than
//! going through the engine (ACLs don't touch object content).

use crate::cli::aws_creds;
use crate::cli::config as cli_exit;
use crate::cli::ls::should_allow_local;
use crate::cli::s3_url::{is_s3_url, parse_s3_url};
use crate::config::BackendConfig;
use crate::storage::S3Backend;
use aws_sdk_s3::types::BucketCannedAcl;
use serde::Serialize;

#[derive(clap::Args, Debug, Clone)]
pub struct GetArgs {
    /// S3 URL (`s3://bucket` — bucket-scoped only).
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

#[derive(clap::Args, Debug, Clone)]
pub struct PutArgs {
    /// S3 URL (`s3://bucket` — bucket-scoped only).
    #[arg(value_name = "S3_URL")]
    pub url: String,

    /// Canned ACL: one of `private`, `public-read`, `public-read-write`,
    /// `authenticated-read`. Mutually exclusive with the `--grant-*`
    /// flags per AWS S3 semantics — the SDK rejects mixing them.
    #[arg(long, value_name = "CANNED")]
    pub acl: Option<String>,

    /// Grant full control. Format: `id=<canonical-id>` (or
    /// `emailAddress=<...>`, `uri=<group-uri>`).
    #[arg(long, value_name = "GRANTEE")]
    pub grant_full_control: Option<String>,

    /// Grant read permission. Same grantee format as
    /// `--grant-full-control`.
    #[arg(long, value_name = "GRANTEE")]
    pub grant_read: Option<String>,

    /// Grant permission to read the bucket ACL itself.
    #[arg(long, value_name = "GRANTEE")]
    pub grant_read_acp: Option<String>,

    /// Grant write permission (create / overwrite / delete objects).
    #[arg(long, value_name = "GRANTEE")]
    pub grant_write: Option<String>,

    /// Grant permission to modify the bucket ACL.
    #[arg(long, value_name = "GRANTEE")]
    pub grant_write_acp: Option<String>,

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

/// JSON-serialisable wire shape mirroring the AWS CLI's `get-bucket-acl`
/// output. We don't try to derive serde on the SDK types (they're
/// non-exhaustive and codegen-fragile) — we lift the fields we care
/// about into plain structs and serialise those.
#[derive(Debug, Serialize)]
struct AclResponse {
    #[serde(rename = "Owner")]
    owner: Option<OwnerJson>,
    #[serde(rename = "Grants")]
    grants: Vec<GrantJson>,
}

#[derive(Debug, Serialize)]
struct OwnerJson {
    #[serde(rename = "DisplayName", skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(rename = "ID", skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Debug, Serialize)]
struct GrantJson {
    #[serde(rename = "Grantee", skip_serializing_if = "Option::is_none")]
    grantee: Option<GranteeJson>,
    #[serde(rename = "Permission", skip_serializing_if = "Option::is_none")]
    permission: Option<String>,
}

#[derive(Debug, Serialize)]
struct GranteeJson {
    #[serde(rename = "Type")]
    r#type: String,
    #[serde(rename = "DisplayName", skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(rename = "EmailAddress", skip_serializing_if = "Option::is_none")]
    email_address: Option<String>,
    #[serde(rename = "ID", skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "URI", skip_serializing_if = "Option::is_none")]
    uri: Option<String>,
}

pub async fn get_run(args: GetArgs) -> i32 {
    let bucket = match validate_bucket_only(&args.url) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let client = match build_client(
        args.access_key_id.as_deref(),
        args.secret_access_key.as_deref(),
        args.region.as_deref(),
        args.profile.as_deref(),
        args.endpoint_url.clone(),
        args.force_path_style,
    )
    .await
    {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.get_bucket_acl().bucket(&bucket).send().await {
        Ok(out) => {
            let resp = AclResponse {
                owner: out.owner().map(|o| OwnerJson {
                    display_name: o.display_name().map(str::to_string),
                    id: o.id().map(str::to_string),
                }),
                grants: out
                    .grants()
                    .iter()
                    .map(|g| GrantJson {
                        grantee: g.grantee().map(|gr| GranteeJson {
                            r#type: gr.r#type().as_str().to_string(),
                            display_name: gr.display_name().map(str::to_string),
                            email_address: gr.email_address().map(str::to_string),
                            id: gr.id().map(str::to_string),
                            uri: gr.uri().map(str::to_string),
                        }),
                        permission: g.permission().map(|p| p.as_str().to_string()),
                    })
                    .collect(),
            };
            match serde_json::to_string_pretty(&resp) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("error: serialise ACL response: {e}");
                    return cli_exit::EXIT_IO;
                }
            }
            cli_exit::EXIT_OK
        }
        Err(e) => {
            eprintln!("error: get_bucket_acl failed: {}", display_sdk_err(&e));
            cli_exit::EXIT_HTTP
        }
    }
}

pub async fn put_run(args: PutArgs) -> i32 {
    let bucket = match validate_bucket_only(&args.url) {
        Ok(b) => b,
        Err(code) => return code,
    };

    // Validate: at least one of --acl or --grant-* must be supplied.
    let has_grant = args.grant_full_control.is_some()
        || args.grant_read.is_some()
        || args.grant_read_acp.is_some()
        || args.grant_write.is_some()
        || args.grant_write_acp.is_some();
    if args.acl.is_none() && !has_grant {
        eprintln!("error: at least one of --acl or --grant-* must be supplied");
        return cli_exit::EXIT_USAGE;
    }

    let canned = match args.acl.as_deref().map(parse_canned_acl) {
        Some(Ok(c)) => Some(c),
        Some(Err(msg)) => {
            eprintln!("error: {msg}");
            return cli_exit::EXIT_USAGE;
        }
        None => None,
    };

    let client = match build_client(
        args.access_key_id.as_deref(),
        args.secret_access_key.as_deref(),
        args.region.as_deref(),
        args.profile.as_deref(),
        args.endpoint_url.clone(),
        args.force_path_style,
    )
    .await
    {
        Ok(c) => c,
        Err(code) => return code,
    };

    let mut req = client.put_bucket_acl().bucket(&bucket);
    if let Some(c) = canned {
        req = req.acl(c);
    }
    if let Some(g) = args.grant_full_control.as_deref() {
        req = req.grant_full_control(g);
    }
    if let Some(g) = args.grant_read.as_deref() {
        req = req.grant_read(g);
    }
    if let Some(g) = args.grant_read_acp.as_deref() {
        req = req.grant_read_acp(g);
    }
    if let Some(g) = args.grant_write.as_deref() {
        req = req.grant_write(g);
    }
    if let Some(g) = args.grant_write_acp.as_deref() {
        req = req.grant_write_acp(g);
    }

    match req.send().await {
        Ok(_) => {
            println!("OK: ACL updated on s3://{bucket}");
            cli_exit::EXIT_OK
        }
        Err(e) => {
            eprintln!("error: put_bucket_acl failed: {}", display_sdk_err(&e));
            cli_exit::EXIT_HTTP
        }
    }
}

/// Pure: parse one of the four AWS canned-ACL strings.
fn parse_canned_acl(s: &str) -> Result<BucketCannedAcl, String> {
    match s {
        "private" => Ok(BucketCannedAcl::Private),
        "public-read" => Ok(BucketCannedAcl::PublicRead),
        "public-read-write" => Ok(BucketCannedAcl::PublicReadWrite),
        "authenticated-read" => Ok(BucketCannedAcl::AuthenticatedRead),
        other => Err(format!(
            "unknown canned ACL `{other}` (expected one of: \
             private, public-read, public-read-write, authenticated-read)"
        )),
    }
}

fn validate_bucket_only(url: &str) -> Result<String, i32> {
    if !is_s3_url(url) {
        eprintln!("error: expected an `s3://bucket` URL, got `{url}`");
        return Err(cli_exit::EXIT_USAGE);
    }
    let loc = parse_s3_url(url).map_err(|e| {
        eprintln!("error: bad S3 URL: {e}");
        cli_exit::EXIT_PARSE
    })?;
    if !loc.key.is_empty() {
        eprintln!(
            "error: bucket-acl is bucket-scoped (no key); got s3://{}/{}",
            loc.bucket, loc.key
        );
        return Err(cli_exit::EXIT_USAGE);
    }
    Ok(loc.bucket)
}

async fn build_client(
    access_key: Option<&str>,
    secret_key: Option<&str>,
    region_flag: Option<&str>,
    profile_flag: Option<&str>,
    endpoint: Option<String>,
    force_path_style: bool,
) -> Result<aws_sdk_s3::Client, i32> {
    let creds = aws_creds::resolve(aws_creds::CredsInputs {
        access_key_flag: access_key,
        secret_key_flag: secret_key,
        region_flag,
        profile_flag,
        ..Default::default()
    })
    .map_err(|e| {
        eprintln!("error: {e}");
        cli_exit::EXIT_AUTH
    })?;

    if should_allow_local(endpoint.as_deref()) {
        // SAFETY: see engine_factory.rs for the same one-shot CLI
        // setup pattern. We're the only writer; readers haven't been
        // spawned yet.
        unsafe {
            std::env::set_var("DGP_BACKEND_ALLOW_LOCAL", "true");
        }
    }

    let backend = BackendConfig::S3 {
        endpoint,
        region: creds.region.unwrap_or_else(|| "us-east-1".into()),
        force_path_style,
        access_key_id: Some(creds.access_key_id),
        secret_access_key: Some(creds.secret_access_key),
    };
    S3Backend::build_client(&backend).await.map_err(|e| {
        eprintln!("error: failed to initialise S3 client: {e}");
        cli_exit::EXIT_HTTP
    })
}

fn display_sdk_err<E: std::error::Error>(e: &E) -> String {
    // SDK errors stringify nicely via `Display`; keep the formatter
    // simple so the user gets the raw cause without us trying to
    // re-format every SDK error variant.
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        s.push_str(": ");
        s.push_str(&inner.to_string());
        src = inner.source();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canned_acl_parsing_covers_all_four_variants() {
        assert!(matches!(
            parse_canned_acl("private"),
            Ok(BucketCannedAcl::Private)
        ));
        assert!(matches!(
            parse_canned_acl("public-read"),
            Ok(BucketCannedAcl::PublicRead)
        ));
        assert!(matches!(
            parse_canned_acl("public-read-write"),
            Ok(BucketCannedAcl::PublicReadWrite)
        ));
        assert!(matches!(
            parse_canned_acl("authenticated-read"),
            Ok(BucketCannedAcl::AuthenticatedRead)
        ));
    }

    #[test]
    fn canned_acl_rejects_unknown() {
        let err = parse_canned_acl("everyone-can-read").unwrap_err();
        assert!(err.contains("unknown canned ACL"));
        assert!(err.contains("private"));
    }

    #[test]
    fn bucket_only_url_rejects_key() {
        // Bucket name must be 3+ chars per S3 rules — use a real one.
        let r = validate_bucket_only("s3://my-bucket/k").unwrap_err();
        assert_eq!(r, cli_exit::EXIT_USAGE);
    }

    #[test]
    fn bucket_only_url_rejects_non_s3() {
        let r = validate_bucket_only("https://example.com/b").unwrap_err();
        assert_eq!(r, cli_exit::EXIT_USAGE);
    }

    #[test]
    fn bucket_only_url_accepts_bare_bucket() {
        assert_eq!(validate_bucket_only("s3://my-bucket").unwrap(), "my-bucket");
    }
}
