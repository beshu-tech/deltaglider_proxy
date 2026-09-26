// SPDX-License-Identifier: BUSL-1.1

//! Process-wide "graceful shutdown started" signal.
//!
//! Set once when SIGTERM or SIGINT arrives, BEFORE the server drains and the
//! runtime tears down. While the runtime tears down, work that is still
//! running fails for reasons that say nothing about the data (for example,
//! `spawn_blocking` is refused). Background jobs read this signal to tell such
//! an error from a real one, and to stop at a safe point instead of settling.

use std::sync::LazyLock;

use tokio_util::sync::CancellationToken;

static TOKEN: LazyLock<CancellationToken> = LazyLock::new(CancellationToken::new);

/// Mark the process as shutting down. Idempotent.
pub fn begin() {
    TOKEN.cancel();
}

/// True once [`begin`] ran.
pub fn is_shutting_down() -> bool {
    TOKEN.is_cancelled()
}

/// Resolves when [`begin`] runs (at once if it already ran).
pub async fn started() {
    TOKEN.cancelled().await
}
