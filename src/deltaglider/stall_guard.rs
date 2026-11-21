// SPDX-License-Identifier: BUSL-1.1

//! A rate floor for a response body that holds spool space.
//!
//! A large delta GET streams a verified spool file, and the stream holds the
//! file and its budget until the last byte went out. A client that opens the
//! response and reads nothing would hold them for ever: the server only polls
//! a body when the client accepts bytes, and the request timeout covers the
//! response future, not the body. A client that reads one small chunk every
//! few seconds holds them almost as long.
//!
//! [`StallGuard`] wraps such a stream and counts the bytes that it hands to
//! the consumer. A watchdog task drops the wrapped stream (the file, its
//! budget and the principal's charge go with it) when, in one stall window,
//! the client accepted fewer than its minimum ([`min_bytes_per_window`]: 64 KiB,
//! more for a large file) and the body did not end. Time that the guard spends waiting for its own stream (storage, the
//! codec) is not the client's and does not count: the window measures client
//! time only. A client that keeps up a modest rate is never cut, however long
//! the download takes.

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::Stream;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Instant;

use crate::deltaglider::spool::HeldSpool;
use crate::storage::StorageError;

/// The least that a client must accept in one stall window, whatever the
/// size of the held file: 64 KiB.
pub const STALL_MIN_BYTES: u64 = 64 * 1024;

/// How a body is watched: in each `window` of client time the consumer must
/// take `min_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallPolicy {
    pub window: Duration,
    pub min_bytes: u64,
}

impl StallPolicy {
    /// THE rule of which bodies are watched: one that holds spool space
    /// (`held_spool`, from the response that the engine made; it also says
    /// how many bytes the body sends, which is less than the file for a range).
    /// The window is `DGP_RESPONSE_STALL_SECS`.
    pub fn for_body(held_spool: HeldSpool, stall_secs: u64, max_hold_secs: u64) -> Option<Self> {
        (held_spool.mib() > 0).then(|| Self {
            window: Duration::from_secs(stall_secs.max(1)),
            min_bytes: min_bytes_per_window(held_spool, stall_secs.max(1), max_hold_secs.max(1)),
        })
    }
}

/// Pure: the least a client must take in one window: 64 KiB, or the smaller
/// of the held bytes and the bytes this body sends, times `window / max_hold`
/// when that is more, so the body ends within about `max_hold` whatever the
/// client does. A range of a large file is paced by its own length.
pub fn min_bytes_per_window(held: HeldSpool, window_secs: u64, max_hold_secs: u64) -> u64 {
    let held_bytes = (held.mib() as u128 * 1024 * 1024).min(u128::from(held.send_bytes()));
    let share = held_bytes * u128::from(window_secs) / u128::from(max_hold_secs.max(1));
    u64::try_from(share)
        .unwrap_or(u64::MAX)
        .max(STALL_MIN_BYTES)
}

/// What the watchdog does at a given time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Check again after this many milliseconds.
    Wait(u64),
    /// The client took too little for a whole window.
    Cut,
}

/// Pure: the rate rule, on a clock that the caller gives (milliseconds).
/// Windows follow each other; a window ends after `window_ms` of CLIENT time,
/// the time in which the guard did not wait for its own stream.
#[derive(Debug)]
pub(crate) struct StallClock {
    window_ms: u64,
    min_bytes: u64,
    /// When this window began.
    start: u64,
    /// The time of this window that the guard spent waiting for its stream.
    waited: u64,
    /// The guard waits for its stream since this time, if it does.
    waiting_since: Option<u64>,
    /// The bytes that the consumer took in this window.
    bytes: u64,
}

impl StallClock {
    pub(crate) fn new(now: u64, policy: StallPolicy) -> Self {
        Self {
            window_ms: u64::try_from(policy.window.as_millis())
                .unwrap_or(u64::MAX / 2)
                .max(1),
            min_bytes: policy.min_bytes,
            start: now,
            waited: 0,
            waiting_since: None,
            bytes: 0,
        }
    }

