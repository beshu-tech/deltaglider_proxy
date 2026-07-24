//! The reconcile loop: server-side apply the desired children, then report status.

use crate::crd::DeltaGliderProxy;
use crate::resources;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{ConfigMap, Service};
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

pub async fn reconcile(cr: Arc<DeltaGliderProxy>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = cr.namespace().ok_or(Error::NoNamespace)?;
    let client = &ctx.client;

    if let Some(cm) = resources::config_configmap(&cr) {
        apply::<ConfigMap>(client, &ns, &cm).await?;
    }
    apply::<ConfigMap>(client, &ns, &resources::router_configmap(&cr)).await?;
    apply::<Service>(client, &ns, &resources::headless_service(&cr)).await?;
    apply::<Service>(client, &ns, &resources::entry_service(&cr)).await?;
    apply::<StatefulSet>(client, &ns, &resources::proxy_statefulset(&cr)).await?;
    apply::<Deployment>(client, &ns, &resources::router_deployment(&cr)).await?;
    apply::<PodDisruptionBudget>(client, &ns, &resources::router_pdb(&cr)).await?;

    // Status from the children's own status stanzas.
    let name = resources::cr_name(&cr);
    let sts: Api<StatefulSet> = Api::namespaced(client.clone(), &ns);
    let dep: Api<Deployment> = Api::namespaced(client.clone(), &ns);
    let ready = sts
        .get_status(&name)
        .await
        .ok()
        .and_then(|s| s.status)
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);
    let router_ready = dep
        .get_status(&format!("{name}-router"))
        .await
        .ok()
        .and_then(|d| d.status)
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);
    let want = cr.replicas();
    let phase = if ready >= want && router_ready >= 1 {
        "Ready"
    } else {
        "Progressing"
    };
    let status = json!({
        "apiVersion": "deltaglider.beshu.tech/v1alpha1",
        "kind": "DeltaGliderProxy",
        "status": {
            "observedGeneration": cr.meta().generation,
            "readyReplicas": ready,
            "routerReadyReplicas": router_ready,
            "phase": phase,
            "message": format!("{ready}/{want} proxy pods, {router_ready} router pods ready"),
        }
    });
    let api: Api<DeltaGliderProxy> = Api::namespaced(client.clone(), &ns);
    api.patch_status(&name, &apply_params(), &Patch::Apply(&status))
        .await?;

    tracing::info!(cr = %name, %ns, ready, router_ready, phase, "reconciled");
    Ok(Action::requeue(Duration::from_secs(300)))
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
