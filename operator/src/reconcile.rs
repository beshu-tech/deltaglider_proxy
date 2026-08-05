// SPDX-License-Identifier: BUSL-1.1

//! The reconcile loop: server-side apply the desired children, then report status.

use crate::crd::DeltaGliderProxy;
use crate::preflight::{effective_replicas, multi_replica_problems, phase_and_message, EnvSecret};
use crate::resources;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, Client, Resource, ResourceExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("kube API error: {0}")]
    Kube(#[from] kube::Error),
    #[error("CR has no namespace")]
    NoNamespace,
}

pub struct Ctx {
    pub client: Client,
}

fn apply_params() -> PatchParams {
    PatchParams::apply(resources::MANAGER).force()
}

async fn apply<K>(client: &Client, ns: &str, obj: &Value) -> Result<(), Error>
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug,
    K::DynamicType: Default,
{
    let name = obj["metadata"]["name"]
        .as_str()
        .expect("builders always set metadata.name")
        .to_string();
    let api: Api<K> = Api::namespaced(client.clone(), ns);
    api.patch(&name, &apply_params(), &Patch::Apply(obj))
        .await?;
    Ok(())
}

/// Observe spec.envFromSecret in the cluster (key names only, never values).
async fn observe_env_secret(client: &Client, ns: &str, cr: &DeltaGliderProxy) -> EnvSecret {
    let Some(name) = &cr.spec.env_from_secret else {
        return EnvSecret::NotNamed;
    };
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    match api.get_opt(name).await {
        Ok(Some(secret)) => {
            let mut keys: Vec<String> = secret
                .data
                .map(|d| d.keys().cloned().collect())
                .unwrap_or_default();
            keys.extend(
                secret
                    .string_data
                    .map(|d| d.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default(),
            );
            EnvSecret::Keys(keys)
        }
        Ok(None) => EnvSecret::Missing(name.clone()),
        // Transient read failure: the truth is unknown — never guess in either direction.
        Err(e) => {
            tracing::warn!(secret = %name, error = %e, "env Secret unreadable this cycle");
            EnvSecret::Unknown
        }
    }
}

/// Create `<name>-bootstrap` once if autoGenerate is on. Never overwrites.
async fn ensure_bootstrap_secret(
    client: &Client,
    ns: &str,
    cr: &DeltaGliderProxy,
) -> Result<(), Error> {
    if !cr.bootstrap_auto_generate() {
        return Ok(());
    }
    let name = format!("{}-bootstrap", resources::cr_name(cr));
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    if api.get_opt(&name).await?.is_some() {
        return Ok(());
    }
    use rand::distributions::{Alphanumeric, DistString};
    let password = Alphanumeric.sample_string(&mut rand::rngs::OsRng, 32);
    let hash = bcrypt::hash(&password, 12).expect("bcrypt with fixed cost cannot fail");
    let hash_b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(hash)
    };
    let obj = resources::bootstrap_secret(cr, &password, &hash_b64);
    let secret: Secret = serde_json::from_value(obj).expect("builder output is a valid Secret");
    match api.create(&Default::default(), &secret).await {
        Ok(_) => {
            tracing::info!(%name, %ns, "generated bootstrap password Secret");
            Ok(())
        }
        // A concurrent reconcile won the race — fine, it exists.
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub async fn reconcile(cr: Arc<DeltaGliderProxy>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = cr.namespace().ok_or(Error::NoNamespace)?;
    let client = &ctx.client;

    ensure_bootstrap_secret(client, &ns, &cr).await?;

    // Multi-replica preflight: contract violations block scale-UP (fresh deploys come
    // up at 1) but never scale DOWN a running fleet — that would kill healthy pods.
    let name = resources::cr_name(&cr);
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), &ns);
    let current = sts_api
        .get_opt(&name)
        .await?
        .and_then(|s| s.spec)
        .and_then(|s| s.replicas);
    let env_secret = observe_env_secret(client, &ns, &cr).await;
    let unknown = env_secret == EnvSecret::Unknown;
    let problems = if unknown {
        vec![]
    } else {
        multi_replica_problems(&cr, &env_secret)
    };
    let may_scale_up = !unknown && problems.is_empty();
    let replicas = effective_replicas(cr.replicas(), may_scale_up, current);

    if let Some(cm) = resources::config_configmap(&cr) {
        apply::<ConfigMap>(client, &ns, &cm).await?;
    }
    apply::<ConfigMap>(client, &ns, &resources::router_configmap(&cr, replicas)).await?;
    apply::<Service>(client, &ns, &resources::headless_service(&cr)).await?;
    apply::<Service>(client, &ns, &resources::entry_service(&cr)).await?;
    apply::<StatefulSet>(client, &ns, &resources::proxy_statefulset(&cr, replicas)).await?;
    apply::<Deployment>(client, &ns, &resources::router_deployment(&cr, replicas)).await?;
    apply::<PodDisruptionBudget>(client, &ns, &resources::router_pdb(&cr)).await?;

    // Status from the children's own status stanzas.
    let dep: Api<Deployment> = Api::namespaced(client.clone(), &ns);
    let ready = sts_api
        .get_opt(&name)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.status)
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);
    let router_ready = dep
        .get_opt(&format!("{name}-router"))
        .await
        .ok()
        .flatten()
        .and_then(|d| d.status)
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);
    let (phase, message) = phase_and_message(&problems, ready, replicas, router_ready);
    let status = json!({
        "apiVersion": "deltaglider.beshu.tech/v1alpha1",
        "kind": "DeltaGliderProxy",
        "status": {
            "observedGeneration": cr.meta().generation,
            "readyReplicas": ready,
            "routerReadyReplicas": router_ready,
            "phase": phase,
            "message": message,
        }
    });
    let api: Api<DeltaGliderProxy> = Api::namespaced(client.clone(), &ns);
    api.patch_status(&name, &apply_params(), &Patch::Apply(&status))
        .await?;

    tracing::info!(cr = %name, %ns, ready, router_ready, phase, "reconciled");
    // An unreadable env Secret held the fleet in place — re-check soon, not in 5 min.
    let requeue = if unknown { 30 } else { 300 };
    Ok(Action::requeue(Duration::from_secs(requeue)))
}

