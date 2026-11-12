// SPDX-License-Identifier: BUSL-1.1

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
/// S3 traffic is consistent-hashed by the DIRECTORY of the URL path (= the deltaspace),
/// so every request touching one delta prefix — all keys in it, all multipart parts —
/// lands on the same proxy pod, keeping the in-process reference lock sufficient.
/// Admin UI traffic (/_/) is source-IP sticky because sessions are in-memory.
pub fn haproxy_cfg(cr_name: &str, namespace: &str, proxy_replicas: i32) -> String {
    let mut servers = String::new();
    let mut admin_servers = String::new();
    for i in 0..proxy_replicas {
        // StatefulSet stable DNS names keep the hash ring identical on every router.
        let host = format!("{cr_name}-{i}.{cr_name}-pods.{namespace}.svc.cluster.local");
        servers.push_str(&format!(
            "  server dgp{i} {host}:{PROXY_PORT} check resolvers kube init-addr none\n"
        ));
        // The admin backend reuses the s3 backend's check of the same pod:
        // one readiness check per pod per router, not two.
        admin_servers.push_str(&format!(
            "  server dgp{i} {host}:{PROXY_PORT} track s3/dgp{i} resolvers kube init-addr none\n"
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

# Hash the DIRECTORY of the path (last segment → constant, query ignored): one
# deltaspace = one pod, so the in-process reference lock and MPU state stay safe.
backend s3
  balance hash path,regsub([^/]*$,x)
  hash-type consistent
  option httpchk GET /_/ready
  http-check expect status 200
  default-server inter 10s rise 2 fall 3
{servers}
# Admin UI sessions are in-memory and IP-bound: source-IP stickiness.
# Each server tracks the s3 backend's check of the same pod.
backend admin
  balance source
  hash-type consistent
{admin_servers}"#
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

/// ConfigMap holding the rendered haproxy.cfg. `proxy_replicas` is the EFFECTIVE
/// ring size (preflight may hold it below spec.replicas).
pub fn router_configmap(cr: &DeltaGliderProxy, proxy_replicas: i32) -> Value {
    let name = format!("{}-router", cr_name(cr));
    let ns = cr.meta().namespace.clone().unwrap_or_default();
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": meta(cr, &name, "router"),
        "data": { "haproxy.cfg": haproxy_cfg(&cr_name(cr), &ns, proxy_replicas) },
    })
}

/// Key of the config DB key in the bootstrap Secret (`DGP_CONFIG_DB_KEY` of every pod).
pub const DB_KEY_SECRET_KEY: &str = "dbKey";

/// The size limit of each proxy pod's `/tmp` emptyDir, in MiB. The proxy's
/// spool (`DGP_SPOOL_DIR`, default `/tmp/dgp-spool`) lives there: upload
/// bodies above the spool threshold, large delta reconstructions, multipart
/// relay parts. Without a limit the spool could fill the node's disk; with
/// one, a spool budget above it gets the pod evicted.
pub const TMP_SIZE_LIMIT_MIB: u64 = 8192;

/// The grace period of a proxy pod's stop, in seconds. A proxy stop takes
/// up to 55 s (10 s each for requests, a maintenance job, the event outbox
/// and `DGP_SHUTDOWN_TIMEOUT_SECS` of blocking work, and 15 s for work that
/// outlives its request); the Kubernetes
/// default of 30 s would kill it first. The Helm chart uses the same value.
pub const TERMINATION_GRACE_SECS: i64 = 60;

/// The `timeoutSeconds` of a proxy pod's readiness probe. `/_/ready` runs its
/// checks at the same time and answers within 3 s at the latest (the proxy's
/// `READY_DEADLINE`); the Kubernetes default of 1 s would fail a healthy pod
/// whose checks take longer. The Helm chart uses the same value.
pub const READINESS_TIMEOUT_SECS: i64 = 5;

/// Pure: the `DGP_SPOOL_MAX_BYTES` for a `/tmp` of `limit_mib`: three
/// quarters of it, so the spool never reaches the limit (the rest is for
/// the other files in `/tmp`).
pub fn spool_max_bytes_for_tmp(limit_mib: u64) -> u64 {
    limit_mib * 1024 * 1024 / 4 * 3
}

/// The auto-generated bootstrap Secret (`password` for the human, `hash` and `dbKey`
/// for the pods). Create-once and NO ownerReference: `dbKey` decrypts IAM data on
/// PVCs that survive CR deletion, so the Secret must survive too (manual teardown,
/// like the PVCs).
pub fn bootstrap_secret(
    cr: &DeltaGliderProxy,
    password: &str,
    hash_b64: &str,
    db_key: &str,
) -> Value {
    let name = format!("{}-bootstrap", cr_name(cr));
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": name,
            "namespace": cr.meta().namespace,
            "labels": labels(cr.meta().name.as_deref().unwrap_or_default(), "proxy"),
        },
        "type": "Opaque",
        "stringData": { "password": password, "hash": hash_b64, DB_KEY_SECRET_KEY: db_key },
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
/// as the Helm chart / Dockerfile. `replicas` is the EFFECTIVE count.
pub fn proxy_statefulset(cr: &DeltaGliderProxy, replicas: i32) -> Value {
    let name = cr_name(cr);
    let has_config = cr.spec.config_yaml.is_some();

    // The router is the only supported entrypoint and stamps X-Forwarded-For
    // (option forwardfor): trust it so rate limits, aws:SourceIp conditions, and
    // IP-bound admin sessions see the real client, not the router pod. Trust
    // needs the router's networks: without them the proxy refuses to boot,
    // because any client could write the header.
    let mut env = vec![
        json!({ "name": "DGP_TRUST_PROXY_HEADERS", "value": "true" }),
        json!({ "name": "DGP_TRUSTED_PROXY_CIDRS", "value": cr.trusted_proxy_cidrs() }),
        // The spool budget stays below the size limit of /tmp, where it lives.
        json!({
            "name": "DGP_SPOOL_MAX_BYTES",
            "value": spool_max_bytes_for_tmp(TMP_SIZE_LIMIT_MIB).to_string(),
        }),
    ];
    if has_config {
        env.push(json!({ "name": "DGP_CONFIG", "value": CONFIG_MOUNT }));
    }
    if cr.bootstrap_auto_generate() {
        env.push(json!({
            "name": "DGP_BOOTSTRAP_PASSWORD_HASH",
            "valueFrom": { "secretKeyRef": {
                "name": format!("{name}-bootstrap"), "key": "hash",
            }},
        }));
        env.push(json!({
            "name": "DGP_CONFIG_DB_KEY",
            "valueFrom": { "secretKeyRef": {
                "name": format!("{name}-bootstrap"), "key": DB_KEY_SECRET_KEY,
            }},
        }));
    }

    let mut env_from = vec![];
    if let Some(secret) = &cr.spec.env_from_secret {
        env_from.push(json!({ "secretRef": { "name": secret } }));
    }

    let mut volume_mounts = vec![
        json!({ "name": "data", "mountPath": "/data" }),
        json!({ "name": "tmp", "mountPath": "/tmp" }),
    ];
    let mut volumes = vec![json!({
        "name": "tmp",
        "emptyDir": { "sizeLimit": format!("{TMP_SIZE_LIMIT_MIB}Mi") },
    })];
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
            "replicas": replicas,
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
                    "terminationGracePeriodSeconds": TERMINATION_GRACE_SECS,
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
                            "timeoutSeconds": READINESS_TIMEOUT_SECS,
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
pub fn router_deployment(cr: &DeltaGliderProxy, proxy_replicas: i32) -> Value {
    let name = cr_name(cr);
    let router_name = format!("{name}-router");
    let ns = cr.meta().namespace.clone().unwrap_or_default();
    let mut pod_labels = labels(&name, "router");
    pod_labels["dgp.beshu.tech/ring"] = json!(fnv1a_hex(&haproxy_cfg(&name, &ns, proxy_replicas)));
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

/// The name of the opt-in proxy NetworkPolicy (`spec.router.networkPolicy`).
pub fn proxy_network_policy_name(cr: &DeltaGliderProxy) -> String {
    format!("{}-proxy", cr_name(cr))
}

/// Opt-in NetworkPolicy: the proxy pods accept connections only from this
/// resource's router pods, on the proxy port. The selectors are the
/// StatefulSet and router Deployment selectors. Ingress only, no egress rules:
/// the pods must still reach their storage backend.
pub fn proxy_network_policy(cr: &DeltaGliderProxy) -> Value {
    let name = cr_name(cr);
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": meta(cr, &proxy_network_policy_name(cr), "proxy"),
        "spec": {
            "podSelector": { "matchLabels": {
                "app.kubernetes.io/instance": name,
                "app.kubernetes.io/component": "proxy",
            }},
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{ "podSelector": { "matchLabels": {
                    "app.kubernetes.io/instance": name,
                    "app.kubernetes.io/component": "router",
                }}}],
                "ports": [{ "protocol": "TCP", "port": PROXY_PORT }],
            }],
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
                    trusted_proxy_cidrs: None,
                    network_policy: None,
                }),
                service: None,
                resources: None,
                bootstrap_password: None,
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
            cfg.contains("balance hash path,regsub([^/]*$,x)"),
            "S3 backend must hash by path DIRECTORY: the deltaspace reference lock is \
             in-process, so all writes into one prefix must colocate on one pod \
             (verified live on haproxy:3.0 — same dir/folder-marker → same server, \
             query string ignored)"
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
        assert_eq!(
            cfg.matches("option httpchk").count(),
            1,
            "one readiness check per pod: the admin backend tracks the s3 one"
        );
        for i in 0..3 {
            let host = format!("dgp-{i}.dgp-pods.dgp-ns.svc.cluster.local:9000");
            assert_eq!(cfg.matches(&host).count(), 2, "{host} in both backends");
            assert!(
                cfg.contains(&format!("{host} track s3/dgp{i} ")),
                "admin dgp{i} tracks s3/dgp{i}"
            );
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
        let sts: StatefulSet =
            serde_json::from_value(proxy_statefulset(&cr, cr.replicas())).unwrap();
        let dep: Deployment =
            serde_json::from_value(router_deployment(&cr, cr.replicas())).unwrap();
        let hs: Service = serde_json::from_value(headless_service(&cr)).unwrap();
        let es: Service = serde_json::from_value(entry_service(&cr)).unwrap();
        let rcm: ConfigMap = serde_json::from_value(router_configmap(&cr, cr.replicas())).unwrap();
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

    /// The spool budget is set below the size limit of the /tmp emptyDir
    /// that holds the spool: a budget above it gets the pod evicted, and
    /// /tmp without a limit can fill the node's disk.
    #[test]
    fn the_spool_budget_is_below_the_tmp_size_limit() {
        let sts = proxy_statefulset(&cr(1), 1);
        let pod = &sts["spec"]["template"]["spec"];
        let tmp = pod["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "tmp")
            .unwrap();
        let limit = tmp["emptyDir"]["sizeLimit"].as_str().unwrap();
        let limit_bytes = limit.strip_suffix("Mi").unwrap().parse::<u64>().unwrap() * 1024 * 1024;
        let spool = pod["containers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "DGP_SPOOL_MAX_BYTES")
            .expect("DGP_SPOOL_MAX_BYTES is set")["value"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(spool > 0 && spool < limit_bytes, "{spool} vs {limit}");
        assert_eq!(spool_max_bytes_for_tmp(4), 3 * 1024 * 1024);
    }

    /// A proxy stop takes up to 55 s, so the pod's grace period is longer
    /// than that (the Kubernetes default of 30 s would SIGKILL a drain).
    #[test]
    fn proxy_pods_get_a_grace_period_longer_than_a_stop() {
        let cr = cr(1);
        let sts: StatefulSet = serde_json::from_value(proxy_statefulset(&cr, 1)).unwrap();
        let grace = sts
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .termination_grace_period_seconds;
        assert_eq!(grace, Some(TERMINATION_GRACE_SECS));
        const { assert!(TERMINATION_GRACE_SECS > 55) };
    }

    /// The readiness probe waits longer than `/_/ready` takes in the worst
    /// case (3 s), not the 1 s default.
    #[test]
    fn proxy_pods_get_a_readiness_timeout_that_fits_the_worst_case() {
        let cr = cr(1);
        let sts: StatefulSet = serde_json::from_value(proxy_statefulset(&cr, 1)).unwrap();
        let probe = sts.spec.unwrap().template.spec.unwrap().containers[0]
            .readiness_probe
            .clone()
            .unwrap();
        assert_eq!(
            probe.timeout_seconds,
            Some(READINESS_TIMEOUT_SECS as i32),
            "the probe must set timeoutSeconds"
        );
        const { assert!(READINESS_TIMEOUT_SECS > 3) };
    }

    #[test]
    fn config_mounted_over_writable_data_via_subpath() {
        let cr = cr(1);
        let sts: StatefulSet =
            serde_json::from_value(proxy_statefulset(&cr, cr.replicas())).unwrap();
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

    /// auth-2: the proxy refuses DGP_TRUST_PROXY_HEADERS=true without
    /// DGP_TRUSTED_PROXY_CIDRS, so the operator always sets both.
    #[test]
    fn proxy_pods_trust_only_the_router_networks() {
        let cidrs = |cr: &DeltaGliderProxy| {
            let sts: StatefulSet =
                serde_json::from_value(proxy_statefulset(cr, cr.replicas())).unwrap();
            let pod = sts.spec.unwrap().template.spec.unwrap();
            pod.containers[0]
                .env
                .as_ref()
                .unwrap()
                .iter()
                .find(|e| e.name == "DGP_TRUSTED_PROXY_CIDRS")
                .and_then(|e| e.value.clone())
        };
        assert_eq!(
            cidrs(&cr(1)).as_deref(),
            Some(crate::crd::DEFAULT_TRUSTED_PROXY_CIDRS)
        );
        let mut narrow = cr(1);
        narrow
            .spec
            .router
            .get_or_insert_with(Default::default)
            .trusted_proxy_cidrs = Some(vec!["10.42.0.0/16".into(), "fd00:42::/56".into()]);
        assert_eq!(cidrs(&narrow).as_deref(), Some("10.42.0.0/16,fd00:42::/56"));
        let mut empty = cr(1);
        empty
            .spec
            .router
            .get_or_insert_with(Default::default)
            .trusted_proxy_cidrs = Some(vec![]);
        assert_eq!(
            cidrs(&empty).as_deref(),
            Some(crate::crd::DEFAULT_TRUSTED_PROXY_CIDRS),
            "an empty list would make the proxy refuse to boot"
        );
    }

    #[test]
    fn config_change_rolls_proxy_pods_via_template_hash() {
        let a = cr(1);
        let mut b = cr(1);
        b.spec.config_yaml = Some("storage:\n  filesystem: /data/other\n".into());
        let hash = |cr: &DeltaGliderProxy| {
            proxy_statefulset(cr, 1)["spec"]["template"]["metadata"]["annotations"]
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
    fn clamped_replicas_build_one_pod_objects() {
        // The preflight-hold path: spec says 3, effective is 1.
        let cr = cr(3);
        let sts: StatefulSet = serde_json::from_value(proxy_statefulset(&cr, 1)).unwrap();
        assert_eq!(sts.spec.unwrap().replicas, Some(1));
        let ring = haproxy_cfg("dgp", "dgp-ns", 1);
        assert_eq!(
            ring.matches("server dgp0 ").count(),
            2,
            "one pod in both backends"
        );
        assert!(
            !ring.contains("server dgp1 "),
            "held ring must not list pod 1"
        );
    }

    #[test]
    fn bootstrap_secret_survives_cr_deletion() {
        use k8s_openapi::api::core::v1::Secret;
        let s: Secret =
            serde_json::from_value(bootstrap_secret(&cr(1), "pw", "aGFzaA==", "k")).unwrap();
        assert!(
            s.metadata.owner_references.is_none(),
            "an ownerReference would GC the dbKey while the PVCs survive → IAM DB lockout"
        );
        let data = s.string_data.unwrap();
        assert!(data.contains_key("password") && data.contains_key("hash"));
        assert!(data.contains_key(DB_KEY_SECRET_KEY));
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
        let sts: StatefulSet =
            serde_json::from_value(proxy_statefulset(&cr, cr.replicas())).unwrap();
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
        let a = router_deployment(&cr(2), 2);
        let b = router_deployment(&cr(3), 3);
        let ring =
            |v: &Value| v["spec"]["template"]["metadata"]["labels"]["dgp.beshu.tech/ring"].clone();
        assert_ne!(ring(&a), ring(&b));
    }

    #[test]
    fn network_policy_admits_only_this_instances_routers() {
        use k8s_openapi::api::networking::v1::NetworkPolicy;
        let c = cr(2);
        let np: NetworkPolicy = serde_json::from_value(proxy_network_policy(&c)).unwrap();
        assert_eq!(np.metadata.name.as_deref(), Some("dgp-proxy"));
        assert_eq!(np.metadata.owner_references.as_ref().unwrap().len(), 1);
        let spec = np.spec.unwrap();

        // The policy selects exactly the StatefulSet's pods.
        let sts: StatefulSet = serde_json::from_value(proxy_statefulset(&c, 2)).unwrap();
        let sts_selector = sts.spec.unwrap().selector.match_labels.unwrap();
        assert_eq!(
            spec.pod_selector.unwrap().match_labels.unwrap(),
            sts_selector
        );
        assert_eq!(spec.policy_types.unwrap(), vec!["Ingress".to_string()]);

        // One ingress rule: from the router Deployment's pods, to the proxy port.
        let dep: Deployment = serde_json::from_value(router_deployment(&c, 2)).unwrap();
        let router_selector = dep.spec.unwrap().selector.match_labels.unwrap();
        let ingress = spec.ingress.unwrap();
        assert_eq!(ingress.len(), 1);
        let from = ingress[0].from.as_ref().unwrap();
        assert_eq!(from.len(), 1);
        assert!(from[0].namespace_selector.is_none(), "same namespace only");
        assert_eq!(
            from[0]
                .pod_selector
                .as_ref()
                .unwrap()
                .match_labels
                .as_ref()
                .unwrap(),
            &router_selector
        );
        let ports = ingress[0].ports.as_ref().unwrap();
        assert_eq!(
            serde_json::to_value(&ports[0].port).unwrap(),
            serde_json::json!(PROXY_PORT)
        );
    }

    #[test]
    fn network_policy_is_opt_in() {
        let mut c = cr(1);
        assert!(!c.network_policy());
        c.spec.router.as_mut().unwrap().network_policy = Some(true);
        assert!(c.network_policy());
    }
}
