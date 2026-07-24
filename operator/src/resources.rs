//! Pure builders: CR → desired child objects as JSON values (apiVersion/kind included,
//! as server-side apply requires). Tests deserialize them into typed k8s structs.

use crate::crd::{DeltaGliderProxy, PROXY_PORT};
use kube::Resource;
use serde_json::{json, Value};

pub const MANAGER: &str = "deltaglider-operator";
pub const CONFIG_FILENAME: &str = "deltaglider_proxy.yaml";
pub const CONFIG_MOUNT: &str = "/data/deltaglider_proxy.yaml";

fn labels(cr_name: &str, component: &str) -> Value {
    json!({
        "app.kubernetes.io/name": "deltaglider-proxy",
        "app.kubernetes.io/instance": cr_name,
        "app.kubernetes.io/component": component,
        "app.kubernetes.io/managed-by": MANAGER,
    })
}

fn owner_ref(cr: &DeltaGliderProxy) -> Value {
    let oref = cr
        .controller_owner_ref(&())
        .expect("CR from the API server always has name+uid");
    serde_json::to_value(&oref).expect("owner ref serializes")
}

fn meta(cr: &DeltaGliderProxy, name: &str, component: &str) -> Value {
    json!({
        "name": name,
        "namespace": cr.meta().namespace,
        "labels": labels(cr.meta().name.as_deref().unwrap_or_default(), component),
        "ownerReferences": [owner_ref(cr)],
    })
}

pub fn cr_name(cr: &DeltaGliderProxy) -> String {
    cr.meta().name.clone().unwrap_or_default()
}

/// FNV-1a, inlined for stability: DefaultHasher may change across Rust releases,
/// which would spuriously roll every StatefulSet on an operator rebuild.
fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The HAProxy config implementing the multi-pod contract:
/// S3 traffic is consistent-hashed by URL path (bucket/key), so every request for a
/// given object — including all parts of a multipart upload — lands on the same proxy
/// pod. Admin UI traffic (/_/) is source-IP sticky because sessions are in-memory.
pub fn haproxy_cfg(cr_name: &str, namespace: &str, replicas: i32) -> String {
    let mut servers_s3 = String::new();
    let mut servers_admin = String::new();
    for i in 0..replicas {
        // StatefulSet stable DNS names keep the hash ring identical on every router.
        let host = format!("{cr_name}-{i}.{cr_name}-pods.{namespace}.svc.cluster.local");
        servers_s3.push_str(&format!(
            "  server dgp{i} {host}:{PROXY_PORT} check resolvers kube init-addr none\n"
        ));
        servers_admin.push_str(&format!(
            "  server dgp{i} {host}:{PROXY_PORT} check resolvers kube init-addr none\n"
        ));
    }
    format!(
        r#"global
  maxconn 4096
  log stdout format raw local0

defaults
  mode http
  log global
  option httplog
  option dontlognull
  option forwardfor
  timeout connect 5s
  timeout client 1h
  timeout server 1h
  timeout http-request 5m

resolvers kube
  parse-resolv-conf
  hold valid 10s

frontend dgp
  bind :{PROXY_PORT}
  acl is_admin path_beg /_/
  use_backend admin if is_admin
  default_backend s3

# Consistent hash by URI path only (query string and any HTTP/2 authority excluded):
# all requests for one object key — every multipart part included — pin to one pod.
backend s3
  balance uri path-only
  hash-type consistent
  option httpchk GET /_/ready
  http-check expect status 200
  default-server inter 10s rise 2 fall 3
{servers_s3}
# Admin UI sessions are in-memory and IP-bound: source-IP stickiness.
backend admin
  balance source
  hash-type consistent
  option httpchk GET /_/ready
  http-check expect status 200
  default-server inter 10s rise 2 fall 3
{servers_admin}"#
    )
}

