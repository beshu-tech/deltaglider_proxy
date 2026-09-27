// SPDX-License-Identifier: BUSL-1.1

use crate::config::*;

/// Review-2 (S9): one ref into a typed field that serializes as a string
/// (`listen_addr: SocketAddr`) makes the value-tree round-trip fail, and
/// the fallback clone writes EVERY ref-sourced secret in plaintext.
#[test]
fn review2_ref_in_typed_field_does_not_abort_reinsertion() {
    let mut cfg = Config {
        secret_access_key: Some("custom-secret-value-1".into()),
        ..Default::default()
    };
    cfg.listen_addr = "0.0.0.0:9123".parse().unwrap();
    cfg.env_refs.insert("LISTEN".into(), "0.0.0.0:9123".into());
    cfg.env_refs
        .insert("MYSECRET".into(), "custom-secret-value-1".into());
    assert_eq!(
        cfg.with_env_refs_reinserted().secret_access_key.as_deref(),
        Some("${env:MYSECRET}")
    );
}

/// Review-2 (S9): the comment says `us-east-1` in several fields stays
/// materialized, but it is 9 bytes (>= SHARED_REF_MIN_LEN = 8), so every
/// field that holds it is coupled to the env var.
#[test]
fn review2_region_value_in_two_fields_stays_uncoupled() {
    let mut cfg = Config {
        access_key_id: Some("us-east-1".into()),
        secret_access_key: Some("us-east-1".into()),
        ..Default::default()
    };
    cfg.env_refs.insert("REGION".into(), "us-east-1".into());
    let out = cfg.with_env_refs_reinserted();
    assert_eq!(
        (
            out.access_key_id.as_deref(),
            out.secret_access_key.as_deref()
        ),
        (Some("us-east-1"), Some("us-east-1")),
        "a non-secret shared value got coupled to ${{env:REGION}}"
    );
}

/// Review-2 (S8): the non-TTY banner tells the operator to run
/// `deltaglider_proxy --set-bootstrap-password <pw>`. The flag is a bool
/// that reads the password from stdin; the binary answers
/// `error: unrecognized subcommand '<pw>'`.
#[test]
fn review2_first_run_banner_advises_a_command_that_parses() {
    let lines = first_run_banner("pw", "hash", false);
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("--set-bootstrap-password <pw>")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("' | deltaglider_proxy --set-bootstrap-password`")),
        "the stdin form: {lines:?}"
    );
}
