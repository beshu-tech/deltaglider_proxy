// SPDX-License-Identifier: GPL-3.0-only

//! The DeltaGliderProxy custom resource definition.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default proxy image when `spec.image` is not set.
pub const DEFAULT_IMAGE: &str = "beshultd/deltaglider_proxy:1.16.0";
/// Default router image when `spec.router.image` is not set.
pub const DEFAULT_ROUTER_IMAGE: &str = "haproxy:3.0-alpine";
/// The single port everything listens on (S3 API + admin UI).
pub const PROXY_PORT: i32 = 9000;

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "deltaglider.beshu.tech",
    version = "v1alpha1",
    kind = "DeltaGliderProxy",
    namespaced,
    status = "DeltaGliderProxyStatus",
    shortname = "dgp",
    printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".spec.replicas"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.phase"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct DeltaGliderProxySpec {
    /// Number of proxy pods. Above 1, read the HA notes in the operator README first:
    /// you need an S3 storage backend, a shared bootstrap password hash, and a config
    /// sync bucket. Multipart uploads work because the managed router consistently
    /// hashes requests by URL path, pinning each object's requests to one pod.
    pub replicas: Option<i32>,
    /// Proxy container image. Defaults to the operator's pinned release.
    pub image: Option<String>,
    /// Inline DeltaGlider YAML config. Rendered to a ConfigMap and mounted at
    /// /data/deltaglider_proxy.yaml so the encrypted IAM DB lands on the writable
    /// volume next to it. Secrets belong in `envFromSecret`, not here.
    pub config_yaml: Option<String>,
    /// Name of an existing Secret whose keys are injected as environment variables
    /// (DGP_ACCESS_KEY_ID, DGP_SECRET_ACCESS_KEY, DGP_BOOTSTRAP_PASSWORD_HASH,
    /// DGP_BE_AWS_* backend credentials, ...).
    pub env_from_secret: Option<String>,
    /// Per-pod persistent volume for /data (config DB, filesystem-backend objects).
    pub storage: Option<StorageSpec>,
    /// The consistent-hashing HAProxy router in front of the proxy pods.
    pub router: Option<RouterSpec>,
    /// How the router is exposed.
    pub service: Option<ServiceSpec>,
    /// Proxy container resources.
    pub resources: Option<ResourcesSpec>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    /// PVC size per pod (default "10Gi").
    pub size: Option<String>,
    /// StorageClass name (default: cluster default).
    pub storage_class: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct RouterSpec {
    /// Router pod count (default 2). Routers are stateless; the hash ring is
    /// deterministic, so every router maps a given path to the same proxy pod.
    pub replicas: Option<i32>,
    /// Router image (default haproxy:3.0-alpine).
    pub image: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSpec {
    /// Service type for the entrypoint: ClusterIP (default), NodePort, LoadBalancer.
    pub r#type: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesSpec {
    pub requests: Option<ResourceAmounts>,
    pub limits: Option<ResourceAmounts>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResourceAmounts {
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeltaGliderProxyStatus {
    pub observed_generation: Option<i64>,
    pub ready_replicas: Option<i32>,
    pub router_ready_replicas: Option<i32>,
    /// Ready | Progressing
    pub phase: Option<String>,
    pub message: Option<String>,
}

impl DeltaGliderProxy {
    pub fn replicas(&self) -> i32 {
        self.spec.replicas.unwrap_or(1).max(0)
    }
    pub fn image(&self) -> String {
        self.spec
            .image
            .clone()
            .unwrap_or_else(|| DEFAULT_IMAGE.into())
    }
    pub fn router_replicas(&self) -> i32 {
        self.spec
            .router
            .as_ref()
            .and_then(|r| r.replicas)
            .unwrap_or(2)
            .max(1)
    }
    pub fn router_image(&self) -> String {
        self.spec
            .router
            .as_ref()
            .and_then(|r| r.image.clone())
            .unwrap_or_else(|| DEFAULT_ROUTER_IMAGE.into())
    }
    pub fn service_type(&self) -> String {
        self.spec
            .service
            .as_ref()
            .and_then(|s| s.r#type.clone())
            .unwrap_or_else(|| "ClusterIP".into())
    }
    pub fn storage_size(&self) -> String {
        self.spec
            .storage
            .as_ref()
            .and_then(|s| s.size.clone())
            .unwrap_or_else(|| "10Gi".into())
    }
}
