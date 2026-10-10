// SPDX-License-Identifier: BUSL-1.1

//! S3 client construction: the SSRF guard on custom endpoints and the
//! request deadlines.

use super::*;

/// The outbound-URL check of [`guard_s3_endpoint`], without a client: the
/// admin API runs it on a new backend so a refused endpoint is a 400, not a
/// failed engine rebuild (500). Returns the URL kind the resolver enforces.
pub(crate) fn check_s3_endpoint(
    ep: &str,
    allow_local: bool,
) -> Result<crate::security::UrlKind, String> {
    let env_allow = crate::config::env_bool("DGP_BACKEND_ALLOW_LOCAL", false);
    let kind = if allow_local || env_allow {
        crate::security::UrlKind::BackendDev
    } else {
        crate::security::UrlKind::Backend
    };
    crate::security::validate_outbound_url(ep, kind).map_err(|e| {
        format!(
            "Refusing to use S3 endpoint {ep:?}: {e}. \
             Set `allow_local: true` in the backend config (or \
             DGP_BACKEND_ALLOW_LOCAL=true env) to permit http:// + \
             private IPs for dev/CI."
        )
    })?;
    Ok(kind)
}

/// Point `builder` at the operator-supplied endpoint `ep`, SSRF-guarded:
/// THE one place an S3 client gets a custom endpoint (engine backends,
/// config sync, S3 leases, the reference lock, health and capability
/// probes). A source test refuses a direct `endpoint_url` call elsewhere.
///
/// Rejects endpoints that point at cloud instance-metadata services,
/// RFC1918 / loopback / link-local ranges, or other internal hosts.
/// Without this the S3 client becomes an SSRF pivot: a compromised admin
/// can swap the endpoint to http://169.254.169.254 and the proxy will
/// faithfully relay signed requests against IMDS.
///
/// `BackendDev` keeps the door open for local MinIO so dev/CI deployments
/// still work — opted into via either the typed `BackendConfig::S3.allow_local`
/// field (the preferred path) or the legacy `DGP_BACKEND_ALLOW_LOCAL=true`
/// env var. A hardened production env must keep both off.
#[allow(
    clippy::disallowed_methods,
    reason = "THE endpoint setter: runs the SSRF check first"
)]
pub(crate) fn guard_s3_endpoint(
    builder: aws_sdk_s3::config::Builder,
    ep: &str,
    allow_local: bool,
) -> Result<aws_sdk_s3::config::Builder, String> {
    let kind = check_s3_endpoint(ep, allow_local)?;
    let mut builder = builder.endpoint_url(ep);
    // The text check above cannot see DNS: a name whose record points at
    // IMDS (or rebinds there) passed. Private answers stay allowed: on-prem
    // storage behind internal DNS is normal.
    if let Some(host) = reqwest::Url::parse(ep)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    {
        builder = builder.http_client(ssrf_guarded_http_client(
            crate::security::SdkSsrfGuardedResolver::new(kind, host.trim_matches(['[', ']'])),
        ));
    }
    Ok(builder)
}

/// The SDK's default HTTPS client (hyper 1 + rustls/aws-lc, env proxy
/// config, SDK connector settings), with an SSRF-guarded DNS resolver.
/// Mirrors `aws_smithy_runtime::client::http::default_https_client`.
pub(super) fn ssrf_guarded_http_client(
    resolver: crate::security::SdkSsrfGuardedResolver,
) -> aws_smithy_runtime_api::client::http::SharedHttpClient {
    use aws_smithy_http_client::{proxy::ProxyConfig, tls, Builder, ConnectorBuilder};
    Builder::new().build_with_connector_fn(move |settings, runtime_components| {
        let mut conn = ConnectorBuilder::default().tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::AwsLc,
        ));
        conn.set_connector_settings(settings.cloned());
        if let Some(rc) = runtime_components {
            conn.set_sleep_impl(rc.sleep_impl());
        }
        conn.set_proxy_config(Some(ProxyConfig::from_env()));
        conn.build_with_resolver(resolver.clone())
    })
}

