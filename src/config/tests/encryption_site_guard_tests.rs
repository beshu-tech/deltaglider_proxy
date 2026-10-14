// SPDX-License-Identifier: BUSL-1.1

//! Source guard (issue #116): every encryption config of a `Config` goes
//! through the one site visitor on every secrets surface.

/// Every struct field that holds an encryption config is a site of
/// `Config::encryption_sites`, and every secrets surface walks the config
/// through that visitor. A new field (or a new hand-rolled walk) fails
/// here until the visitor covers it.
#[test]
fn every_encryption_site_goes_through_the_visitor() {
    use crate::source_scan::prod_lines;
    // (file, enclosing type, field): the flat Config's singleton,
    // NamedBackendConfig, the bucket policy; the sectioned wire shape's
    // StorageSection collapses into the first (`into_flat`).
    let known: &[(&str, &str, &str)] = &[
        ("src/config/mod.rs", "Config", "backend_encryption"),
        ("src/config/mod.rs", "NamedBackendConfig", "encryption"),
        ("src/bucket_policy.rs", "BucketPolicyConfig", "encryption"),
        (
            "src/config_sections.rs",
            "StorageSection",
            "backend_encryption",
        ),
    ];
    let found = encryption_config_fields();
    for (file, ty, field) in &found {
        assert!(
            known
                .iter()
                .any(|(f, t, n)| f == file && t == ty && n == field),
            "new encryption-config field {file}: {ty}.{field}: add it to \
             Config::encryption_sites and to this list"
        );
    }
    for (f, t, n) in known {
        assert!(
            found
                .iter()
                .any(|(ff, tt, nn)| ff == f && tt == t && nn == n),
            "{f}: {t}.{n} no longer found: the scan may be blind, fix it"
        );
    }
    // The secrets surfaces call the per-config helpers only from inside the
    // visitor loops: each helper has exactly these production call sites.
    let calls: &[(&str, &str, usize)] = &[
        ("src/config/mod.rs", "redact_secrets()", 1),
        ("src/config/bucket_encryption.rs", "redact_secrets()", 1),
        ("src/api/admin/backup.rs", "SecretsEncryption::harvest(", 1),
        ("src/api/admin/backup.rs", ".hydrate(", 1),
        ("src/api/admin/config/section_level.rs", "fp_enc(", 2),
    ];
    for (file, needle, want) in calls {
        let text = crate::source_scan::read(file);
        let n = prod_lines(&text)
            .iter()
            .filter(|(_, l)| l.contains(needle))
            .count();
        assert_eq!(
            n, *want,
            "{file}: `{needle}` must stay inside the site visitor"
        );
    }
    // An API response that hands out the bucket policies redacts them.
    let field_level = crate::source_scan::read("src/api/admin/config/field_level.rs");
    assert!(
        !prod_lines(&field_level)
            .iter()
            .any(|(_, l)| l.contains("buckets.clone()")),
        "GET /config must use Config::redacted_bucket_policies"
    );
    for (file, needle) in [
        ("src/config/mod.rs", "encryption_sites_mut()"),
        ("src/api/admin/backup.rs", "encryption_sites()"),
        ("src/api/admin/backup.rs", "encryption_sites_mut()"),
        (
            "src/api/admin/config/section_level.rs",
            "encryption_sites_mut()",
        ),
        ("src/config/env_overrides.rs", "encryption_sites()"),
    ] {
        assert!(
            crate::source_scan::read(file).contains(needle),
            "{file} must walk the encryption sites with `{needle}`"
        );
    }
}

