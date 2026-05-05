//! Default test harness bootstrap hash must be stable across builders and
//! config formats so HA replicas share one SQLCipher key (see
//! `TEST_BOOTSTRAP_PASSWORD_HASH` in `common/mod.rs`).

mod common;

use common::TestServer;

fn extract_bootstrap_hash_line(cfg: &str) -> &str {
    for line in cfg.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("bootstrap_password_hash = \"") {
            return v.trim_end_matches('"');
        }
        if let Some(v) = t.strip_prefix("bootstrap_password_hash: \"") {
            return v.trim_end_matches('"');
        }
    }
    panic!("bootstrap_password_hash not found in config:\n{cfg}");
}

#[test]
fn default_bootstrap_hash_matches_across_builders_and_toml_yaml() {
    let tom_a = TestServer::builder().generated_config_document();
    let tom_b = TestServer::builder().generated_config_document();
    let y_a = TestServer::builder()
        .yaml_config()
        .generated_config_document();
    let y_b = TestServer::builder()
        .yaml_config()
        .generated_config_document();

    let h_tom_a = extract_bootstrap_hash_line(&tom_a);
    let h_tom_b = extract_bootstrap_hash_line(&tom_b);
    let h_y_a = extract_bootstrap_hash_line(&y_a);
    let h_y_b = extract_bootstrap_hash_line(&y_b);

    assert_eq!(h_tom_a, h_tom_b, "two default TOML builders must agree");
    assert_eq!(h_y_a, h_y_b, "two default YAML builders must agree");
    assert_eq!(h_tom_a, h_y_a, "TOML and YAML default paths must agree");
}