/// ConfigMap holding the inline DeltaGlider YAML (only when spec.configYaml is set).
pub fn config_configmap(cr: &DeltaGliderProxy) -> Option<Value> {
    let yaml = cr.spec.config_yaml.as_ref()?;
    let name = format!("{}-config", cr_name(cr));
    Some(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": meta(cr, &name, "proxy"),
        "data": { CONFIG_FILENAME: yaml },
    }))
}

/// ConfigMap holding the rendered haproxy.cfg.
pub fn router_configmap(cr: &DeltaGliderProxy) -> Value {
    let name = format!("{}-router", cr_name(cr));
    let ns = cr.meta().namespace.clone().unwrap_or_default();
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": meta(cr, &name, "router"),
        "data": { "haproxy.cfg": haproxy_cfg(&cr_name(cr), &ns, cr.replicas()) },
    })
}

/// Headless service governing the proxy StatefulSet (stable per-pod DNS for the ring).
pub fn headless_service(cr: &DeltaGliderProxy) -> Value {
    let name = format!("{}-pods", cr_name(cr));
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": meta(cr, &name, "proxy"),
        "spec": {
            "clusterIP": "None",
            "selector": {
                "app.kubernetes.io/instance": cr_name(cr),
                "app.kubernetes.io/component": "proxy",
            },
            "ports": [{ "name": "http", "port": PROXY_PORT, "targetPort": PROXY_PORT }],
        },
    })
}

/// The entrypoint service, pointing at the router pods.
pub fn entry_service(cr: &DeltaGliderProxy) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": meta(cr, &cr_name(cr), "router"),
        "spec": {
            "type": cr.service_type(),
            "selector": {
                "app.kubernetes.io/instance": cr_name(cr),
                "app.kubernetes.io/component": "router",
            },
            "ports": [{ "name": "http", "port": PROXY_PORT, "targetPort": PROXY_PORT }],
        },
    })
}

