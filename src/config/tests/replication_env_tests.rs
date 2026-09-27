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
