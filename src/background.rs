// SPDX-License-Identifier: GPL-3.0-only

//! Shared infrastructure for background job runners.
//!
//! - [`parse_duration_or`] — env-var duration parsing with sensible
//!   defaults (used by replication, lifecycle, event_delivery).

use std::time::Duration;
use tracing::warn;

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
