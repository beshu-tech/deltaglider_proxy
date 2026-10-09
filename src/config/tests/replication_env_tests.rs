// SPDX-License-Identifier: BUSL-1.1

use crate::config::*;

fn only(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |n: &str| {
        pairs
            .iter()
            .find(|(k, _)| *k == n)
            .map(|(_, v)| v.to_string())
    }
}

/// `DGP_REPLICATION_TRANSFERS` and `DGP_UPLOAD_CONCURRENCY` are registered
/// and documented, and environment variables win over the file. The
/// replication worker reads `replication.transfers` / `.upload_concurrency`
/// from the config, so the override must land there (the env readers in
/// `transfer_plan` never reached a replication run).
#[test]
fn replication_concurrency_env_vars_override_the_file() {
    let mut cfg = Config::default();
    cfg.replication.transfers = 2;
    cfg.replication.upload_concurrency = 3;
    cfg.apply_env_overrides_with(&only(&[
        ("DGP_REPLICATION_TRANSFERS", "9"),
        ("DGP_UPLOAD_CONCURRENCY", "7"),
    ]));
    assert_eq!(cfg.replication.transfers, 9);
    assert_eq!(cfg.replication.upload_concurrency, 7);

    // Unset or unparsable: the file value stays.
    let mut cfg = Config::default();
    cfg.replication.transfers = 2;
    cfg.apply_env_overrides_with(&only(&[("DGP_REPLICATION_TRANSFERS", "many")]));
    assert_eq!(cfg.replication.transfers, 2);
    assert_eq!(
        cfg.replication.upload_concurrency,
        crate::transfer_plan::UPLOAD_CONCURRENCY as u32
    );
}

/// B034: a blank secret (compose substitutes an unset variable as "") is no
/// credential. It must not turn SigV4 on with an empty secret that any
/// client that knows the logged access key id can sign with.
#[test]
fn a_blank_bootstrap_secret_is_no_credential() {
    let mut cfg = Config::default();
    cfg.apply_env_overrides_with(&only(&[
        ("DGP_ACCESS_KEY_ID", "AKTEST"),
        ("DGP_SECRET_ACCESS_KEY", ""),
    ]));
    assert!(!cfg.auth_enabled(), "a blank secret enabled SigV4");
    assert!(matches!(
        cfg.classify_auth_config(false),
        AuthConfigOutcome::Missing
    ));

    let yaml = Config::from_yaml_str("access:\n  access_key_id: AKTEST\n  secret_access_key: ''\n")
        .expect("parse");
    assert!(!yaml.auth_enabled(), "a blank YAML secret enabled SigV4");
}