/// The proxy StatefulSet: one PVC per pod, config mounted read-only at
/// /data/deltaglider_proxy.yaml over the writable volume (subPath), same hardening
/// as the Helm chart / Dockerfile.
pub fn proxy_statefulset(cr: &DeltaGliderProxy) -> Value {
    let name = cr_name(cr);
    let has_config = cr.spec.config_yaml.is_some();

    // The router is the only supported entrypoint and stamps X-Forwarded-For
    // (option forwardfor): trust it so rate limits, aws:SourceIp conditions, and
    // IP-bound admin sessions see the real client, not the router pod.
    let mut env = vec![json!({ "name": "DGP_TRUST_PROXY_HEADERS", "value": "true" })];
    if has_config {
        env.push(json!({ "name": "DGP_CONFIG", "value": CONFIG_MOUNT }));
    }

    let mut env_from = vec![];
    if let Some(secret) = &cr.spec.env_from_secret {
        env_from.push(json!({ "secretRef": { "name": secret } }));
    }

    let mut volume_mounts = vec![
        json!({ "name": "data", "mountPath": "/data" }),
        json!({ "name": "tmp", "mountPath": "/tmp" }),
    ];
    let mut volumes = vec![json!({ "name": "tmp", "emptyDir": {} })];
    if has_config {
        volume_mounts.push(json!({
            "name": "config",
            "mountPath": CONFIG_MOUNT,
            "subPath": CONFIG_FILENAME,
            "readOnly": true,
        }));
        volumes.push(json!({
            "name": "config",
            "configMap": { "name": format!("{name}-config") },
        }));
    }

    let mut resources = json!({});
    if let Some(r) = &cr.spec.resources {
        let amounts = |a: &Option<crate::crd::ResourceAmounts>| -> Value {
            let mut m = serde_json::Map::new();
            if let Some(a) = a {
                if let Some(cpu) = &a.cpu {
                    m.insert("cpu".into(), json!(cpu));
                }
                if let Some(mem) = &a.memory {
                    m.insert("memory".into(), json!(mem));
                }
            }
            Value::Object(m)
        };
        resources = json!({ "requests": amounts(&r.requests), "limits": amounts(&r.limits) });
    }

    let mut pvc_spec = json!({
        "accessModes": ["ReadWriteOnce"],
        "resources": { "requests": { "storage": cr.storage_size() } },
    });
    if let Some(class) = cr
        .spec
        .storage
        .as_ref()
        .and_then(|s| s.storage_class.clone())
    {
        pvc_spec["storageClassName"] = json!(class);
    }

    // subPath mounts never see ConfigMap updates and DGP reads config at boot, so a
    // config change must roll the pods: stamp its hash onto the pod template.
    let mut template_meta = json!({ "labels": labels(&name, "proxy") });
    if let Some(yaml) = &cr.spec.config_yaml {
        template_meta["annotations"] = json!({ "dgp.beshu.tech/config-hash": fnv1a_hex(yaml) });
    }

    json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": meta(cr, &name, "proxy"),
        "spec": {
            "replicas": cr.replicas(),
            "serviceName": format!("{name}-pods"),
            "podManagementPolicy": "Parallel",
            "selector": { "matchLabels": {
                "app.kubernetes.io/instance": name,
                "app.kubernetes.io/component": "proxy",
            }},
            "template": {
                "metadata": template_meta,
                "spec": {
                    "securityContext": {
                        "runAsNonRoot": true,
                        "runAsUser": 999,
                        "runAsGroup": 999,
                        "fsGroup": 999,
                    },
                    "automountServiceAccountToken": false,
                    "containers": [{
                        "name": "proxy",
                        "image": cr.image(),
                        "ports": [{ "name": "http", "containerPort": PROXY_PORT }],
                        "env": env,
                        "envFrom": env_from,
                        "resources": resources,
                        "securityContext": {
                            "readOnlyRootFilesystem": true,
                            "allowPrivilegeEscalation": false,
                            "capabilities": { "drop": ["ALL"] },
                        },
                        "livenessProbe": {
                            "httpGet": { "path": "/_/health", "port": PROXY_PORT },
                            "initialDelaySeconds": 5, "periodSeconds": 10,
                        },
                        "readinessProbe": {
                            "httpGet": { "path": "/_/ready", "port": PROXY_PORT },
                            "initialDelaySeconds": 5, "periodSeconds": 10,
                        },
                        "volumeMounts": volume_mounts,
                    }],
                    "volumes": volumes,
                },
            },
            "volumeClaimTemplates": [{
                "metadata": { "name": "data" },
                "spec": pvc_spec,
            }],
        },
    })
}

/// The router Deployment. The pod template carries a hash of the rendered
/// haproxy.cfg so ANY config change rolls the routers (subPath mounts never update).
pub fn router_deployment(cr: &DeltaGliderProxy) -> Value {
    let name = cr_name(cr);
    let router_name = format!("{name}-router");
    let ns = cr.meta().namespace.clone().unwrap_or_default();
    let mut pod_labels = labels(&name, "router");
    pod_labels["dgp.beshu.tech/ring"] = json!(fnv1a_hex(&haproxy_cfg(&name, &ns, cr.replicas())));
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": meta(cr, &router_name, "router"),
        "spec": {
            "replicas": cr.router_replicas(),
            "selector": { "matchLabels": {
                "app.kubernetes.io/instance": name,
                "app.kubernetes.io/component": "router",
            }},
            "template": {
                "metadata": { "labels": pod_labels },
                "spec": {
                    "automountServiceAccountToken": false,
                    "securityContext": {
                        "runAsNonRoot": true,
                        // The official haproxy image's unprivileged user; :9000 needs no caps.
                        "runAsUser": 99,
                        "runAsGroup": 99,
                    },
                    "containers": [{
                        "name": "haproxy",
                        "image": cr.router_image(),
                        "ports": [{ "name": "http", "containerPort": PROXY_PORT }],
                        "securityContext": {
                            "readOnlyRootFilesystem": true,
                            "allowPrivilegeEscalation": false,
                            "capabilities": { "drop": ["ALL"] },
                        },
                        "livenessProbe": {
                            "tcpSocket": { "port": PROXY_PORT },
                            "initialDelaySeconds": 3, "periodSeconds": 10,
                        },
                        "readinessProbe": {
                            "tcpSocket": { "port": PROXY_PORT },
                            "initialDelaySeconds": 3, "periodSeconds": 5,
                        },
                        "volumeMounts": [{
                            "name": "config",
                            "mountPath": "/usr/local/etc/haproxy/haproxy.cfg",
                            "subPath": "haproxy.cfg",
                            "readOnly": true,
                        }],
                    }],
                    "volumes": [{
                        "name": "config",
                        "configMap": { "name": router_name },
                    }],
                },
            },
        },
    })
}