    /// The guard starts to wait for its own stream (a poll found it pending).
    pub(crate) fn wait_begins(&mut self, now: u64) {
        self.waiting_since.get_or_insert(now);
    }

    /// The consumer took a chunk of `len` bytes (this ends a wait).
    pub(crate) fn chunk(&mut self, now: u64, len: u64) {
        if let Some(since) = self.waiting_since.take() {
            self.waited += now.saturating_sub(since.max(self.start));
        }
        self.bytes = self.bytes.saturating_add(len);
    }

    /// The client time of this window up to `now`.
    fn client_elapsed(&self, now: u64) -> u64 {
        let open = self
            .waiting_since
            .map_or(0, |since| now.saturating_sub(since.max(self.start)));
        now.saturating_sub(self.start)
            .saturating_sub(self.waited)
            .saturating_sub(open)
    }

    /// What to do at `now`: a window that ends with enough bytes starts the
    /// next one.
    pub(crate) fn check(&mut self, now: u64) -> Verdict {
        loop {
            let elapsed = self.client_elapsed(now);
            if elapsed < self.window_ms {
                return Verdict::Wait(self.window_ms - elapsed);
            }
            if self.bytes < self.min_bytes {
                return Verdict::Cut;
            }
            self.start = now;
            self.waited = 0;
            self.waiting_since = self.waiting_since.map(|_| now);
            self.bytes = 0;
        }
    }
}

struct Inner {
    stream: Option<BoxStream<'static, Result<Bytes, StorageError>>>,
    /// `None`: no watchdog, the guard only wraps.
    clock: Option<StallClock>,
    /// The watchdog cut the body: the next poll says so, once.
    cut: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    base: Instant,
    /// Wakes the watchdog when the guard is dropped.
    gone: Arc<tokio::sync::Notify>,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX / 2)
    }
}

/// See the module docs.
pub struct StallGuard {
    shared: Arc<Shared>,
}

impl StallGuard {
    /// Guard `stream`. `policy` `None`: no watchdog, the guard only gives the
    /// stream a `Sync` wrapper. `Some` needs a Tokio runtime.
    pub fn new(
        stream: BoxStream<'static, Result<Bytes, StorageError>>,
        policy: Option<StallPolicy>,
    ) -> Self {
        let base = Instant::now();
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                stream: Some(stream),
                clock: policy.map(|p| StallClock::new(0, p)),
                cut: false,
            }),
            base,
            gone: Arc::new(tokio::sync::Notify::new()),
        });
        if policy.is_some() {
            tokio::spawn(watchdog(Arc::downgrade(&shared)));
        }
        Self { shared }
    }
}

async fn watchdog(shared: std::sync::Weak<Shared>) {
    loop {
        let Some(strong) = shared.upgrade() else {
            return;
        };
        let wait = {
            let mut inner = strong.inner.lock().expect("stall guard mutex poisoned");
            let now = strong.now_ms();
            let Some((verdict, window_ms)) = inner
                .clock
                .as_mut()
                .map(|clock| (clock.check(now), clock.window_ms))
            else {
                return;
            };
            match verdict {
                Verdict::Wait(ms) => ms,
                Verdict::Cut => {
                    // Under the lock a poll holds, so a chunk is never cut in half.
                    inner.cut = inner.stream.take().is_some();
                    if inner.cut {
                        tracing::warn!(
                            "A response body that holds spool space was read at less than {} \
                             KiB in {} s: it ended and released its spool space \
                             (DGP_RESPONSE_STALL_SECS)",
                            STALL_MIN_BYTES / 1024,
                            window_ms / 1000
                        );
                    }
                    return;
                }
            }
        };
        let gone = strong.gone.clone();
        let notified = gone.notified();
        tokio::pin!(notified);
        drop(strong);
        tokio::select! {
            () = &mut notified => return,
            () = tokio::time::sleep(Duration::from_millis(wait.max(1))) => {}
        }
    }
}

