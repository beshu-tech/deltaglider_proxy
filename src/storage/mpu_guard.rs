// SPDX-License-Identifier: BUSL-1.1

//! [`MpuAbortGuard`]: THE drop guard of every multipart upload that the
//! proxy opens on a backend for its own writes (a streaming copy, an
//! UploadPartCopy copy).
//!
//! A backend keeps the parts of an unfinished upload (and bills for them)
//! until somebody aborts it. A future that is dropped (a timeout, a killed
//! run, a shutdown) never reaches its `Err` arm, so the abort cannot hang on
//! error handling. The guard is armed when the upload is created and
//! disarmed only after Complete returned `Ok`; every other exit, a drop
//! included, aborts the upload.

use std::future::Future;
use std::pin::Pin;

type AbortFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Aborts a multipart upload when dropped, unless [`Self::disarm`]ed.
#[must_use = "a guard that is dropped at once aborts the upload at once"]
pub struct MpuAbortGuard {
    abort: Option<Box<dyn FnOnce() -> AbortFuture + Send>>,
}

impl MpuAbortGuard {
    /// Arm a guard. `abort` makes the best-effort abort request.
    pub fn new<F, Fut>(abort: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self {
            abort: Some(Box::new(move || Box::pin(abort()))),
        }
    }

    /// Complete returned `Ok`: nothing is left to abort.
    pub fn disarm(mut self) {
        self.abort = None;
    }

    /// Abort now and wait for the request (the error path).
    pub async fn abort(mut self) {
        if let Some(abort) = self.abort.take() {
            abort().await;
        }
    }
}

impl Drop for MpuAbortGuard {
    fn drop(&mut self) {
        let Some(abort) = self.abort.take() else {
            return;
        };
        // Drop is sync: the abort request runs on its own task.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(abort());
            }
            Err(_) => tracing::warn!(
                "a multipart upload was dropped outside a runtime; its parts stay on the backend"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn counting() -> (Arc<AtomicUsize>, MpuAbortGuard) {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let guard = MpuAbortGuard::new(move || async move {
            n2.fetch_add(1, Ordering::SeqCst);
        });
        (n, guard)
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn drop_aborts_once() {
        let (n, guard) = counting();
        drop(guard);
        settle().await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn disarm_aborts_nothing() {
        let (n, guard) = counting();
        guard.disarm();
        settle().await;
        assert_eq!(n.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn explicit_abort_runs_once_and_drop_adds_nothing() {
        let (n, guard) = counting();
        guard.abort().await;
        settle().await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    /// Functions that call `create_multipart_upload` without the guard: the
    /// trait methods themselves (`create_multipart_upload` of the backends
    /// and their wrappers: they forward or implement the request, the caller
    /// above them holds the guard). Every other function that opens a
    /// multipart upload on a backend arms an [`MpuAbortGuard`] in the same
    /// function, so the upload cannot outlive a dropped future.
    fn unguarded(sources: &[crate::source_scan::ast::Source]) -> Vec<String> {
        let mut out = Vec::new();
        for src in sources {
            for call in src.prod.calls_named("create_multipart_upload") {
                let name = src.fn_name(call.func);
                let guarded =
                    src.prod.calls.iter().any(|c| {
                        c.func == call.func && c.segs.iter().any(|s| s == "MpuAbortGuard")
                    });
                if name != "create_multipart_upload" && !guarded {
                    out.push(format!("{}:{} in {name}", src.rel, call.line));
                }
            }
        }
        out
    }

    #[test]
    fn every_backend_multipart_upload_has_an_abort_guard() {
        let sources = crate::source_scan::ast::src();
        let found = unguarded(sources);
        assert!(found.is_empty(), "an upload without a guard: {found:?}");
        // The scan sees the two openers it is about.
        for (rel, name) in [
            ("src/storage/s3/copy.rs", "copy_parts"),
            (
                "src/deltaglider/engine/store.rs",
                "begin_passthrough_multipart",
            ),
        ] {
            let src = sources.iter().find(|s| s.rel == rel).unwrap();
            assert!(
                src.prod
                    .calls_named("create_multipart_upload")
                    .any(|c| src.fn_name(c.func) == name),
                "the scan missed {name} in {rel}"
            );
        }
        let fixture = [crate::source_scan::ast::parse(
            "src/x.rs",
            "async fn bad(c: C) { c.create_multipart_upload().send().await; } \
                 async fn good(c: C) { let _g = MpuAbortGuard::new(|| async {}); \
                 c.create_multipart_upload().send().await; } \
                 async fn create_multipart_upload(c: C) { c.create_multipart_upload(); }",
        )];
        assert_eq!(unguarded(&fixture), ["src/x.rs:1 in bad"]);
    }
}