/// `DGP_BACKEND_REQUEST_TIMEOUT_SECS` (default 30; 0 turns it off): the
/// deadline for one backend request without a large body, retries included.
/// A hung backend then costs a request this long, not minutes.
pub(crate) fn backend_request_timeout() -> Option<std::time::Duration> {
    match crate::config::env_parse_with_default("DGP_BACKEND_REQUEST_TIMEOUT_SECS", 30u64) {
        0 => None,
        secs => Some(std::time::Duration::from_secs(secs)),
    }
}

/// The slowest body rate that an upload's deadline allows for.
const UPLOAD_FLOOR_BYTES_PER_SEC: u64 = 1024 * 1024;

/// Pure: the whole-operation deadline (all SDK attempts included) of an
/// upload of `bytes`: the request deadline `base`, plus one second per MiB
/// of body (`UPLOAD_FLOOR_BYTES_PER_SEC`). `None` (the request deadline is
/// off) leaves uploads without one.
pub(super) fn upload_deadline(
    base: Option<std::time::Duration>,
    bytes: u64,
) -> Option<std::time::Duration> {
    base.map(|b| {
        b.saturating_add(std::time::Duration::from_secs(
            bytes.div_ceil(UPLOAD_FLOOR_BYTES_PER_SEC),
        ))
    })
}

/// A retry partition of its own for one backend definition. The SDK's
/// default partition is process-wide per region (`s3-<region>`): its retry
/// token bucket and its adaptive rate limiter were shared by every backend
/// in the region, so SlowDowns from one backend slowed the others and
/// emptied their retry quota. A custom partition owns its token bucket and
/// rate limiter; the two clients of one backend share it (a clone). Named
/// by a hash of the definition (the definition holds the secret).
pub(super) fn backend_retry_partition(
    config: &BackendConfig,
) -> aws_sdk_s3::config::retry::RetryPartition {
    use sha2::Digest;
    let fp = crate::coordination::capability::fingerprint(config);
    let hash = hex::encode(sha2::Sha256::digest(fp.as_bytes()));
    aws_sdk_s3::config::retry::RetryPartition::custom(format!("dgp-backend-{}", &hash[..12]))
        .build()
}

impl S3Backend {
    /// `DGP_BACKEND_REQUEST_TIMEOUT_SECS` as a duration; `None` when off.
    /// The deadline of one backend request without a large body.
    pub fn request_timeout() -> Option<std::time::Duration> {
        backend_request_timeout()
    }

    /// Build an S3 client from a BackendConfig without creating an S3Backend.
    /// Useful for one-off operations like testing connectivity.
    pub async fn build_client(config: &BackendConfig) -> Result<Client, StorageError> {
        Self::build_client_with(config, None).await
    }

    /// [`Self::build_client`] with a whole-operation deadline (all retries
    /// included) on top of the per-attempt timeouts.
    pub(super) async fn build_client_with(
        config: &BackendConfig,
        operation_timeout: Option<std::time::Duration>,
    ) -> Result<Client, StorageError> {
        Self::build_client_in(config, operation_timeout, backend_retry_partition(config)).await
    }