fn error_policy(cr: Arc<DeltaGliderProxy>, err: &Error, _ctx: Arc<Ctx>) -> Action {
    tracing::warn!(cr = %cr.name_any(), error = %err, "reconcile failed, requeueing");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run(client: Client) -> anyhow::Result<()> {
    let crs: Api<DeltaGliderProxy> = Api::all(client.clone());
    let ctx = Arc::new(Ctx {
        client: client.clone(),
    });
    Controller::new(crs, watcher::Config::default())
        .owns(
            Api::<StatefulSet>::all(client.clone()),
            watcher::Config::default(),
        )
        .owns(
            Api::<Deployment>::all(client.clone()),
            watcher::Config::default(),
        )
        .owns(
            Api::<Service>::all(client.clone()),
            watcher::Config::default(),
        )
        .owns(
            Api::<ConfigMap>::all(client.clone()),
            watcher::Config::default(),
        )
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::warn!(error = %e, "controller event error");
            }
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod bootstrap_hash_tests {
    //! Regression guard for the operator↔proxy bootstrap-hash contract.
    //! The operator emits `base64(bcrypt::hash(pw, 12))` into the Secret key
    //! `hash`; the proxy's `Config::decode_hash` base64-decodes any non-`$`
    //! value and expects a `$2b$…` bcrypt string. If either side's encoding
    //! drifts, autoGenerate ships a bricked login — this test fails first.
    use base64::Engine;

    /// Reproduce the operator's exact recipe (see `ensure_bootstrap_secret`).
    fn operator_hash_b64(password: &str) -> String {
        let hash = bcrypt::hash(password, 12).expect("bcrypt with fixed cost cannot fail");
        base64::engine::general_purpose::STANDARD.encode(hash)
    }

    #[test]
    fn hash_b64_decodes_to_verifiable_bcrypt() {
        let password = "correct horse battery staple";
        let b64 = operator_hash_b64(password);

        // The proxy: non-`$` value → base64 decode → must be UTF-8 bcrypt.
        let decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .expect("operator output must be valid base64");
        let decoded = String::from_utf8(decoded_bytes).expect("bcrypt hash is UTF-8");

        // Proxy's decode_hash gate: must start with `$2` (accepts $2a/$2b/$2y).
        assert!(
            decoded.starts_with("$2"),
            "decoded hash must be a bcrypt string, got {decoded:?}"
        );
        // The rust bcrypt crate emits the $2b$ variant at cost 12.
        assert!(
            decoded.starts_with("$2b$12$"),
            "expected $2b$ cost-12, got {decoded:?}"
        );

        // The whole point: the plaintext verifies against the decoded hash.
        assert!(
            bcrypt::verify(password, &decoded).expect("verify runs"),
            "plaintext must verify against the operator-generated hash"
        );

        // Emit a specimen for the manual end-to-end boot test.
        eprintln!("SPECIMEN_PASSWORD={password}");
        eprintln!("SPECIMEN_HASH_B64={b64}");
    }
}
