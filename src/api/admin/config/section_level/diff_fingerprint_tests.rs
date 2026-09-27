// SPDX-License-Identifier: BUSL-1.1

use super::*;

fn base_cfg() -> crate::config::Config {
    crate::config::Config {
        access_key_id: Some("admin".into()),
        secret_access_key: Some("old-secret".into()),
        ..Default::default()
    }
}

/// The bug this guards: a credentials rotation used to produce an
/// EMPTY diff (both sides redacted to None) and the Apply dialog
/// claimed "No changes detected" for an apply that changed them.
#[test]
fn credential_rotation_surfaces_in_diff() {
    let old_cfg = base_cfg();
    let mut new_cfg = base_cfg();
    new_cfg.secret_access_key = Some("new-secret".into());
    let diff = compute_section_diff(SectionName::Access, &old_cfg, &new_cfg);
    let access = diff.get("access").and_then(|v| v.as_object()).unwrap();
    let change = access
        .get("secret_access_key")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!("rotation must appear in the diff: {diff}"));
    let before = change["before"].as_str().unwrap();
    let after = change["after"].as_str().unwrap();
    assert!(
        before.starts_with("fp:"),
        "fingerprint, not plaintext: {before}"
    );
    assert!(after.starts_with("fp:"));
    assert_ne!(before, after);
    assert!(!diff.to_string().contains("old-secret"), "no leak");
    assert!(!diff.to_string().contains("new-secret"), "no leak");
}

#[test]
fn preserved_credential_produces_no_diff() {
    let old_cfg = base_cfg();
    let new_cfg = base_cfg();
    let diff = compute_section_diff(SectionName::Access, &old_cfg, &new_cfg);
    let access = diff.get("access").and_then(|v| v.as_object()).unwrap();
    assert!(
        access.is_empty(),
        "identical configs must diff empty, got {diff}"
    );
}

#[test]
fn env_ref_secrets_stay_readable_in_diff() {
    let old_cfg = base_cfg();
    let mut new_cfg = base_cfg();
    new_cfg.secret_access_key = Some("${env:DGP_BOOTSTRAP_SECRET_ACCESS_KEY}".into());
    let diff = compute_section_diff(SectionName::Access, &old_cfg, &new_cfg);
    let s = diff.to_string();
    assert!(
        s.contains("${env:DGP_BOOTSTRAP_SECRET_ACCESS_KEY}"),
        "refs are not secrets — show them verbatim: {s}"
    );
}

#[test]
fn iam_user_secret_rotation_surfaces_in_diff() {
    let mk = |secret: &str| {
        let mut c = base_cfg();
        c.iam_users = vec![crate::iam::DeclarativeUser {
            name: "ci-uploader".into(),
            access_key_id: "ci-uploader".into(),
            secret_access_key: secret.into(),
            enabled: true,
            groups: vec![],
            permissions: vec![],
            auth_source: None,
        }];
        c
    };
    let diff = compute_section_diff(SectionName::Access, &mk("a-secret"), &mk("b-secret"));
    let s = diff.to_string();
    assert!(s.contains("fp:"), "user secret rotation must surface: {s}");
    assert!(
        !s.contains("a-secret") && !s.contains("b-secret"),
        "no leak"
    );
}