/// Review G: a key id has one resolver, `config::resolve_site_keys`. In
/// production code, only it derives an id and parses a configured key (the
/// preserve step keeps its hex-validity filter, which resolves nothing).
#[test]
fn key_ids_have_one_resolver() {
    use crate::source_scan::{prod_lines, rel, rust_files};
    let allowed: &[(&str, &str, usize)] = &[
        ("src/config/keys.rs", "derive_key_id(", 2),
        // The definition.
        (
            "src/deltaglider/engine/construction.rs",
            "derive_key_id(",
            1,
        ),
        ("src/config/keys.rs", "EncryptionKey::from_hex(", 1),
        (
            "src/api/admin/config/section_level.rs",
            "EncryptionKey::from_hex(",
            2,
        ),
    ];
    let mut offenders = Vec::new();
    for path in rust_files("src") {
        let r = rel(&path);
        if r.ends_with("tests.rs") || r.contains("/tests/") || r.ends_with("_tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for needle in ["derive_key_id(", "EncryptionKey::from_hex("] {
            let n = prod_lines(&text)
                .iter()
                .filter(|(_, l)| l.contains(needle) && !l.trim_start().starts_with("//"))
                .count();
            let limit = allowed
                .iter()
                .find(|(f, nd, _)| *f == r && *nd == needle)
                .map_or(0, |(_, _, n)| *n);
            if n > limit {
                offenders.push(format!("{r}: {needle} x{n} (allowed {limit})"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "resolve key ids with config::resolve_site_keys: {offenders:?}"
    );
}

/// Review F: every field of every struct or enum (production code) whose
/// TYPE mentions `BackendEncryptionConfig` anywhere (bare, `Option<..>`,
/// `Vec<..>`, `BTreeMap<_, ..>`, a tuple), as `(file, type, field)`. Enum
/// tuple variants count with the variant name as the field.
fn encryption_config_fields() -> Vec<(String, String, String)> {
    use crate::source_scan::{prod_lines, rel, rust_files};
    let head = regex_lite::Regex::new(
        r"^(\s*)(pub(\([a-z]+\))?\s+)?(struct|enum)\s+([A-Za-z0-9_]+)[^;]*\{\s*$",
    )
    .unwrap();
    let field =
        regex_lite::Regex::new(r"^\s*(pub(\([a-z]+\))?\s+)?([A-Za-z0-9_]+)\s*[:(]").unwrap();
    let mut out = Vec::new();
    for path in rust_files("src") {
        let r = rel(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines = prod_lines(&text);
        let mut open: Option<(String, String)> = None; // (type name, indent)
        for (_, line) in &lines {
            if let Some((ty, indent)) = &open {
                if line.starts_with(&format!("{indent}}}")) {
                    open = None;
                    continue;
                }
                let code = line.split("//").next().unwrap_or("");
                // A borrowed field (`&'a BackendEncryptionConfig`) is a VIEW
                // of a site (BucketOverride, BucketEncryption), not a site.
                let ty_part = code.split_once(':').map_or(code, |(_, t)| t);
                if code.contains("BackendEncryptionConfig")
                    && !ty_part.trim_start().starts_with('&')
                {
                    let name = field
                        .captures(code)
                        .map(|c| c[3].to_string())
                        .unwrap_or_else(|| code.trim().to_string());
                    out.push((r.clone(), ty.clone(), name));
                }
            } else if let Some(c) = head.captures(line) {
                open = Some((c[5].to_string(), c[1].to_string()));
            }
        }
    }
    out
}

/// Review F, the serde level: a recognisable key in EVERY slot of every
/// site (backend key and legacy key, singleton, named backend, bucket
/// override) never reaches a redacted surface: GET /config's policies,
/// the redacted config, the canonical export.
#[test]
fn no_key_survives_a_redacted_surface() {
    let key = |n: u8| format!("{n:x}c0ffee{n:x}").repeat(8);
    let yaml = format!(
        "storage:\n  backend_encryption: {{ mode: aes256-gcm-proxy, key: \"{}\", legacy_key: \"{}\" }}\n  \
         backends:\n    - name: local-disk\n      type: filesystem\n      path: /tmp/p116-f\n      \
         encryption: {{ mode: aes256-gcm-proxy, key: \"{}\", legacy_key: \"{}\" }}\n    \
         - name: aws-dr\n      type: filesystem\n      path: /tmp/p116-f2\n      \
         encryption: {{ mode: none, legacy_key: \"{}\" }}\n  \
         buckets:\n    db-archive:\n      backend: local-disk\n      encryption: {{ mode: \
         aes256-gcm-proxy, key: \"{}\", key_id: db-archive-b, legacy_key: \"{}\", legacy_key_id: \
         db-archive-a }}\n",
        key(1),
        key(2),
        key(3),
        key(4),
        key(5),
        key(6),
        key(7)
    );
    let cfg = crate::config::Config::from_yaml_str(&yaml).unwrap();
    assert_eq!(key(1).len(), 64);
    let hex64 = regex_lite::Regex::new("[0-9a-f]{64}").unwrap();
    let redacted = cfg.redact_all_secrets();
    let surfaces = [
        (
            "redact_all_secrets (json)",
            serde_json::to_string(&redacted).unwrap(),
        ),
        (
            "redact_all_secrets (sectioned yaml)",
            serde_yaml::to_string(&crate::config_sections::SectionedConfig::from_flat(
                &redacted,
            ))
            .unwrap(),
        ),
        (
            "canonical export",
            cfg.to_canonical_yaml_with(&|_| None).unwrap(),
        ),
        (
            "GET /config bucket_policies",
            serde_json::to_string(&cfg.redacted_bucket_policies()).unwrap(),
        ),
    ];
    for (what, text) in surfaces {
        assert!(
            !hex64.is_match(&text),
            "{what} carries a 64-hex key:\n{text}"
        );
    }
}
