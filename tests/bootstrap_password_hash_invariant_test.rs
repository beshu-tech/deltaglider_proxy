// SPDX-License-Identifier: GPL-3.0-only

//! Default test harness bootstrap hash must be stable across builders so
//! HA replicas share one SQLCipher key (see `TEST_BOOTSTRAP_PASSWORD_HASH`
//! in `common/mod.rs`).

mod common;

use common::TestServer;

fn extract_bootstrap_hash_line(cfg: &str) -> &str {
    for line in cfg.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("bootstrap_password_hash: \"") {
            return v.trim_end_matches('"');
        }
    }
    panic!("bootstrap_password_hash not found in config:\n{cfg}");
}

#[test]
fn default_bootstrap_hash_matches_across_builders() {
    let a = TestServer::builder().generated_config_document();
    let b = TestServer::builder().generated_config_document();

    let h_a = extract_bootstrap_hash_line(&a);
    let h_b = extract_bootstrap_hash_line(&b);

    assert_eq!(h_a, h_b, "two default builders must agree");
    assert_eq!(
        h_a,
        common::TEST_BOOTSTRAP_PASSWORD_HASH,
        "default builders must embed the shared constant hash"
    );
}