    /// [`Self::build_client_with`] in the retry partition `partition`.
    pub(super) async fn build_client_in(
        config: &BackendConfig,
        operation_timeout: Option<std::time::Duration>,
        partition: aws_sdk_s3::config::retry::RetryPartition,
    ) -> Result<Client, StorageError> {
        let (
            endpoint,
            region,
            force_path_style,
            access_key_id,
            secret_access_key,
            allow_local,
            session_token,
        ) = match config {
            BackendConfig::S3 {
                endpoint,
                region,
                force_path_style,
                access_key_id,
                secret_access_key,
                allow_local,
                session_token,
            } => (
                endpoint.clone(),
                region.clone(),
                *force_path_style,
                access_key_id.clone(),
                secret_access_key.clone(),
                *allow_local,
                session_token.clone(),
            ),
            _ => {
                return Err(StorageError::Other(
                    "S3Backend requires S3 configuration".to_string(),
                ))
            }
        };

        // Require explicit credentials — never fall back to the default AWS credential chain
        // (env vars, ~/.aws/credentials, instance metadata, etc.)
        let credentials = match (access_key_id, secret_access_key) {
            (Some(ref key_id), Some(ref secret)) => Credentials::new(
                key_id,
                secret,
                session_token,
                None,
                "deltaglider_proxy-config",
            ),
            _ => {
                return Err(StorageError::Other(
                    "S3 backend requires explicit credentials: set DGP_BE_AWS_ACCESS_KEY_ID and DGP_BE_AWS_SECRET_ACCESS_KEY".to_string(),
                ));
            }
        };

        // Build S3 client directly — no aws-config needed since we use static credentials.
        // Disable automatic request checksums (CRC32/CRC64) added by the SDK by default.
        // S3-compatible stores (Hetzner, MinIO, Backblaze B2) reject these headers with
        // BadRequest. Setting WhenRequired preserves compatibility with both AWS S3 and
        // S3-compatible endpoints. See: Python deltaglider [6.1.1] for the equivalent fix.
        // Per-attempt + read/connect timeouts so a stalled socket fails
        // fast (per multipart part) instead of hanging the whole copy
        // until lease lapse. Phase B streaming relies on these to bound a
        // mid-part GET/PUT. All env-overridable in seconds.
        let read_timeout = crate::config::env_parse_with_default("DGP_S3_READ_TIMEOUT_SECS", 60u64);
        let connect_timeout =
            crate::config::env_parse_with_default("DGP_S3_CONNECT_TIMEOUT_SECS", 10u64);
        let attempt_timeout =
            crate::config::env_parse_with_default("DGP_S3_OPERATION_ATTEMPT_TIMEOUT_SECS", 300u64);
        let stall_grace = crate::config::env_parse_with_default("DGP_S3_STALL_GRACE_SECS", 20u64);
        let mut timeout_config = aws_sdk_s3::config::timeout::TimeoutConfig::builder()
            .read_timeout(std::time::Duration::from_secs(read_timeout))
            .connect_timeout(std::time::Duration::from_secs(connect_timeout))
            .operation_attempt_timeout(std::time::Duration::from_secs(attempt_timeout));
        if let Some(t) = operation_timeout {
            timeout_config = timeout_config.operation_timeout(t);
        }
        let timeout_config = timeout_config.build();
        let stalled_stream_protection =
            aws_sdk_s3::config::StalledStreamProtectionConfig::enabled()
                .grace_period(std::time::Duration::from_secs(stall_grace))
                .build();

        let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region))
            .credentials_provider(credentials)
            .force_path_style(force_path_style)
            .timeout_config(timeout_config)
            // Adaptive retry = the SDK's rate limiter: a 503 SlowDown from
            // the backend throttles every later request in the partition
            // (HEAD bursts, listings, replication reads) instead of each
            // call site hammering a struggling backend. THE retry layer for
            // throttles, 5xx and timeouts: the application loops retry only
            // what the SDK does not (`objects::is_unidentified_400`). The
            // partition is this backend's own (`backend_retry_partition`).
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::adaptive())
            .retry_partition(partition)
            .stalled_stream_protection(stalled_stream_protection)
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .response_checksum_validation(
                aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired,
            );

        if let Some(ref ep) = endpoint {
            s3_config_builder = guard_s3_endpoint(s3_config_builder, ep, allow_local)
                .map_err(StorageError::Other)?;
        }

        Ok(Client::from_conf(s3_config_builder.build()))
    }
}
