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

static EXIT_CODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Start the graceful shutdown (the server drains, as on SIGTERM) and make
/// the process exit with `code` after the drain. For a failure that the
/// process cannot go on with but that a hard `process::exit` would turn
/// into a dropped drain. The first non-zero code stays.
pub fn request_exit(code: i32) {
    let _ = EXIT_CODE.compare_exchange(
        0,
        code,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    );
    begin();
}

/// The exit code that [`request_exit`] asked for, if any (non-zero).
pub fn requested_exit_code() -> Option<i32> {
    match EXIT_CODE.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        code => Some(code),
    }
}

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
