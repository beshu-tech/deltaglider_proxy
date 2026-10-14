// SPDX-License-Identifier: BUSL-1.1

//! Source guard (issue #116): every encryption config of a `Config` goes
//! through the one site visitor on every secrets surface.

/// Every struct field that holds an encryption config is a site of
/// `Config::encryption_sites`, and every secrets surface walks the config
/// through that visitor. A new field (or a new hand-rolled walk) fails
/// here until the visitor covers it.
#[test]
fn every_encryption_site_goes_through_the_visitor() {
    use crate::source_scan::{prod_lines, rel, rust_files};
    // (file, field): the flat Config's singleton, NamedBackendConfig, the
    // bucket policy; config_sections' wire copy collapses into the first.
    let known: &[(&str, &str)] = &[
        ("src/config/mod.rs", "backend_encryption"),
        ("src/config/mod.rs", "encryption"),
        ("src/bucket_policy.rs", "encryption"),
        ("src/config_sections.rs", "backend_encryption"),
    ];
    let field = regex_lite::Regex::new(
        r"^\s*(pub(\([a-z]+\))?\s+)?([a-z_]+):\s*(Option<)?(crate::config::|super::)?BackendEncryptionConfig>?,?\s*$",
    )
    .unwrap();
    let mut found = Vec::new();
    for path in rust_files("src") {
        let r = rel(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        for (_, line) in prod_lines(&text) {
            if let Some(c) = field.captures(line) {
                found.push((r.clone(), c[3].to_string()));
            }
        }
    }
    for (file, name) in &found {
        assert!(
            known.iter().any(|(f, n)| f == file && n == name),
            "new encryption-config field {file}:{name}: add it to Config::encryption_sites \
             and to this list"
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