impl Stream for StallGuard {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let shared = &self.shared;
        let mut inner = shared.inner.lock().expect("stall guard mutex poisoned");
        let Some(stream) = inner.stream.as_mut() else {
            // Cut by the watchdog (the first poll after it says why), or
            // finished already.
            return Poll::Ready(
                std::mem::take(&mut inner.cut)
                    .then(|| Err(StorageError::Other("response body stalled".into()))),
            );
        };
        let polled = stream.as_mut().poll_next(cx);
        let now = shared.now_ms();
        match &polled {
            Poll::Pending => {
                if let Some(clock) = inner.clock.as_mut() {
                    clock.wait_begins(now);
                }
            }
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(clock) = inner.clock.as_mut() {
                    clock.chunk(now, chunk.len() as u64);
                }
            }
            // The end, or an error: nothing more to hold.
            Poll::Ready(_) => inner.stream = None,
        }
        polled
    }
}

impl Drop for StallGuard {
    fn drop(&mut self) {
        self.shared.gone.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const KIB: usize = 1024;

    fn policy(window_secs: u64) -> StallPolicy {
        StallPolicy {
            window: Duration::from_secs(window_secs),
            min_bytes: STALL_MIN_BYTES,
        }
    }

    // ---- the rate rule, on a given clock ----

    #[test]
    fn a_client_that_reads_nothing_is_cut_at_the_end_of_the_window() {
        let mut c = StallClock::new(0, policy(60));
        assert_eq!(c.check(0), Verdict::Wait(60_000));
        assert_eq!(c.check(59_999), Verdict::Wait(1));
        assert_eq!(c.check(60_000), Verdict::Cut);
    }

    #[test]
    fn one_small_chunk_in_59_seconds_is_cut() {
        let mut c = StallClock::new(0, policy(60));
        c.chunk(59_000, 8 * KIB as u64);
        assert_eq!(c.check(59_500), Verdict::Wait(500));
        assert_eq!(c.check(60_000), Verdict::Cut);
        // The same rate every window: cut in the first one.
        let mut c = StallClock::new(0, policy(60));
        for k in 1..=3u64 {
            c.chunk(k * 59_000, 8 * KIB as u64);
        }
        assert_eq!(c.check(60_000), Verdict::Cut);
    }

    #[test]
    fn a_client_at_100_kib_per_second_is_never_cut() {
        let mut c = StallClock::new(0, policy(60));
        // A chunk of 25 KiB every 250 ms for 20 minutes.
        for step in 1..=4800u64 {
            let t = step * 250;
            c.chunk(t, 25 * KIB as u64);
            assert!(matches!(c.check(t), Verdict::Wait(_)), "cut at {t} ms");
        }
    }

    #[test]
    fn a_window_with_enough_bytes_starts_the_next_one_afresh() {
        let mut c = StallClock::new(0, policy(60));
        c.chunk(1_000, STALL_MIN_BYTES);
        assert_eq!(c.check(60_000), Verdict::Wait(60_000));
        // The second window took nothing: cut at its end.
        assert_eq!(c.check(119_999), Verdict::Wait(1));
        assert_eq!(c.check(120_000), Verdict::Cut);
    }

    #[test]
    fn time_spent_waiting_for_the_stream_is_not_the_clients() {
        let mut c = StallClock::new(0, policy(60));
        // The codec needs 5 minutes for the first chunk.
        c.wait_begins(10);
        assert_eq!(c.check(150_000), Verdict::Wait(59_990));
        assert!(
            matches!(c.check(299_999), Verdict::Wait(_)),
            "never cut while it waits"
        );
        c.chunk(300_000, 4);
        // The client has had its 10 ms before the wait and gets a full window now.
        assert!(matches!(c.check(300_000), Verdict::Wait(ms) if ms >= 59_000));
        assert_eq!(c.check(360_000), Verdict::Cut);
    }

    proptest! {
        /// A client that takes at least the minimum in every window of
        /// client time is never cut (no waiting for the stream).
        #[test]
        fn a_client_above_the_floor_is_never_cut(
            window_s in 1u64..120,
            extra in proptest::collection::vec(0u64..100_000, 1..40),
        ) {
            let mut c = StallClock::new(0, policy(window_s));
            let window = window_s * 1000;
            for (k, extra) in extra.iter().enumerate() {
                let t = k as u64 * window;
                // Inside the window: the floor, plus a random amount.
                c.chunk(t + window / 2, STALL_MIN_BYTES + extra);
                prop_assert!(matches!(c.check(t + window - 1), Verdict::Wait(_)));
                prop_assert!(matches!(c.check(t + window), Verdict::Wait(_)));
            }
        }

        /// Below the floor a window is always cut, however the bytes arrive.
        #[test]
        fn a_client_below_the_floor_is_cut_in_its_first_window(
            window_s in 1u64..120,
            chunks in proptest::collection::vec((0u64..60_000, 1u64..2000), 0..30),
        ) {
            let window = window_s * 1000;
            let total: u64 = chunks.iter().map(|(_, n)| n).sum();
            prop_assume!(total < STALL_MIN_BYTES);
            let mut c = StallClock::new(0, policy(window_s));
            let mut times: Vec<_> = chunks.iter().map(|(t, n)| (t % window, *n)).collect();
            times.sort();
            for (t, n) in times { c.chunk(t, n); }
            prop_assert_eq!(c.check(window), Verdict::Cut);
        }
    }

    #[test]
    fn the_policy_watches_only_bodies_that_hold_spool() {
        assert_eq!(StallPolicy::for_body(HeldSpool::NOTHING, 60, 21_600), None);
        let held = HeldSpool::of_stream_bytes(1).at_least_one_mib();
        let p = StallPolicy::for_body(held, 30, 21_600).unwrap();
        assert_eq!(
            (p.window, p.min_bytes),
            (Duration::from_secs(30), 64 * 1024)
        );
        assert_eq!(
            StallPolicy::for_body(held, 0, 21_600).unwrap().window,
            Duration::from_secs(1)
        );
    }

    // ---- the floor grows with the held reservation ----

    const GIB_MIB: usize = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    fn held_mib(mib: usize) -> HeldSpool {
        HeldSpool::of_stream_bytes(mib as u64 * 1024 * 1024)
    }

    /// Feed a clock one chunk of `chunk` bytes every `every_ms` for `secs`
    /// seconds; the first time it says `Cut`, return that time (ms).
    fn cut_at(p: StallPolicy, chunk: u64, every_ms: u64, secs: u64) -> Option<u64> {
        let mut c = StallClock::new(0, p);
        let mut t = 0;
        while t < secs * 1000 {
            t += every_ms;
            c.chunk(t, chunk);
            if c.check(t) == Verdict::Cut {
                return Some(t);
            }
        }
        None
    }

    #[test]
    fn the_floor_of_a_window_grows_with_the_held_reservation() {
        let min = |mib, window, hold| {
            StallPolicy::for_body(held_mib(mib), window, hold)
                .unwrap()
                .min_bytes
        };
        // A tiny hold keeps the 64 KiB floor.
        assert_eq!(min(1, 60, 21_600), STALL_MIN_BYTES);
        assert_eq!(min(10, 60, 21_600), STALL_MIN_BYTES);
        // 1 GiB over 6 h: 1 GiB * 60 / 21600 per window.
        assert_eq!(min(GIB_MIB, 60, 21_600), 1024 * 1024 * 1024 * 60 / 21_600);
        // 25 GiB: 25 times that.
        assert_eq!(
            min(25 * GIB_MIB, 60, 21_600),
            25 * 1024 * 1024 * 1024 * 60 / 21_600
        );
        // A shorter max hold asks for more.
        assert!(min(GIB_MIB, 60, 3_600) > min(GIB_MIB, 60, 21_600));
    }

    #[test]
    fn a_1_gib_hold_read_at_10_kib_per_second_is_cut_and_at_100_kib_it_finishes() {
        let p = StallPolicy::for_body(held_mib(GIB_MIB), 60, 21_600).unwrap();
        // 10 KiB/s: 2.5 KiB every 250 ms. The first window ends below the floor.
        let cut = cut_at(p, 2560, 250, 3600).expect("cut");
        assert_eq!(cut, 60_000, "cut at the end of the first window");
        // 100 KiB/s for two hours is never cut.
        assert_eq!(cut_at(p, 25 * 1024, 250, 7200), None);
    }

    /// A range stream is paced by the bytes it sends, not by the whole file
    /// it holds: an 8 MiB range of a 25 GiB hold at 100 KiB/s ends in 82 s,
    /// where the whole 25 GiB at that rate is cut in the first window.
    #[test]
    fn a_range_of_a_large_hold_is_paced_by_its_own_length() {
        let held = held_mib(25 * GIB_MIB);
        let range = StallPolicy::for_body(held.sending(8 * MIB), 60, 21_600).unwrap();
        assert_eq!(range.min_bytes, STALL_MIN_BYTES);
        // 100 KiB/s (25 KiB every 250 ms) for the 82 s that 8 MiB takes.
        assert_eq!(cut_at(range, 25 * 1024, 250, 82), None);
        let whole = StallPolicy::for_body(held.sending(25 * GIB), 60, 21_600).unwrap();
        assert_eq!(cut_at(whole, 25 * 1024, 250, 3600), Some(60_000));
        // A range as long as the file is paced as the file.
        assert_eq!(whole.min_bytes, 25 * GIB * 60 / 21_600);
    }

    /// A stream that sends more than it holds (a small delta against a large
    /// reference) is paced by what it holds.
    #[test]
    fn a_stream_is_never_paced_above_what_it_holds() {
        let p = StallPolicy::for_body(held_mib(1).sending(100 * GIB), 60, 21_600).unwrap();
        assert_eq!(p.min_bytes, STALL_MIN_BYTES);
    }

    #[test]
    fn a_tiny_hold_read_at_2_kib_per_second_is_still_cut_by_the_64_kib_floor() {
        let p = StallPolicy::for_body(held_mib(1), 60, 21_600).unwrap();
        assert_eq!(cut_at(p, 100, 250, 600), Some(60_000));
        // 2 KiB/s is 120 KiB per window: above the floor of a tiny hold.
        assert_eq!(cut_at(p, 2048, 1000, 600), None);
    }

    /// A held file is released within about `max_hold` whatever the client does:
    /// the slowest client that is never cut takes the file in `max_hold`.
    #[test]
    fn the_slowest_client_that_is_never_cut_finishes_within_the_max_hold() {
        for mib in [1usize, 10, 1024, 25 * 1024] {
            let bytes = mib as u64 * 1024 * 1024;
            let p = StallPolicy::for_body(held_mib(mib), 60, 21_600).unwrap();
            let windows = bytes.div_ceil(p.min_bytes);
            let secs = windows * 60;
            // The floor is 64 KiB per 60 s at least; the file is taken in `secs`.
            assert!(
                secs <= 21_600 + 60 || p.min_bytes == STALL_MIN_BYTES,
                "{mib} MiB needs {secs} s"
            );
        }
    }

    // ---- the guard, on the paused clock of the Tokio test runtime ----

    /// A stream of `n` chunks of `len` bytes that counts its own drop.
    struct Held(Arc<AtomicUsize>);
    impl Drop for Held {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn chunks(
        n: usize,
        len: usize,
        dropped: &Arc<AtomicUsize>,
    ) -> BoxStream<'static, Result<Bytes, StorageError>> {
        let held = Held(dropped.clone());
        futures::stream::iter((0..n).map(move |i| {
            let _ = &held;
            Ok(Bytes::from(vec![i as u8; len]))
        }))
        .boxed()
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_nobody_polls_is_dropped_after_the_window() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut guard = StallGuard::new(chunks(10, 4, &dropped), Some(policy(60)));
        // The consumer takes one chunk, then stops reading.
        assert!(guard.next().await.unwrap().is_ok());
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "not before the window ends"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "the stream, and its spool, is gone"
        );
        // The client comes back: the body ends with an error, once.
        assert!(guard.next().await.unwrap().is_err());
        assert!(guard.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_never_started_is_dropped_too() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let _guard = StallGuard::new(chunks(10, 4, &dropped), Some(policy(60)));
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    /// The old guard measured the time since the last read, so one small chunk
    /// every 59 s kept a spool file for hours.
    #[tokio::test(start_paused = true)]
    async fn a_client_that_reads_one_small_chunk_per_59_seconds_is_cut() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut guard = StallGuard::new(chunks(1000, 8 * KIB, &dropped), Some(policy(60)));
        let mut got = 0;
        loop {
            match guard.next().await {
                Some(Ok(_)) => got += 1,
                Some(Err(_)) => break,
                None => panic!("the body ended: it was never cut"),
            }
            tokio::time::sleep(Duration::from_secs(59)).await;
            assert!(got < 5, "still served after {got} chunks");
        }
        assert!(got <= 2, "cut after {got} chunks");
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    /// A client at 100 KiB/s for ten minutes is served to the end.
    #[tokio::test(start_paused = true)]
    async fn a_client_at_100_kib_per_second_is_served_to_the_end() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut guard = StallGuard::new(chunks(600, 100 * KIB, &dropped), Some(policy(60)));
        let mut got = 0;
        while let Some(chunk) = guard.next().await {
            chunk.unwrap();
            got += 1;
            assert_eq!(dropped.load(Ordering::SeqCst), 0);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(got, 600);
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "released at the end of the body"
        );
    }

    /// Time that the proxy spends on storage or the codec is not the client's.
    #[tokio::test(start_paused = true)]
    async fn a_slow_source_is_no_stall() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let held = Held(dropped.clone());
        // The source needs 100 s for each chunk; the consumer waits for it.
        let slow = futures::stream::unfold(0u8, move |i| {
            let _ = &held;
            async move {
                if i == 3 {
                    return None;
                }
                tokio::time::sleep(Duration::from_secs(100)).await;
                Some((Ok(Bytes::from_static(b"x")), i + 1))
            }
        })
        .boxed();
        let mut guard = StallGuard::new(slow, Some(policy(60)));
        let mut got = 0;
        while let Some(chunk) = guard.next().await {
            chunk.unwrap();
            got += 1;
        }
        assert_eq!(got, 3);
    }

    /// A body that ends is never cut: the small body of a delta against a
    /// large reference is read in one go.
    #[tokio::test(start_paused = true)]
    async fn a_body_that_ended_is_never_cut() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut guard = StallGuard::new(chunks(3, 4, &dropped), Some(policy(60)));
        while let Some(chunk) = guard.next().await {
            chunk.unwrap();
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_secs(1000)).await;
        assert!(guard.next().await.is_none(), "no late error after the end");
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_policy_the_guard_only_wraps() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut guard = StallGuard::new(chunks(2, 4, &dropped), None);
        assert!(guard.next().await.unwrap().is_ok());
        tokio::time::sleep(Duration::from_secs(100_000)).await;
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert!(guard.next().await.unwrap().is_ok());
        assert!(guard.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_guard_ends_the_watchdog() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let guard = StallGuard::new(chunks(2, 4, &dropped), Some(policy(60)));
        drop(guard);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        // The paused clock reaches the watchdog's sleep: it returns, no panic.
        tokio::time::sleep(Duration::from_secs(120)).await;
    }
}
