// SPDX-License-Identifier: GPL-3.0-only

//! Shared infrastructure for background job runners.
//!
//! - [`parse_duration_or`] — env-var duration parsing with sensible
//!   defaults (used by replication, lifecycle, event_delivery).
//! - [`RunLease`] — per-run leader-lease knobs, shared by the replication +
//!   lifecycle workers (their heartbeat loops still differ — replication has a
//!   lock-acquire retry lifecycle doesn't — but the struct is identical).

use std::time::Duration;
use tracing::warn;

/// Leader-lease knobs threaded through a leased background run.
#[derive(Debug, Clone)]
pub struct RunLease {
    pub owner: String,
    pub ttl_secs: i64,
    pub heartbeat_secs: i64,
}

pub(crate) fn parse_duration_or(
    value: &str,
    default: Duration,
    minimum: Duration,
    label: &str,
) -> Duration {
    match humantime::parse_duration(value) {
        Ok(duration) if duration >= minimum => duration,
        Ok(duration) => {
            warn!(
                "{}={} below minimum {}; using {}",
                label,
                humantime::format_duration(duration),
                humantime::format_duration(minimum),
                humantime::format_duration(minimum),
            );
            minimum
        }
        Err(err) => {
            warn!(
                "{}={} invalid: {}; using {}",
                label,
                value,
                err,
                humantime::format_duration(default),
            );
            default
        }
    }
}
