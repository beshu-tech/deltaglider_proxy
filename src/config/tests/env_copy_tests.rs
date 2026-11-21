// SPDX-License-Identifier: BUSL-1.1

//! A server-side copy of a field that comes from a reference carries the
//! template with it, and ends with the copy. No decision compares a string
//! with the value of a secret: a literal is a literal, wherever it stands.

use crate::config::env_template::Escape;
use crate::config::*;

const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const SECRET: &str = "S3CR3T-VALUE-9f8e7d";

const BOOT: &str = "\
access:
  access_key_id: AKBOOT
  secret_access_key: ${env:SK}
storage:
  backend:
    type: filesystem
    path: ./data
  buckets:
    releases:
      encryption:
        mode: aes256-gcm-proxy
        key: ${env:BUCKET_KEY}
        key_id: rel
";

fn lookup(name: &str) -> Option<String> {
    match name {
        "SK" => Some(SECRET.into()),
        "BUCKET_KEY" => Some(KEY.into()),
        _ => None,
    }
}

fn boot() -> Config {
    Config::from_template_with(BOOT, &lookup).expect("boot loads")
}

const ROUTE: &str = "__dgmigrate_releases_0";

/// The server copies the bucket's encryption block to a transient route, as the
/// migrate stage does.
fn with_route(cfg: &mut Config) {
    let enc = cfg.buckets["releases"].encryption.clone();
    cfg.buckets.insert(
        ROUTE.into(),
        crate::bucket_policy::BucketPolicyConfig {
            encryption: enc,
            ..Default::default()
        },
    );
}

/// A literal that the operator wrote beside a reference holds the same value
/// as the reference: persist and export write it as the literal it is.
#[test]
fn one_value_by_reference_and_as_a_literal_is_written_as_each() {
    let text = format!(
        "access:\n  access_key_id: AKBOOT\n  secret_access_key: ${{env:SK}}\nstorage:\n  backend:\n    type: s3\n    endpoint: http://127.0.0.1:1\n    region: us-east-1\n    access_key_id: AKS\n    secret_access_key: {SECRET}\n"
    );
    let cfg = Config::from_template_with(&text, &lookup).unwrap();
    let out = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert_eq!(out.matches("${env:SK}").count(), 1, "{out}");
    assert_eq!(out.matches(SECRET).count(), 1, "{out}");
}

#[test]
fn a_copy_that_carries_the_template_is_written_as_the_reference() {
    let mut cfg = boot();
    with_route(&mut cfg);
    cfg.env_refs.derive_templates(
        "/buckets/releases/encryption",
        &format!("/buckets/{ROUTE}/encryption"),
    );
    let text = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(!text.contains(KEY), "{text}");
    assert_eq!(text.matches("${env:BUCKET_KEY}").count(), 2, "{text}");
    // A restart reads the reference at the copy's path as an ordinary one.
    let again = Config::from_template_with(&text, &lookup).unwrap();
    assert_eq!(
        again.env_refs.frozen.templates[&format!("/buckets/{ROUTE}/encryption/key")]
            .render(Escape::Minimal),
        "${env:BUCKET_KEY}"
    );
    assert!(again.buckets[ROUTE].encryption.is_some());
}

#[test]
fn a_dropped_copy_leaves_no_template_behind() {
    let mut cfg = boot();
    cfg.env_refs.derive_templates(
        "/buckets/releases/encryption",
        &format!("/buckets/{ROUTE}/encryption"),
    );
    assert!(cfg
        .env_refs
        .template_at(&format!("/buckets/{ROUTE}/encryption/key"))
        .is_some());
    cfg.env_refs
        .drop_templates_under(&format!("/buckets/{ROUTE}"));
    assert!(cfg
        .env_refs
        .template_at(&format!("/buckets/{ROUTE}/encryption/key"))
        .is_none());
    // The source stays.
    assert!(cfg
        .env_refs
        .template_at("/buckets/releases/encryption/key")
        .is_some());
}

