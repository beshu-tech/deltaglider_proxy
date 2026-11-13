// SPDX-License-Identifier: BUSL-1.1

//! THE log capture of unit tests: every test that reads log output goes
//! through [`install`] / [`capture`] (source guard
//! `log_capture_goes_through_the_one_helper` in `lib.rs`).
//!
//! `tracing` caches, per callsite, whether any subscriber wants it. While
//! only ONE dispatcher is alive, a callsite that a thread without a
//! subscriber reaches first is cached as "never" for every thread
//! (tracing-core 0.1.36 `callsite.rs`: `Dispatchers::rebuilder` returns
//! `JustOne` when `has_just_one`, and `JustOne` asks only the CURRENT
//! thread's default dispatcher, `rebuild_callsite_interest`). A capture then
//! sees nothing from that callsite, however its own subscriber is set up,
//! and a "this secret is not logged" check passes without a check.
//!
//! The fix is a process-wide dispatcher that is never current and answers
//! "sometimes" for every callsite. With two live dispatchers `rebuilder`
//! returns `Read`, which ANDs the answers of all of them, so no callsite is
//! cached as "never" and each one asks the subscriber of its own thread.
//! A capture is then thread-local: it sees the events of its own test only.

use std::sync::{Arc, Mutex, OnceLock};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::{DefaultGuard, Interest};
use tracing::{Dispatch, Event, Metadata, Subscriber};

/// A subscriber that wants nothing itself and keeps every callsite open.
struct KeepCallsitesOpen;

impl Subscriber for KeepCallsitesOpen {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        false
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

static KEEP_OPEN: OnceLock<Dispatch> = OnceLock::new();

/// Make `subscriber` the default of THIS thread until the guard drops.
/// Use it for a subscriber that is not a plain text capture.
pub(crate) fn install<S>(subscriber: S) -> DefaultGuard
where
    S: Subscriber + Send + Sync + 'static,
{
    // First, so the dispatcher of the test is never the only one.
    KEEP_OPEN.get_or_init(|| Dispatch::new(KeepCallsitesOpen));
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    guard
}

/// A writer that keeps every byte of the log output.
#[derive(Clone, Default)]
pub(crate) struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The text a test thread logged since [`capture`] / [`capture_with`].
pub(crate) struct LogCapture {
    sink: Sink,
    _guard: DefaultGuard,
}

impl LogCapture {
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.sink.0.lock().unwrap().clone()).unwrap()
    }

    /// Fail when no line was logged by `target` (a module path prefix). Call
    /// it for every layer the test claims to watch: a layer that logged
    /// nothing leaves a "not logged" check with nothing to check.
    #[track_caller]
    pub(crate) fn require_target(&self, target: &str) {
        let text = self.text();
        assert!(
            has_target(&text, target),
            "the capture has no line from the target `{target}`, so a check on it is blind:\n{text}"
        );
    }
}

/// Does a `fmt` line carry `target` (a prefix) as its target? The target is
/// a word that ends in `:` (`s3s::service:`); a word in a message does not.
fn has_target(text: &str, target: &str) -> bool {
    text.split_whitespace()
        .any(|w| w.starts_with(target) && w.ends_with(':'))
}

/// Capture the events of this thread that the `EnvFilter` directives pass.
pub(crate) fn capture(directives: &str) -> LogCapture {
    let filter: tracing_subscriber::EnvFilter = directives.parse().unwrap();
    capture_with(|sink| {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(sink)
            .finish()
    })
}

/// Capture into a sink of the caller's own `fmt` subscriber.
pub(crate) fn capture_with<S>(build: impl FnOnce(Sink) -> S) -> LogCapture
where
    S: Subscriber + Send + Sync + 'static,
{
    let sink = Sink::default();
    let guard = install(build(sink.clone()));
    LogCapture {
        sink,
        _guard: guard,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(who: &str) {
        tracing::info!(target: "t7_capture_probe", who, "probe line");
    }

    /// The blind spot itself: a thread with no subscriber reaches a callsite
    /// FIRST, while the capture is the only dispatcher; the capture of the
    /// test thread must still see that callsite, and only its own events.
    /// This is the only test that reaches `probe`, so its first hit is here.
    #[test]
    fn a_capture_sees_a_callsite_that_another_thread_reached_first() {
        let cap = capture("trace");
        std::thread::spawn(|| probe("noise")).join().unwrap();
        probe("test");
        cap.require_target("t7_capture_probe");
        let text = cap.text();
        assert!(text.contains("who=\"test\""), "{text}");
        assert!(!text.contains("noise"), "{text}");
    }

    /// Pure, so it makes no second dispatcher beside the one of the test above.
    #[test]
    fn a_target_is_seen_only_when_a_line_names_it() {
        let text = "2026-11-13T00:00:00Z  INFO s3s::service: request done\n\
                    2026-11-13T00:00:00Z DEBUG tower_http::trace: started\n";
        assert!(has_target(text, "s3s"));
        assert!(has_target(text, "tower_http"));
        assert!(!has_target(text, "aws_sigv4"));
        assert!(!has_target("", "s3s"));
        // A span prefix before the target.
        assert!(has_target(
            "INFO request{method=GET}: tower_http::trace: started",
            "tower_http"
        ));
        // A word in a message is not a target.
        assert!(!has_target("INFO app: the s3s crate said hi", "s3s"));
    }
}