/// PDB: a node drain must never take every router down (the routers ARE the entrypoint).
pub fn router_pdb(cr: &DeltaGliderProxy) -> Value {
    let name = cr_name(cr);
    json!({
        "apiVersion": "policy/v1",
        "kind": "PodDisruptionBudget",
        "metadata": meta(cr, &format!("{name}-router"), "router"),
        "spec": {
            "minAvailable": 1,
            "selector": { "matchLabels": {
                "app.kubernetes.io/instance": name,
                "app.kubernetes.io/component": "router",
            }},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{DeltaGliderProxySpec, RouterSpec, StorageSpec};
    use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
    use k8s_openapi::api::core::v1::{ConfigMap, Service};

    fn cr(replicas: i32) -> DeltaGliderProxy {
        let mut cr = DeltaGliderProxy::new(
            "dgp",
            DeltaGliderProxySpec {
                replicas: Some(replicas),
                image: None,
                config_yaml: Some("storage:\n  filesystem: /data/storage\n".into()),
                env_from_secret: Some("dgp-env".into()),
                storage: Some(StorageSpec {
                    size: Some("20Gi".into()),
                    storage_class: None,
                }),
                router: Some(RouterSpec {
                    replicas: Some(2),
                    image: None,
                }),
                service: None,
                resources: None,
            },
        );
        cr.meta_mut().namespace = Some("dgp-ns".into());
        cr.meta_mut().uid = Some("uid-1".into());
        cr
    }

    #[test]
    fn haproxy_cfg_pins_by_path_and_lists_stable_pod_dns() {
        let cfg = haproxy_cfg("dgp", "dgp-ns", 3);
        assert!(
            cfg.contains("balance uri path-only"),
            "S3 backend must hash by path only (query string + h2 authority excluded)"
        );
        assert!(cfg.contains("hash-type consistent"));
        assert!(
            cfg.contains("balance source"),
            "admin backend must be source-sticky"
        );
        assert!(cfg.contains("acl is_admin path_beg /_/"));
        assert!(
            cfg.contains("option forwardfor"),
            "router must stamp X-Forwarded-For for rate-limit/IAM/session IPs"
        );
        assert!(
            cfg.contains("option httpchk GET /_/ready")
                && cfg.contains("http-check expect status 200"),
            "ring membership must track real readiness, not liveness"
        );
        for i in 0..3 {
            let host = format!("dgp-{i}.dgp-pods.dgp-ns.svc.cluster.local:9000");
            assert_eq!(cfg.matches(&host).count(), 2, "{host} in both backends");
        }
        assert!(!cfg.contains("dgp-3."), "no server beyond replica count");
    }

    #[test]
    fn haproxy_cfg_is_deterministic_across_routers() {
        assert_eq!(haproxy_cfg("dgp", "ns", 4), haproxy_cfg("dgp", "ns", 4));
    }

    #[test]
    fn children_typecheck_and_carry_owner_refs() {
        let cr = cr(3);
        let sts: StatefulSet = serde_json::from_value(proxy_statefulset(&cr)).unwrap();
        let dep: Deployment = serde_json::from_value(router_deployment(&cr)).unwrap();
        let hs: Service = serde_json::from_value(headless_service(&cr)).unwrap();
        let es: Service = serde_json::from_value(entry_service(&cr)).unwrap();
        let rcm: ConfigMap = serde_json::from_value(router_configmap(&cr)).unwrap();
        let ccm: ConfigMap = serde_json::from_value(config_configmap(&cr).unwrap()).unwrap();
        for m in [
            sts.metadata.clone(),
            dep.metadata.clone(),
            hs.metadata,
            es.metadata,
            rcm.metadata,
            ccm.metadata,
        ] {
            let orefs = m.owner_references.unwrap();
            assert_eq!(orefs.len(), 1);
            assert_eq!(orefs[0].kind, "DeltaGliderProxy");
            assert_eq!(orefs[0].controller, Some(true));
        }
        let sts_spec = sts.spec.unwrap();
        assert_eq!(sts_spec.replicas, Some(3));
        assert_eq!(sts_spec.service_name.as_deref(), Some("dgp-pods"));
        assert_eq!(dep.spec.unwrap().replicas, Some(2));
    }

    #[test]
    fn config_mounted_over_writable_data_via_subpath() {
        let cr = cr(1);
        let sts: StatefulSet = serde_json::from_value(proxy_statefulset(&cr)).unwrap();
        let pod = sts.spec.unwrap().template.spec.unwrap();
        let c = &pod.containers[0];
        let cfg_mount = c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "config")
            .expect("config volume mount");
        assert_eq!(cfg_mount.mount_path, CONFIG_MOUNT);
        assert_eq!(cfg_mount.sub_path.as_deref(), Some(CONFIG_FILENAME));
        let env = c.env.as_ref().unwrap();
        assert!(env
            .iter()
            .any(|e| e.name == "DGP_CONFIG" && e.value.as_deref() == Some(CONFIG_MOUNT)));
        assert!(env
            .iter()
            .any(|e| e.name == "DGP_TRUST_PROXY_HEADERS" && e.value.as_deref() == Some("true")));
    }

    #[test]
    fn config_change_rolls_proxy_pods_via_template_hash() {
        let a = cr(1);
        let mut b = cr(1);
        b.spec.config_yaml = Some("storage:\n  filesystem: /data/other\n".into());
        let hash = |cr: &DeltaGliderProxy| {
            proxy_statefulset(cr)["spec"]["template"]["metadata"]["annotations"]
                ["dgp.beshu.tech/config-hash"]
                .clone()
        };
        assert!(hash(&a).is_string());
        assert_ne!(
            hash(&a),
            hash(&b),
            "configYaml edit must roll the StatefulSet"
        );
        let mut c = cr(1);
        c.spec.config_yaml = None;
        assert!(hash(&c).is_null(), "no inline config → no hash annotation");
    }

    #[test]
    fn router_pdb_keeps_one_router_through_drains() {
        use k8s_openapi::api::policy::v1::PodDisruptionBudget;
        let pdb: PodDisruptionBudget = serde_json::from_value(router_pdb(&cr(2))).unwrap();
        let spec = pdb.spec.unwrap();
        assert_eq!(
            spec.min_available,
            Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(1))
        );
    }

    #[test]
    fn no_config_yaml_means_no_configmap_and_no_mount() {
        let mut cr = cr(1);
        cr.spec.config_yaml = None;
        assert!(config_configmap(&cr).is_none());
        let sts: StatefulSet = serde_json::from_value(proxy_statefulset(&cr)).unwrap();
        let pod = sts.spec.unwrap().template.spec.unwrap();
        assert!(pod.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .all(|m| m.name != "config"));
    }

    #[test]
    fn ring_annotation_rolls_routers_on_scale() {
        let a = router_deployment(&cr(2));
        let b = router_deployment(&cr(3));
        let ring =
            |v: &Value| v["spec"]["template"]["metadata"]["labels"]["dgp.beshu.tech/ring"].clone();
        assert_ne!(ring(&a), ring(&b));
    }
}
