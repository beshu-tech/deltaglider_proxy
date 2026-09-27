// SPDX-License-Identifier: BUSL-1.1

use crate::config::*;

/// `RUST_LOG` sets the running filter at startup, above `DGP_LOG_LEVEL`
/// and the file. The config must say so: the GUI showed the file's
/// level (default debug) while the process ran at `RUST_LOG=info`, and
/// a GUI apply then replaced the `RUST_LOG` filter.
#[test]
fn rust_log_is_the_effective_log_level() {
    let only = |pairs: &'static [(&'static str, &'static str)]| {
        move |n: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.to_string())
        }
    };
    let mut cfg = Config::default();
    cfg.apply_env_overrides_with(&only(&[("RUST_LOG", "deltaglider_proxy=info")]));
    assert_eq!(cfg.log_level, "deltaglider_proxy=info");

    let mut cfg = Config::default();
    cfg.apply_env_overrides_with(&only(&[("RUST_LOG", "info"), ("DGP_LOG_LEVEL", "warn")]));
    assert_eq!(cfg.log_level, "info", "RUST_LOG beats DGP_LOG_LEVEL");

    let mut cfg = Config::default();
    cfg.apply_env_overrides_with(&only(&[("DGP_LOG_LEVEL", "warn")]));
    assert_eq!(cfg.log_level, "warn");
}

/// Owner decision (refactor round 2026-09-27): the default is info, not
/// debug — debug logged every request and flooded production logs.
#[test]
fn default_log_level_is_info() {
    assert_eq!(
        Config::default().log_level,
        "deltaglider_proxy=info,tower_http=info"
    );
    assert_eq!(default_log_level(), DEFAULT_LOG_LEVEL);
}
