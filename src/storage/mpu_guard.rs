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
    /// Kept (not consumed) by [`Self::abort`] until its request ended, so
    /// a cut during that request leaves the guard armed.
    abort: Option<Box<dyn Fn() -> AbortFuture + Send + Sync>>,
}

impl MpuAbortGuard {
    /// Arm a guard. `abort` makes the best-effort abort request; it may run
    /// twice (an explicit abort that is cut, then the drop), and an abort
    /// of an upload that is gone is harmless.
    pub fn new<F, Fut>(abort: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
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

    /// Abort now and wait for the request (the error path). The guard stays
    /// armed until the request ended: a future that is cut here still
    /// aborts the upload from the drop.
    pub async fn abort(mut self) {
        if let Some(abort) = self.abort.as_ref() {
            abort().await;
        }
        self.abort = None;
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
        let guard = MpuAbortGuard::new(move || {
            let n2 = n2.clone();
            async move {
                n2.fetch_add(1, Ordering::SeqCst);
            }
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

    /// A cut during the explicit abort request still aborts, from the drop.
    #[tokio::test]
    async fn an_abort_that_is_cut_leaves_the_guard_armed() {
        let n = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let (n2, started2) = (n.clone(), started.clone());
        let guard = MpuAbortGuard::new(move || {
            let (n2, started2) = (n2.clone(), started2.clone());
            async move {
                // The first request stalls (it is cut); the second finishes.
                if started2.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::future::pending::<()>().await;
                }
                n2.fetch_add(1, Ordering::SeqCst);
            }
        });
        let cut = tokio::time::timeout(std::time::Duration::from_millis(50), guard.abort()).await;
        assert!(cut.is_err(), "the abort was cut");
        settle().await;
        assert_eq!(started.load(Ordering::SeqCst), 2, "the drop aborted again");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn explicit_abort_runs_once_and_drop_adds_nothing() {
        let (n, guard) = counting();
        guard.abort().await;
        settle().await;
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    /// The backends' own `create_multipart_upload` methods, which implement
    /// or forward the request: the caller above them holds the guard. An
    /// exemption is a (file, function) pair, never a function name alone.
    const TRAIT_METHODS: [(&str, &str); 4] = [
        // The client's CreateMultipartUpload: an in-memory relay upload, no
        // backend upload exists until the store at Complete.
        ("src/s3_adapter_s3s/mod.rs", "create_multipart_upload"),
        ("src/storage/s3/mod.rs", "create_multipart_upload"),
        ("src/storage/routing.rs", "create_multipart_upload"),
        ("src/storage/encrypting/mod.rs", "create_multipart_upload"),
    ];

    /// Openers that hand the guard to their caller (it is the caller that
    /// disarms or aborts it); every caller of one is checked too.
    const RETURNS_GUARD: [(&str, &str); 1] = [(
        "src/deltaglider/engine/store.rs",
        "begin_passthrough_multipart",
    )];

    /// Does the function `func` of `src` end a guard (`disarm` or `abort`)?
    fn ends_guard(src: &crate::source_scan::ast::Source, func: usize) -> bool {
        src.prod
            .calls
            .iter()
            .any(|c| c.func == func && (c.named("disarm") || c.named("abort")))
    }

    /// Functions that open a multipart upload on a backend without a guard:
    /// the guard is made in the same function, bound to a variable (an
    /// unbound one would drop, and abort, at once), and ended there with
    /// `disarm` or `abort` (or the function is in [`RETURNS_GUARD`], and
    /// each of its callers ends the guard instead).
    fn unguarded(sources: &[crate::source_scan::ast::Source]) -> Vec<String> {
        let mut out = Vec::new();
        for src in sources {
            for call in src.prod.calls_named("create_multipart_upload") {
                let name = src.fn_name(call.func);
                if TRAIT_METHODS
                    .iter()
                    .any(|(rel, f)| *rel == src.rel && *f == name)
                {
                    continue;
                }
                let bound = src.prod.bindings.iter().any(|b| {
                    b.func == call.func && b.init_idents.iter().any(|i| i == "MpuAbortGuard")
                });
                let returns = RETURNS_GUARD
                    .iter()
                    .any(|(rel, f)| *rel == src.rel && *f == name);
                if !bound || !(returns || ends_guard(src, call.func)) {
                    out.push(format!(
                        "{}:{} in {name} (guard bound to a variable, then disarm or abort)",
                        src.rel, call.line
                    ));
                }
            }
            for (_, opener) in RETURNS_GUARD {
                for call in src.prod.calls_named(opener) {
                    if src.fn_name(call.func) != opener && !ends_guard(src, call.func) {
                        out.push(format!(
                            "{}:{} calls {opener} and never ends its guard",
                            src.rel, call.line
                        ));
                    }
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
        // The scan sees the openers and the caller it is about.
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
        let transfer = sources.iter().find(|s| s.rel == "src/transfer.rs").unwrap();
        assert!(
            transfer
                .prod
                .calls_named("begin_passthrough_multipart")
                .next()
                .is_some(),
            "the scan missed the caller of the opener"
        );
        let fixture = [
            crate::source_scan::ast::parse(
                "src/x.rs",
                "async fn bad(c: C) { c.create_multipart_upload().send().await; } \
                 async fn good(c: C) { let g = MpuAbortGuard::new(|| async {}); \
                 c.create_multipart_upload().send().await; g.disarm(); } \
                 async fn unbound(c: C) { MpuAbortGuard::new(|| async {}); \
                 c.create_multipart_upload().send().await; } \
                 async fn never_ended(c: C) { let g = MpuAbortGuard::new(|| async {}); \
                 c.create_multipart_upload().send().await; } \
                 async fn caller(e: E) { let (h, g) = e.begin_passthrough_multipart(l).await; }",
            ),
            // A trait method of that name in a file that is not listed.
            crate::source_scan::ast::parse(
                "src/y.rs",
                "async fn create_multipart_upload(c: C) { c.create_multipart_upload(); }",
            ),
        ];
        let found = unguarded(&fixture);
        let names: Vec<&str> = found.iter().map(String::as_str).collect();
        assert_eq!(found.len(), 5, "{names:?}");
        for expected in [
            "in bad",
            "in unbound",
            "in never_ended",
            "src/y.rs",
            "never ends",
        ] {
            assert!(
                found.iter().any(|f| f.contains(expected)),
                "{expected} not caught: {names:?}"
            );
        }
    }
}
