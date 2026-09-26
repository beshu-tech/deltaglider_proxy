// SPDX-License-Identifier: BUSL-1.1

//! The two s3s hooks the S3 router installs: the secret-key lookup s3s
//! verifies signatures with, and the access check that binds the identity
//! our SigV4 middleware resolved to the key s3s verified.
//!
//! They live in the library (not `startup.rs`) so the middleware-vs-s3s
//! contract test drives the same hooks as production.

use s3s::access::{S3Access, S3AccessContext};
use s3s::auth::{S3Auth, SecretKey};

use crate::iam::{IamState, SharedIamState};

/// s3s secret-key provider backed by the live IAM state.
#[derive(Clone)]
pub struct DeltaGliderS3sAuth {
    pub iam_state: SharedIamState,
}

#[async_trait::async_trait]
impl S3Auth for DeltaGliderS3sAuth {
    async fn get_secret_key(&self, access_key: &str) -> s3s::S3Result<SecretKey> {
        match self.iam_state.load().as_ref() {
            IamState::Disabled => {
                // The legacy Axum path ignores signatures in open-dev mode.
                // In open mode, accept the common "same access key + secret"
                // dummy pattern used by SDK clients (test/test, anonymous/
                // anonymous). This lets s3s decode signed/chunked SDK
                // requests without making local-dev users discover a magic
                // hardcoded secret.
                Ok(SecretKey::from(access_key.to_string()))
            }
            IamState::Legacy(auth) if access_key == auth.access_key_id => {
                Ok(SecretKey::from(auth.secret_access_key.clone()))
            }
            IamState::Iam(index) => index
                .get(access_key)
                .filter(|user| user.enabled)
                .map(|user| SecretKey::from(user.secret_access_key.clone()))
                .ok_or_else(|| s3s::s3_error!(InvalidAccessKeyId)),
            _ => Err(s3s::s3_error!(InvalidAccessKeyId)),
        }
    }
}

/// s3s access hook: refuses a request unless s3s verified the identity the
/// SigV4 middleware resolved, then runs the post-verification gates.
#[derive(Clone)]
pub struct VerifiedIdentityS3sAccess;

#[async_trait::async_trait]
impl S3Access for VerifiedIdentityS3sAccess {
    async fn check(&self, cx: &mut S3AccessContext<'_>) -> s3s::S3Result<()> {
        // IAM/admission authorization is enforced by the outer Axum
        // middleware chain, against the identity the SigV4 middleware
        // resolved. This hook binds that identity to the key s3s verified
        // (see `resolved_identity_is_verified`). It also replaces s3s'
        // default "auth provider implies anonymous deny", which would
        // reject already-admitted public/open-mode requests.
        let verified = cx.credentials().map(|c| c.access_key.clone());
        let resolved = cx.extensions_mut().get::<crate::iam::AuthenticatedUser>();
        if crate::api::auth::resolved_identity_is_verified(resolved, verified.as_deref()) {
            // Only a real verified credential counts as an auth success
            // for the brute-force limiter; anonymous/open requests don't.
            if verified.is_some() {
                if let Some(outcome) = cx.extensions_mut().get::<crate::api::auth::AuthOutcome>() {
                    outcome.mark_verified();
                }
            }
            // The maintenance + backend-health gates run HERE, after s3s
            // verified the signature: their 503 names a busy bucket or a
            // backend's state, which a forged signature must not learn.
            let (method, path) = (cx.method().clone(), cx.uri().path().to_owned());
            crate::maintenance::gate::check_verified_request(cx.extensions_mut(), &method, &path)
                .map_err(|e| {
                    let code = s3s::S3ErrorCode::from_bytes(e.code().as_bytes())
                        .unwrap_or(s3s::S3ErrorCode::ServiceUnavailable);
                    s3s::S3Error::with_message(code, e.to_string())
                })
        } else {
            tracing::warn!(
                "SECURITY | event=identity_mismatch | resolved={} | verified={}",
                resolved.map(|u| u.access_key_id.as_str()).unwrap_or(""),
                verified.as_deref().unwrap_or("<none>")
            );
            Err(s3s::s3_error!(AccessDenied))
        }
    }
}

/// The s3s configuration the S3 router runs with.
pub fn s3s_config() -> std::sync::Arc<s3s::config::StaticConfigProvider> {
    // Pass the documented skew tolerance to s3s; it used its own default.
    let mut config = s3s::config::S3Config::default();
    config.presigned_url_max_skew_time_secs = crate::api::auth::clock_skew_secs();
    std::sync::Arc::new(s3s::config::StaticConfigProvider::new(std::sync::Arc::new(
        config,
    )))
}