/// THE list of the places that copy a value that may be a credential from one
/// config field to another or into a view. A new copy fails this test until it
/// carries the template (`EnvRefs::derive_templates`) or is classified here.
/// Rows: file, text, how many times, why it is safe.
const COPY_SITES: &[(&str, &str, usize, &str)] = &[
    (
        "src/config/mod.rs",
        "self.backend.clone()",
        1,
        "add_named_backend: derives the templates",
    ),
    (
        "src/config/mod.rs",
        "self.backend_encryption.clone()",
        1,
        "add_named_backend: derives the templates",
    ),
    (
        "src/maintenance/migrate.rs",
        "p.encryption.clone()",
        1,
        "route_policy: insert_route derives the templates",
    ),
    (
        "src/api/admin/config/section_level.rs",
        "old_primary.clone()",
        1,
        "key retirement: derives legacy_key",
    ),
    (
        "src/api/admin/config/mod.rs",
        "prev.secret_access_key.clone()",
        1,
        "same path, matched by name",
    ),
    (
        "src/api/admin/config/mod.rs",
        "prev.client_secret.clone()",
        1,
        "same path, matched by name",
    ),
    (
        "src/api/admin/config/field_level.rs",
        "cfg.backend.clone()",
        1,
        "response view, read only",
    ),
    (
        "src/api/admin/config/field_level.rs",
        "cfg.backend_encryption.clone()",
        1,
        "response view, read only",
    ),
    (
        "src/config/bucket_encryption.rs",
        "p.encryption.clone()",
        1,
        "effective encryption lookup, read only",
    ),
    (
        "src/storage/s3/mod.rs",
        "self.backend.clone()",
        1,
        "storage handle, not config",
    ),
    (
        "src/startup.rs",
        "boot.backend.clone()",
        1,
        "engine build at boot",
    ),
    (
        "src/coordination/health.rs",
        "config.backend.clone()",
        1,
        "health probe view",
    ),
    (
        "src/coordination/health.rs",
        "named.backend.clone()",
        1,
        "health probe view",
    ),
    (
        "src/iam/declarative.rs",
        "db.client_secret.clone()",
        1,
        "reconciler: the destination rule decides",
    ),
    (
        "src/iam/declarative.rs",
        "db.secret_access_key.clone()",
        1,
        "reconciler: same user row",
    ),
    (
        "src/event_delivery.rs",
        "event_delivery.clone()",
        1,
        "dispatcher reads its own settings",
    ),
    (
        "src/api/admin/event_outbox.rs",
        "event_delivery.clone()",
        1,
        "read only",
    ),
];

const COPY_NEEDLES: &[&str] = &[
    "self.backend.clone()",
    "self.backend_encryption.clone()",
    "cfg.backend.clone()",
    "cfg.backend_encryption.clone()",
    "config.backend.clone()",
    "named.backend.clone()",
    "boot.backend.clone()",
    "p.encryption.clone()",
    "old_primary.clone()",
    "prev.secret_access_key.clone()",
    "prev.client_secret.clone()",
    "webhook_headers.clone()",
    "event_delivery.clone()",
    "auth_providers.clone()",
    "iam_users.clone()",
    "db.client_secret.clone()",
    "db.secret_access_key.clone()",
];

#[test]
fn every_copy_of_a_possible_credential_is_classified() {
    let mut found = Vec::new();
    for (rel, text) in crate::source_scan::prod_sources("src") {
        let prod = crate::source_scan::prod_text(&text);
        for needle in COPY_NEEDLES {
            let n = prod.matches(needle).count();
            if n > 0 {
                found.push((rel.clone(), needle.to_string(), n));
            }
        }
    }
    let known: Vec<(String, String, usize)> = COPY_SITES
        .iter()
        .map(|(f, n, c, _)| (f.to_string(), n.to_string(), *c))
        .collect();
    let unclassified: Vec<_> = found.iter().filter(|f| !known.contains(f)).collect();
    let gone: Vec<_> = known.iter().filter(|k| !found.contains(k)).collect();
    assert!(
        unclassified.is_empty() && gone.is_empty(),
        "a copy of a possible credential changed. New or changed: {unclassified:?}. \
         Listed but absent: {gone:?}. Carry the template with the copy \
         (EnvRefs::derive_templates) and update COPY_SITES."
    );
}

const ROUTE_FILE: &str = "storage:\n  buckets:\n    __dgmigrate_a_0:\n      encryption:\n        mode: aes256-gcm-proxy\n        key: ${env:BUCKET_KEY}\n        key_id: x\n    keep:\n      encryption:\n        mode: aes256-gcm-proxy\n        key: ${env:BUCKET_KEY}\n        key_id: y\n";

fn violation(cfg: &Config, new: &Config, server_writer: bool) -> Option<String> {
    let tree = |c: &Config| serde_yaml::to_value(c).unwrap();
    crate::config::env_template::frozen_violation(
        &cfg.env_refs,
        &tree(cfg),
        &tree(new),
        server_writer,
    )
}

/// A `__dgmigrate_*` route belongs to the migrate job: after a restart has
/// read its references from the file, only the job's own writer may remove it.
#[test]
fn only_the_job_may_end_a_route_that_the_file_gave_references() {
    let cfg = Config::from_template_with(ROUTE_FILE, &lookup).unwrap();
    let mut route_gone = cfg.clone();
    route_gone.buckets.remove("__dgmigrate_a_0");
    assert!(violation(&cfg, &route_gone, true).is_none());
    assert!(
        violation(&cfg, &route_gone, false).is_some(),
        "an admin write may not"
    );
    let mut keep_gone = cfg.clone();
    keep_gone.buckets.remove("keep");
    assert!(
        violation(&cfg, &keep_gone, true).is_some(),
        "a bucket of the operator stays frozen, also for the job"
    );
}

/// The references of a route end with it, the frozen ones too: no reference
/// stays to be accepted at a route that is made again.
#[test]
fn a_route_that_ends_takes_its_frozen_references_with_it() {
    let mut cfg = Config::from_template_with(ROUTE_FILE, &lookup).unwrap();
    let route = "/buckets/__dgmigrate_a_0/encryption/key";
    assert!(cfg.env_refs.frozen.templates.contains_key(route));
    cfg.env_refs
        .drop_templates_under("/buckets/__dgmigrate_a_0");
    assert!(cfg.env_refs.template_at(route).is_none());
    assert!(cfg
        .env_refs
        .template_at("/buckets/keep/encryption/key")
        .is_some());
}
