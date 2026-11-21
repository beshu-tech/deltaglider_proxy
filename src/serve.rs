// SPDX-License-Identifier: BUSL-1.1

//! THE two ways the proxy serves its listener: plain HTTP and HTTPS.
//!
//! Both set `TCP_NODELAY` on every accepted connection. Without it, hyper
//! writes the response head and the body in two writes, and Nagle holds the
//! second write until the client ACKs the first. The client delays that ACK
//! (about 40 ms on Linux), so every response after the first on a
//! keep-alive connection waited about 40 ms.
//!
//! Both apply the same [`ListenerLimits`] (`DGP_HEADER_READ_TIMEOUT_SECS`,
//! `DGP_MAX_CONNECTIONS`). A client has
//! `header_read_timeout` to send the head of each request (hyper measures
//! it only when it has a timer, and neither `axum::serve` nor
//! `axum-server` gives it one): without it, a connection that sent half a
//! request stayed open for as long as the client wanted. The same timeout
//! closes a keep-alive connection that is idle between requests. And at
//! most `max_connections` connections are open at once: a connection
//! above the cap is closed at once, before the TLS handshake, and counted
//! in `deltaglider_connections_refused_total`. The request concurrency
//! cap (`DGP_MAX_CONCURRENT_REQUESTS`) limits requests in flight; it does
//! not limit connections that send no complete request. hyper has no idle
//! deadline for HTTP/2, so `Slotted` closes a connection with no request in
//! flight: after `header_read_timeout` if it got no response below status 400, after
//! `h2_idle_timeout` (`DGP_H2_IDLE_TIMEOUT_SECS`) once it has; `DGP_MAX_CONNECTIONS_PER_IP` caps one
//! peer address (an IPv6 peer by its /64; a peer in `DGP_TRUSTED_PROXY_CIDRS`
//! is not counted).
//!
//! `axum::serve` sets no timer, and the `axum-server` TLS acceptor wraps
//! `DefaultAcceptor`, which does not set nodelay (axum-server 0.7.3,
//! `accept.rs`). So both listeners go through `axum-server` with the
//! acceptor and the HTTP builder set here. Source guard:
//! `listeners_are_served_only_through_serve_rs`.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum_server::accept::{Accept, NoDelayAcceptor};
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use futures::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The time a client has to send the head of a request (hyper's own
/// default, which it applies only with a timer).
pub const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an HTTP/2 connection that has served a request stays open with
/// no request in flight. A reverse proxy keeps its idle upstream connections
/// for up to 90 s (Traefik), so this is longer: a request never races the close.
pub const DEFAULT_H2_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long an HTTP/2 peer has to answer a keep-alive ping. A ping can wait
/// behind a full socket buffer on a slow link, so this is longer than the
/// idle limit: 30 s cut live slow transfers.
pub const H2_PING_TIMEOUT: Duration = Duration::from_secs(120);

/// The least connection cap. The cap is four connections per request slot
/// (`DGP_MAX_CONCURRENT_REQUESTS`), because most connections of a client
/// pool are idle between requests, and never less than this.
pub const MIN_MAX_CONNECTIONS: usize = 4096;

/// Connections closed at accept because the listener was at its cap.
pub static CONNECTIONS_REFUSED: std::sync::LazyLock<prometheus::IntCounter> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounter::new(
            "deltaglider_connections_refused_total",
            "Connections closed at accept because the listener had its maximum of open connections",
        )
        .expect("valid metric")
    });

/// The limits of one listener (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerLimits {
    /// The time a client has to send the head of a request, and the most
    /// time a keep-alive connection stays idle between requests.
    pub header_read_timeout: Duration,
    /// The most time an HTTP/2 connection that has served a request stays
    /// open with no request in flight (`DGP_H2_IDLE_TIMEOUT_SECS`).
    pub h2_idle_timeout: Duration,
    /// The most connections open at once.
    pub max_connections: usize,
    /// The most connections open at once from one peer address.
    pub max_connections_per_ip: usize,
    /// Peers inside these networks (the reverse proxies of
    /// `DGP_TRUSTED_PROXY_CIDRS`) are not counted against the per-address cap.
    pub trusted_proxies: Arc<[ipnet::IpNet]>,
}

impl ListenerLimits {
    /// The limits for a proxy that serves `max_concurrent_requests`
    /// requests at once.
    pub fn for_request_cap(max_concurrent_requests: usize) -> Self {
        let max_connections = max_concurrent_requests
            .saturating_mul(4)
            .max(MIN_MAX_CONNECTIONS);
        Self {
            header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT,
            h2_idle_timeout: DEFAULT_H2_IDLE_TIMEOUT,
            max_connections,
            max_connections_per_ip: default_per_ip(max_connections),
            trusted_proxies: Arc::new([]),
        }
    }

    /// These limits with `proxies` as the trusted reverse-proxy networks.
    pub fn with_trusted_proxies(self, proxies: Vec<ipnet::IpNet>) -> Self {
        Self {
            trusted_proxies: proxies.into(),
            ..self
        }
    }
}

/// The default per-address cap: a quarter of the connection cap, at least 1.
fn default_per_ip(max_connections: usize) -> usize {
    (max_connections / 4).max(1)
}

impl ListenerLimits {
    /// The limits of a config snapshot: `DGP_HEADER_READ_TIMEOUT_SECS` and
    /// `DGP_MAX_CONNECTIONS` / `DGP_MAX_CONNECTIONS_PER_IP` when set, else the defaults of
    /// [`Self::for_request_cap`]. The listener reads them once, at boot.
    pub fn from_tuning(tuning: &crate::config::RuntimeTuning) -> Self {
        let derived = Self::for_request_cap(tuning.max_concurrent_requests);
        let max_connections = tuning.max_connections.unwrap_or(derived.max_connections);
        Self {
            header_read_timeout: Duration::from_secs(tuning.header_read_timeout_secs),
            h2_idle_timeout: Duration::from_secs(tuning.h2_idle_timeout_secs),
            max_connections,
            max_connections_per_ip: tuning
                .max_connections_per_ip
                .unwrap_or_else(|| default_per_ip(max_connections)),
            trusted_proxies: Arc::new([]),
        }
    }
}

impl Default for ListenerLimits {
    fn default() -> Self {
        Self::from_tuning(&crate::config::RuntimeTuning::default())
    }
}

/// How long a graceful shutdown waits for the requests in flight, on both
/// listeners. A slow download must not hold back the steps after `serve`
/// (usage flush, event-outbox flush) until the orchestrator sends SIGKILL.
pub const GRACEFUL_SHUTDOWN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Serve `app` over plain HTTP on `listener` until `shutdown` resolves,
/// then wait for the requests in flight, up to [`GRACEFUL_SHUTDOWN_DEADLINE`].
/// Middlewares see the peer address through `ConnectInfo<SocketAddr>`.
pub async fn serve_plain<F>(
    listener: TcpListener,
    app: Router,
    shutdown: F,
    limits: ListenerLimits,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let handle = axum_server::Handle::new();
    let on_shutdown = handle.clone();
    let stop = tokio::spawn(async move {
        shutdown.await;
        on_shutdown.graceful_shutdown(Some(GRACEFUL_SHUTDOWN_DEADLINE));
    });
    let mut server = axum_server::from_tcp(listener.into_std()?)
        .acceptor(Capped::new(NoDelayAcceptor::new(), limits.clone()))
        .handle(handle);
    configure(server.http_builder(), &limits);
    let served = server
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await;
    stop.abort();
    served
}

/// Serve `app` over HTTPS on `listener` until `handle` shuts it down.
/// Middlewares see the peer address through `ConnectInfo<SocketAddr>`.
pub async fn serve_tls(
    listener: TcpListener,
    config: RustlsConfig,
    handle: axum_server::Handle,
    app: Router,
    limits: ListenerLimits,
) -> io::Result<()> {
    let mut server = axum_server::from_tcp(listener.into_std()?)
        .acceptor(
            RustlsAcceptor::new(config)
                .acceptor(Capped::new(NoDelayAcceptor::new(), limits.clone())),
        )
        .handle(handle);
    configure(server.http_builder(), &limits);
    server
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
}

/// The HTTP settings that both listeners share.
fn configure(
    builder: &mut hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor>,
    limits: &ListenerLimits,
) {
    // hyper applies the header read timeout only with a timer.
    builder
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    // hyper has no HTTP/2 idle deadline (`Slotted` sets it); its ping closes
    // only a peer that stops answering (`keep_alive_while_idle: true`).
    builder
        .http2()
        .timer(hyper_util::rt::TokioTimer::new())
        .keep_alive_interval(limits.header_read_timeout)
        .keep_alive_timeout(H2_PING_TIMEOUT);
}

/// What one connection shares between its stream (`Slotted`) and its
/// service (`Tracked`): the requests in flight, since when there are none,
/// and whether it has served a request that succeeded (status below 400).
struct ConnState {
    in_flight: AtomicUsize,
    served: AtomicBool,
    idle_since: Mutex<tokio::time::Instant>,
    waker: AtomicWaker,
}

impl ConnState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            in_flight: AtomicUsize::new(0),
            served: AtomicBool::new(false),
            idle_since: Mutex::new(tokio::time::Instant::now()),
            waker: AtomicWaker::new(),
        })
    }

    /// The instant at which the connection is idle for too long, or `None`
    /// while a request is in flight. A connection that has served no
    /// request that succeeded has `first` (the header timeout); one that has, `later`.
    /// Reads of the peer never move it: a peer that sends only pings stays idle.
    fn idle_deadline(&self, first: Duration, later: Duration) -> Option<tokio::time::Instant> {
        if self.in_flight.load(Ordering::Acquire) > 0 {
            return None;
        }
        let limit = if self.served.load(Ordering::Acquire) {
            later
        } else {
            first
        };
        Some(*self.idle_since.lock().unwrap_or_else(|e| e.into_inner()) + limit)
    }
}

/// One request in flight. Dropped when its response body ends or is
/// dropped, or when the request is cancelled.
struct RequestGuard(Arc<ConnState>);

impl RequestGuard {
    fn new(state: &Arc<ConnState>) -> Self {
        state.in_flight.fetch_add(1, Ordering::AcqRel);
        Self(state.clone())
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        *self.0.idle_since.lock().unwrap_or_else(|e| e.into_inner()) = tokio::time::Instant::now();
        if self.0.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The stream asks again and starts the idle clock.
            self.0.waker.wake();
        }
    }
}

/// A service that counts its requests in flight, through the end of each
/// response body.
#[derive(Clone)]
struct Tracked<S> {
    inner: S,
    state: Arc<ConnState>,
}

impl<S, Req, B> tower::Service<Req> for Tracked<S>
where
    S: tower::Service<Req, Response = axum::http::Response<B>>,
    S::Future: Send + 'static,
    B: http_body::Body + Unpin,
{
    type Response = axum::http::Response<GuardedBody<B>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let guard = RequestGuard::new(&self.state);
        let fut = self.inner.call(req);
        Box::pin(async move {
            let response = fut.await?;
            // A refusal proves no real client: only a status below 400 moves the
            // connection from the header limit to the longer idle limit.
            if response.status().as_u16() < 400 {
                guard.0.served.store(true, Ordering::Release);
            }
            Ok(response.map(|body| GuardedBody {
                body,
                _guard: guard,
            }))
        })
    }
}

/// A response body that keeps its request in flight until it is dropped.
struct GuardedBody<B> {
    body: B,
    _guard: RequestGuard,
}

impl<B: http_body::Body + Unpin> http_body::Body for GuardedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// The open connections of each peer address. An entry exists only while
/// its address has a connection open, so the map is never larger than the
/// listener's connection cap.
#[derive(Default)]
struct PeerCounts(Mutex<HashMap<IpAddr, usize>>);

/// One connection counted against its peer address (`None`: a trusted
/// reverse proxy, which is not counted).
struct PeerSlot(Option<(Arc<PeerCounts>, IpAddr)>);

impl PeerCounts {
    fn try_acquire(
        self: &Arc<Self>,
        ip: IpAddr,
        cap: usize,
        trusted: &[ipnet::IpNet],
    ) -> Option<PeerSlot> {
        // Every client of a reverse proxy has its address: no per-address
        // cap there (the listener cap still holds).
        if trusted.iter().any(|net| net.contains(&ip.to_canonical())) {
            return Some(PeerSlot(None));
        }
        let ip = crate::security::peer_bucket(ip);
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let n = map.entry(ip).or_insert(0);
        if *n >= cap {
            return None;
        }
        *n += 1;
        Some(PeerSlot(Some((self.clone(), ip))))
    }
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        let Some((counts, ip)) = self.0.take() else {
            return;
        };
        let mut map = counts.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&ip) {
            *n -= 1;
            if *n == 0 {
                map.remove(&ip);
            }
        }
    }
}

/// An acceptor that holds one connection slot for the life of each
/// connection, and closes a connection that finds no slot free (in the
/// listener, or for its peer address). It runs before the inner acceptor
/// (so before a TLS handshake).
#[derive(Clone)]
struct Capped<A> {
    inner: A,
    slots: Arc<Semaphore>,
    peers: Arc<PeerCounts>,
    per_ip: usize,
    idle_limit: Duration,
    h2_idle_limit: Duration,
    trusted: Arc<[ipnet::IpNet]>,
}

impl<A> Capped<A> {
    fn new(inner: A, limits: ListenerLimits) -> Self {
        let trusted = limits.trusted_proxies.clone();
        Self {
            inner,
            slots: Arc::new(Semaphore::new(limits.max_connections.max(1))),
            peers: Arc::new(PeerCounts::default()),
            per_ip: limits.max_connections_per_ip.max(1),
            idle_limit: limits.header_read_timeout,
            h2_idle_limit: limits.h2_idle_timeout,
            trusted,
        }
    }
}

impl<A, S> Accept<TcpStream, S> for Capped<A>
where
    A: Accept<TcpStream, S> + Clone + Send + 'static,
    A::Future: Send,
    A::Service: Send + 'static,
    S: Send + 'static,
{
    type Stream = Slotted<A::Stream>;
    type Service = Tracked<A::Service>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let slot = self.slots.clone().try_acquire_owned();
        // The TCP peer address, never a forwarded header.
        let peer = stream
            .peer_addr()
            .ok()
            .and_then(|a| self.peers.try_acquire(a.ip(), self.per_ip, &self.trusted));
        let inner = self.inner.clone();
        let (idle_limit, h2_idle_limit) = (self.idle_limit, self.h2_idle_limit);
        Box::pin(async move {
            let (Ok(slot), Some(peer)) = (slot, peer) else {
                CONNECTIONS_REFUSED.inc();
                // Dropping the stream closes the connection.
                return Err(io::Error::other(
                    "the listener has its maximum of open connections",
                ));
            };
            let (stream, service) = inner.accept(stream, service).await?;
            let state = ConnState::new();
            Ok((
                Slotted {
                    inner: stream,
                    state: state.clone(),
                    idle_limit,
                    h2_idle_limit,
                    idle: None,
                    _slot: slot,
                    _peer: peer,
                },
                Tracked {
                    inner: service,
                    state,
                },
            ))
        })
    }
}

/// A connection stream that holds its connection slot until it is dropped,
/// and fails its reads once the connection has had no request in flight for
/// `idle_limit` (`h2_idle_limit` once it has served a request); hyper has no
/// such deadline for HTTP/2.
struct Slotted<T> {
    inner: T,
    state: Arc<ConnState>,
    idle_limit: Duration,
    h2_idle_limit: Duration,
    idle: Option<Pin<Box<tokio::time::Sleep>>>,
    _slot: OwnedSemaphorePermit,
    _peer: PeerSlot,
}

impl<T: AsyncRead + Unpin> AsyncRead for Slotted<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        // Register before the check: a request that ends in between wakes us.
        this.state.waker.register(cx.waker());
        if let Some(deadline) = this
            .state
            .idle_deadline(this.idle_limit, this.h2_idle_limit)
        {
            let sleep = this
                .idle
                .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
            sleep.as_mut().reset(deadline);
            if sleep.as_mut().poll(cx).is_ready() {
                // No GOAWAY: axum-server 0.7.3 holds the connection future
                // (`server.rs:202`) and sends one only on a listener shutdown.
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the connection was idle for too long",
                )));
            }
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Slotted<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::routing::get;
    use std::collections::HashSet;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const REQUESTS: usize = 10;
    /// Ten responses on one connection. With nodelay each takes about 1 ms
    /// on loopback (the body waits 1 ms); without it each waits for the
    /// client's delayed ACK (about 40 ms: measured 418 ms in total).
    const BUDGET: Duration = Duration::from_millis(150);

    /// A 4 KiB body with a known length, sent as a stream.
    fn app(peers: Arc<Mutex<HashSet<SocketAddr>>>) -> Router {
        Router::new().route(
            "/",
            get(move |ConnectInfo(peer): ConnectInfo<SocketAddr>| {
                peers.lock().unwrap().insert(peer);
                async move {
                    // The body is not ready when hyper flushes the head (as
                    // with an object streamed from storage), so the head
                    // and the body go out in two writes.
                    let stream = futures::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        Ok::<_, io::Error>(bytes::Bytes::from(vec![b'x'; BODY]))
                    });
                    (
                        [("content-length", BODY.to_string())],
                        Body::from_stream(stream),
                    )
                }
            }),
        )
    }

    const BODY: usize = 4096;

    /// One GET on `conn`; reads the whole response (head + body).
    fn get_once(conn: &mut impl ReadWrite) {
        conn.write_all(b"GET / HTTP/1.1\r\nhost: test\r\n\r\n")
            .unwrap();
        conn.flush().unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            conn.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 200"), "{head}");
        assert!(head.contains(&format!("content-length: {BODY}")), "{head}");
        let mut body = vec![0u8; BODY];
        conn.read_exact(&mut body).unwrap();
    }

    trait ReadWrite: Read + Write {}
    impl<T: Read + Write> ReadWrite for T {}

    /// A warm-up GET (opens the connection, handshakes), then `REQUESTS`
    /// GETs one after the other on the SAME connection; returns their time.
    fn timed_gets(conn: &mut impl ReadWrite) -> Duration {
        get_once(conn);
        let start = Instant::now();
        for _ in 0..REQUESTS {
            get_once(conn);
        }
        start.elapsed()
    }

    fn connect(addr: SocketAddr) -> std::net::TcpStream {
        let tcp = std::net::TcpStream::connect(addr).unwrap();
        // Client side off the table: only the server's socket option counts.
        tcp.set_nodelay(true).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        tcp
    }

    fn assert_fast(elapsed: Duration, peers: &Mutex<HashSet<SocketAddr>>, listener: &str) {
        assert_eq!(peers.lock().unwrap().len(), 1, "one keep-alive connection");
        assert!(
            elapsed < BUDGET,
            "{REQUESTS} keep-alive GETs took {elapsed:?} (budget {BUDGET:?}): \
             TCP_NODELAY is off on the {listener} listener"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plain_keep_alive_responses_are_not_held_by_nagle() {
        let peers = Arc::new(Mutex::new(HashSet::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_plain(
            listener,
            app(peers.clone()),
            std::future::pending(),
            ListenerLimits::default(),
        ));

        let elapsed = tokio::task::spawn_blocking(move || timed_gets(&mut connect(addr)))
            .await
            .unwrap();
        server.abort();
        assert_fast(elapsed, &peers, "plain");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_keep_alive_responses_are_not_held_by_nagle() {
        crate::tls::install_crypto_provider();
        let tls = crate::config::TlsConfig {
            enabled: true,
            cert_path: None,
            key_path: None,
        };
        let config = crate::tls::build_rustls_config(&tls).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peers = Arc::new(Mutex::new(HashSet::new()));
        let handle = axum_server::Handle::new();
        let server = tokio::spawn(serve_tls(
            listener,
            config,
            handle.clone(),
            app(peers.clone()),
            ListenerLimits::default(),
        ));

        let elapsed = tokio::task::spawn_blocking(move || {
            let client = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyCert))
                .with_no_client_auth();
            let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let conn = rustls::ClientConnection::new(Arc::new(client), name).unwrap();
            timed_gets(&mut rustls::StreamOwned::new(conn, connect(addr)))
        })
        .await
        .unwrap();
        handle.shutdown();
        let _ = server.await;
        assert_fast(elapsed, &peers, "TLS");
    }

    /// Limits with a short header timeout, for the tests.
    fn short(header_secs: u64, max_connections: usize) -> ListenerLimits {
        ListenerLimits {
            header_read_timeout: Duration::from_secs(header_secs),
            h2_idle_timeout: Duration::from_secs(header_secs),
            max_connections,
            max_connections_per_ip: max_connections,
            trusted_proxies: Arc::new([]),
        }
    }

    /// Read until the server closes `conn`; how long that took. A test
    /// read timeout ends it (and fails the test) when the server never
    /// closes it.
    fn time_to_close(conn: &mut impl Read) -> Duration {
        let start = Instant::now();
        let mut buf = [0u8; 1024];
        loop {
            match conn.read(&mut buf) {
                Ok(0) => return start.elapsed(),
                Ok(_) => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    panic!(
                        "the server kept the connection open for {:?}",
                        start.elapsed()
                    )
                }
                // A reset is a close too.
                Err(_) => return start.elapsed(),
            }
        }
    }

    const HALF_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nhost: test\r\nx-slow: ";

    /// A client that sends half a request head and then nothing held its
    /// connection (and a task) for as long as it liked: measured 95 s
    /// live. With the header timeout, the server closes it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plain_closes_a_half_sent_request_after_the_header_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_plain(
            listener,
            app(Default::default()),
            std::future::pending(),
            short(1, 16),
        ));
        let took = tokio::task::spawn_blocking(move || {
            let mut conn = connect(addr);
            conn.write_all(HALF_REQUEST).unwrap();
            time_to_close(&mut conn)
        })
        .await
        .unwrap();
        server.abort();
        assert!(took < Duration::from_secs(5), "closed after {took:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_closes_a_half_sent_request_after_the_header_timeout() {
        let (addr, handle, server) = tls_server(short(1, 16)).await;
        let took = tokio::task::spawn_blocking(move || {
            let mut conn = tls_connect(addr);
            conn.write_all(HALF_REQUEST).unwrap();
            conn.flush().unwrap();
            time_to_close(&mut conn)
        })
        .await
        .unwrap();
        handle.shutdown();
        let _ = server.await;
        assert!(took < Duration::from_secs(5), "closed after {took:?}");
    }

    /// Open connections were not capped: each idle or half-sent connection
    /// holds a socket and a task. Above the cap, a new connection is closed
    /// at once; a slot frees when its connection closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connections_above_the_cap_are_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_plain(
            listener,
            app(Default::default()),
            std::future::pending(),
            short(30, 1),
        ));
        let refused_before = CONNECTIONS_REFUSED.get();
        tokio::task::spawn_blocking(move || {
            let mut first = connect(addr);
            get_once(&mut first);
            let mut second = connect(addr);
            let _ = second.write_all(b"GET / HTTP/1.1\r\nhost: test\r\n\r\n");
            assert!(
                time_to_close(&mut second) < Duration::from_secs(5),
                "the connection above the cap was served"
            );
            // The first one is still served.
            get_once(&mut first);
            drop(first);
            // Its slot frees when the server sees the close.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mut third = connect(addr);
                third
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let served = third
                    .write_all(b"GET / HTTP/1.1\r\nhost: test\r\n\r\n")
                    .is_ok()
                    && {
                        let mut byte = [0u8; 1];
                        matches!(third.read(&mut byte), Ok(1))
                    };
                if served {
                    break;
                }
                assert!(Instant::now() < deadline, "the slot was never freed");
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .await
        .unwrap();
        server.abort();
        assert!(CONNECTIONS_REFUSED.get() > refused_before, "not counted");
    }

    /// The HTTP/2 client preface and an empty SETTINGS frame: all that an
    /// idle-connection flood sends.
    const H2_HELLO: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00";

    fn h2_app() -> Router {
        Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(2500)).await;
                    "done"
                }),
            )
            .route("/fast", get(|| async { "ok" }))
            .route(
                "/refused",
                get(|| async { (axum::http::StatusCode::FORBIDDEN, "no") }),
            )
            .route(
                "/hold",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "done"
                }),
            )
    }

    async fn serve_h2_app(
        limits: ListenerLimits,
    ) -> (SocketAddr, tokio::task::JoinHandle<io::Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_plain(
            listener,
            h2_app(),
            std::future::pending(),
            limits,
        ));
        (addr, server)
    }

    /// An HTTP/2 connection that sends only the preface has no request in
    /// flight. hyper has no idle deadline for HTTP/2, so it stayed open
    /// for as long as the client wanted and held a connection slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plain_closes_an_idle_h2_connection_after_the_idle_limit() {
        let (addr, server) = serve_h2_app(short(1, 16)).await;
        let took = tokio::task::spawn_blocking(move || {
            let mut conn = connect(addr);
            conn.write_all(H2_HELLO).unwrap();
            time_to_close(&mut conn)
        })
        .await
        .unwrap();
        server.abort();
        assert!(took < Duration::from_secs(5), "closed after {took:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_closes_an_idle_h2_connection_after_the_idle_limit() {
        let (addr, handle, server) = tls_server(short(1, 16)).await;
        let took = tokio::task::spawn_blocking(move || {
            let mut conn = tls_connect(addr);
            conn.write_all(H2_HELLO).unwrap();
            conn.flush().unwrap();
            time_to_close(&mut conn)
        })
        .await
        .unwrap();
        handle.shutdown();
        let _ = server.await;
        assert!(took < Duration::from_secs(5), "closed after {took:?}");
    }

    /// A request in flight is never idle: one that runs 2.5 s survives a
    /// 1 s idle limit. When it ends, the connection is idle again and its
    /// slot frees (cap 1: a second connection is served only then).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_h2_request_in_flight_is_not_cut_and_its_connection_idles_out_after() {
        let (addr, server) = serve_h2_app(short(1, 1)).await;
        let client = reqwest::Client::builder()
            .http2_prior_knowledge()
            .build()
            .unwrap();
        let body = client
            .get(format!("http://{addr}/slow"))
            .send()
            .await
            .expect("the request in flight was cut")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "done");
        // The client keeps its connection pooled and answers pings; only
        // the server's idle limit can free the slot.
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let served = tokio::task::spawn_blocking(move || {
                let mut c = connect(addr);
                c.set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let _ = c.write_all(b"GET /slow HTTP/1.1\r\nhost: test\r\n\r\n");
                let mut byte = [0u8; 1];
                // Served: the handler is still asleep, so the read times
                // out. A refused connection is closed (EOF or reset).
                matches!(
                    c.read(&mut byte),
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
                )
            })
            .await
            .unwrap();
            if served {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the idle h2 connection kept its slot"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        drop(client);
        server.abort();
    }

    /// The same limits with a longer HTTP/2 idle timeout.
    fn with_h2_idle(limits: ListenerLimits, secs: u64) -> ListenerLimits {
        ListenerLimits {
            h2_idle_timeout: Duration::from_secs(secs),
            ..limits
        }
    }

    /// A connection that only sent the HTTP/2 preface is closed by the
    /// header timeout (1 s), not by the longer HTTP/2 idle timeout (60 s):
    /// the slot-holding flood stays closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_preface_only_h2_connection_closes_at_the_header_timeout() {
        let (addr, server) = serve_h2_app(with_h2_idle(short(1, 16), 60)).await;
        let took = tokio::task::spawn_blocking(move || {
            let mut conn = connect(addr);
            conn.write_all(H2_HELLO).unwrap();
            time_to_close(&mut conn)
        })
        .await
        .unwrap();
        server.abort();
        assert!(took < Duration::from_secs(5), "closed after {took:?}");
    }

    /// HTTP/1.1 keeps its behaviour: a keep-alive connection that is idle
    /// between requests closes at the header timeout, whatever the HTTP/2
    /// idle timeout is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_h1_keep_alive_connection_still_closes_at_the_header_timeout() {
        let (addr, server) = serve_h2_app(with_h2_idle(short(1, 16), 60)).await;
        let took = tokio::task::spawn_blocking(move || {
            let mut conn = connect(addr);
            conn.write_all(b"GET /fast HTTP/1.1\r\nhost: test\r\n\r\n")
                .unwrap();
            time_to_close(&mut conn)
        })
        .await
        .unwrap();
        server.abort();
        assert!(took < Duration::from_secs(5), "closed after {took:?}");
    }

    /// A reverse proxy keeps an idle upstream connection longer than the
    /// header timeout and sends its next request on it. After a request, an
    /// HTTP/2 connection stays open for the HTTP/2 idle timeout (4 s here),
    /// not for the header timeout (1 s); then it frees its slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_h2_connection_that_sent_a_request_idles_for_the_h2_timeout() {
        let (addr, server) = serve_h2_app(with_h2_idle(short(1, 1), 4)).await;
        let client = reqwest::Client::builder()
            .http2_prior_knowledge()
            .build()
            .unwrap();
        let body = client
            .get(format!("http://{addr}/fast"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");
        let done = Instant::now();
        // The cap is 1 connection: a second one is served only once the
        // idle h2 connection has freed its slot.
        let freed_after = loop {
            let served = tokio::task::spawn_blocking(move || {
                let mut c = connect(addr);
                c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
                let _ = c.write_all(b"GET /slow HTTP/1.1\r\nhost: test\r\n\r\n");
                let mut byte = [0u8; 1];
                matches!(
                    c.read(&mut byte),
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
                )
            })
            .await
            .unwrap();
            if served {
                break done.elapsed();
            }
            assert!(
                done.elapsed() < Duration::from_secs(9),
                "the idle h2 connection kept its slot"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        drop(client);
        server.abort();
        assert!(
            freed_after >= Duration::from_secs(3),
            "an h2 connection that had served a request was closed after {freed_after:?}"
        );
    }

    /// A request that gets a refusal (status 400 or more) is no proof of a
    /// real client: the HTTP/2 connection that sent only such requests
    /// keeps the header limit (1 s here), not the idle limit (4 s).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_h2_connection_that_got_only_refusals_keeps_the_header_limit() {
        let (addr, server) = serve_h2_app(with_h2_idle(short(1, 1), 4)).await;
        let client = reqwest::Client::builder()
            .http2_prior_knowledge()
            .build()
            .unwrap();
        let status = client
            .get(format!("http://{addr}/refused"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 403);
        let done = Instant::now();
        let freed_after = loop {
            let served = tokio::task::spawn_blocking(move || {
                let mut c = connect(addr);
                c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
                let _ = c.write_all(b"GET /slow HTTP/1.1\r\nhost: test\r\n\r\n");
                let mut byte = [0u8; 1];
                matches!(
                    c.read(&mut byte),
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
                )
            })
            .await
            .unwrap();
            if served {
                break done.elapsed();
            }
            assert!(
                done.elapsed() < Duration::from_secs(9),
                "the connection kept its slot"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        drop(client);
        server.abort();
        assert!(
            freed_after < Duration::from_secs(3),
            "a refused h2 connection idled for {freed_after:?}"
        );
    }

    /// `GET /hold` as one HTTP/2 HEADERS frame (stream 1, END_STREAM): the
    /// HPACK block is `:method GET`, `:scheme http`, `:path /hold`,
    /// `:authority test`.
    const H2_GET_HOLD: &[u8] = b"\x00\x00\x0f\x01\x05\x00\x00\x00\x01\
        \x82\x86\x44\x05/hold\x41\x04test";

    /// A slow link can leave a ping waiting behind a full socket buffer, so a
    /// peer with a request in flight gets 120 s to answer a ping, not the
    /// 30 s of the idle limit. This peer never answers: with a ping timeout
    /// equal to the 1 s idle limit, the server cut it at about 2 s.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_peer_with_a_request_in_flight_has_a_long_time_to_answer_a_ping() {
        let (addr, server) = serve_h2_app(short(1, 16)).await;
        let outcome = tokio::task::spawn_blocking(move || {
            let mut conn = connect(addr);
            conn.set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            conn.write_all(H2_HELLO).unwrap();
            conn.write_all(H2_GET_HOLD).unwrap();
            let start = Instant::now();
            let mut buf = [0u8; 1024];
            while start.elapsed() < Duration::from_secs(5) {
                match conn.read(&mut buf) {
                    Ok(0) => return Err(start.elapsed()),
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => return Err(start.elapsed()),
                }
            }
            Ok(())
        })
        .await
        .unwrap();
        server.abort();
        assert_eq!(
            outcome,
            Ok(()),
            "the server cut a peer that had a request in flight"
        );
    }

    /// One address cannot take every slot: a second connection from the
    /// same peer address is closed at once while the cap is 1, though the
    /// listener has room. A slot frees when the connection closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connections_above_the_per_address_cap_are_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let limits = ListenerLimits {
            max_connections_per_ip: 1,
            ..short(30, 16)
        };
        let server = tokio::spawn(serve_plain(
            listener,
            app(Default::default()),
            std::future::pending(),
            limits,
        ));
        let refused_before = CONNECTIONS_REFUSED.get();
        tokio::task::spawn_blocking(move || {
            let mut first = connect(addr);
            get_once(&mut first);
            let mut second = connect(addr);
            let _ = second.write_all(b"GET / HTTP/1.1\r\nhost: test\r\n\r\n");
            assert!(
                time_to_close(&mut second) < Duration::from_secs(5),
                "a second connection of the same address was served"
            );
            get_once(&mut first);
            drop(first);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mut third = connect(addr);
                third
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let served = third
                    .write_all(b"GET / HTTP/1.1\r\nhost: test\r\n\r\n")
                    .is_ok()
                    && matches!(third.read(&mut [0u8; 1]), Ok(1));
                if served {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the address slot was never freed"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .await
        .unwrap();
        server.abort();
        assert!(CONNECTIONS_REFUSED.get() > refused_before, "not counted");
    }

    /// A peer inside `DGP_TRUSTED_PROXY_CIDRS` is a reverse proxy: every
    /// client arrives from its address, so it is not counted against the
    /// per-address cap. The cap of the whole listener still applies.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_trusted_proxy_peer_is_not_capped_per_address() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let limits = ListenerLimits {
            max_connections_per_ip: 1,
            ..short(30, 3)
        }
        .with_trusted_proxies(vec!["127.0.0.1/32".parse().unwrap()]);
        let server = tokio::spawn(serve_plain(
            listener,
            app(Default::default()),
            std::future::pending(),
            limits,
        ));
        tokio::task::spawn_blocking(move || {
            let mut open: Vec<_> = (0..3).map(|_| connect(addr)).collect();
            for conn in &mut open {
                get_once(conn);
            }
            // The listener cap (3) is the one that closes the fourth.
            let mut fourth = connect(addr);
            let _ = fourth.write_all(b"GET / HTTP/1.1\r\nhost: test\r\n\r\n");
            assert!(
                time_to_close(&mut fourth) < Duration::from_secs(5),
                "the listener cap did not apply to a trusted peer"
            );
        })
        .await
        .unwrap();
        server.abort();
    }

    /// Hosts of one IPv6 /64 share one count: a client with a whole /64
    /// has a new address per connection.
    #[test]
    fn hosts_of_one_ipv6_64_share_the_cap() {
        let counts = Arc::new(PeerCounts::default());
        let host = |n: u16| -> IpAddr { format!("2001:db8:1:2::{n:x}").parse().unwrap() };
        let held: Vec<_> = (1..=2)
            .map(|n| counts.try_acquire(host(n), 2, &[]).unwrap())
            .collect();
        assert!(counts.try_acquire(host(3), 2, &[]).is_none());
        drop(held);
        assert!(counts.0.lock().unwrap().is_empty());
    }

    /// The map holds one entry per address with a connection open, so it is
    /// never larger than the connections: it empties as they close, and a
    /// refused connection leaves no entry.
    #[test]
    fn the_peer_counts_hold_only_open_connections() {
        let counts = Arc::new(PeerCounts::default());
        let mut held = Vec::new();
        for n in 0..5_000u32 {
            let ip: IpAddr = format!("2001:db8:{:x}:{:x}::1", n >> 8, n & 0xff)
                .parse()
                .unwrap();
            held.push(counts.try_acquire(ip, 1, &[]).unwrap());
            assert!(counts.try_acquire(ip, 1, &[]).is_none());
        }
        assert_eq!(counts.0.lock().unwrap().len(), 5_000);
        drop(held);
        assert!(counts.0.lock().unwrap().is_empty(), "an entry leaked");
    }

    #[test]
    fn a_trusted_proxy_peer_is_never_counted() {
        let counts = Arc::new(PeerCounts::default());
        let net: ipnet::IpNet = "10.0.0.0/8".parse().unwrap();
        let ip: IpAddr = "10.1.2.3".parse().unwrap();
        let held: Vec<_> = (0..10)
            .map(|_| counts.try_acquire(ip, 1, &[net]).expect("trusted"))
            .collect();
        assert!(counts.0.lock().unwrap().is_empty());
        drop(held);
        // An address outside the network is capped as before.
        let other: IpAddr = "11.1.2.3".parse().unwrap();
        let _one = counts.try_acquire(other, 1, &[net]).unwrap();
        assert!(counts.try_acquire(other, 1, &[net]).is_none());
    }

    /// An acceptor that passes the stream on, or fails.
    #[derive(Clone)]
    struct Pass(bool);

    impl Accept<TcpStream, ()> for Pass {
        type Stream = TcpStream;
        type Service = ();
        type Future = std::future::Ready<io::Result<(TcpStream, ())>>;

        fn accept(&self, stream: TcpStream, _: ()) -> Self::Future {
            std::future::ready(if self.0 {
                Ok((stream, ()))
            } else {
                Err(io::Error::other("the handshake failed"))
            })
        }
    }

    /// The server side of a loopback connection.
    async fn accepted() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (server, client)
    }

    /// Every way a connection can end gives back its listener slot and its
    /// address count: a failed inner accept, a refusal by the other cap, a
    /// future dropped before it ran, and a connection that was served.
    #[tokio::test]
    async fn every_exit_path_releases_the_slot_and_the_address_count() {
        let limits = ListenerLimits {
            max_connections_per_ip: 8,
            ..short(30, 2)
        };
        let free = |c: &Capped<Pass>| {
            (
                c.slots.available_permits(),
                c.peers.0.lock().unwrap().values().sum::<usize>(),
            )
        };
        // Inner accept fails (a TLS handshake that fails).
        let capped = Capped::new(Pass(false), limits.clone());
        let (server, _client) = accepted().await;
        assert!(capped.accept(server, ()).await.is_err());
        assert_eq!(free(&capped), (2, 0), "after a failed inner accept");
        // A future that is dropped before it runs.
        let capped = Capped::new(Pass(true), limits.clone());
        let (server, _client) = accepted().await;
        drop(capped.accept(server, ()));
        assert_eq!(free(&capped), (2, 0), "after a dropped future");
        // Served, then closed.
        let (server, _client) = accepted().await;
        let (stream, service) = capped.accept(server, ()).await.unwrap();
        assert_eq!(free(&capped), (1, 1), "while served");
        drop((stream, service));
        assert_eq!(free(&capped), (2, 0), "after a served connection");
        // Refused: the listener is full; the address count taken for it is
        // given back with the refusal.
        let one = Capped::new(Pass(true), short(30, 1));
        let (a, _ca) = accepted().await;
        let held = one.accept(a, ()).await.unwrap();
        let (b, _cb) = accepted().await;
        assert!(one.accept(b, ()).await.is_err());
        assert_eq!(free(&one), (0, 1), "after a refusal by the listener cap");
        drop(held);
        assert_eq!(free(&one), (1, 0));
    }

    #[test]
    fn the_connection_cap_follows_the_request_cap() {
        assert_eq!(
            ListenerLimits::for_request_cap(1024).max_connections,
            MIN_MAX_CONNECTIONS
        );
        assert_eq!(
            ListenerLimits::for_request_cap(4096).max_connections,
            16_384
        );
        assert_eq!(
            ListenerLimits::default().header_read_timeout,
            Duration::from_secs(30)
        );
    }

    /// `DGP_HEADER_READ_TIMEOUT_SECS` and `DGP_MAX_CONNECTIONS` set the
    /// limits; unset, they keep the defaults (30 s; 4 x the request cap,
    /// at least 4096). A zero value is raised to 1.
    #[test]
    fn the_listener_limits_come_from_the_environment() {
        let limits = |pairs: &[(&str, &str)]| {
            let map: std::collections::HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            ListenerLimits::from_tuning(&crate::config::RuntimeTuning::from_env(&move |n: &str| {
                map.get(n).cloned()
            }))
        };
        assert_eq!(limits(&[]), ListenerLimits::for_request_cap(1024));
        assert_eq!(
            limits(&[("DGP_MAX_CONCURRENT_REQUESTS", "4096")]).max_connections,
            16_384
        );
        let set = limits(&[
            ("DGP_HEADER_READ_TIMEOUT_SECS", "5"),
            ("DGP_MAX_CONNECTIONS", "100"),
        ]);
        assert_eq!(set.header_read_timeout, Duration::from_secs(5));
        assert_eq!(set.max_connections, 100);
        assert_eq!(set.max_connections_per_ip, 25, "a quarter of the cap");
        assert_eq!(limits(&[]).max_connections_per_ip, 1024);
        assert_eq!(
            limits(&[("DGP_MAX_CONNECTIONS_PER_IP", "7")]).max_connections_per_ip,
            7
        );
        assert_eq!(
            limits(&[("DGP_MAX_CONNECTIONS", "2")]).max_connections_per_ip,
            1,
            "at least 1"
        );
        let zero = limits(&[
            ("DGP_HEADER_READ_TIMEOUT_SECS", "0"),
            ("DGP_MAX_CONNECTIONS", "0"),
        ]);
        assert_eq!(zero.header_read_timeout, Duration::from_secs(1));
        assert_eq!(zero.max_connections, 1);
        assert_eq!(
            limits(&[]).h2_idle_timeout,
            Duration::from_secs(120),
            "the default"
        );
        assert_eq!(
            limits(&[("DGP_H2_IDLE_TIMEOUT_SECS", "200")]).h2_idle_timeout,
            Duration::from_secs(200)
        );
        assert_eq!(
            limits(&[("DGP_H2_IDLE_TIMEOUT_SECS", "0")]).h2_idle_timeout,
            Duration::from_secs(1),
            "a zero counts as 1"
        );
        let zero_ip = limits(&[("DGP_MAX_CONNECTIONS_PER_IP", "0")]);
        assert_eq!(zero_ip.max_connections_per_ip, 1);
    }

    async fn tls_server(
        limits: ListenerLimits,
    ) -> (
        SocketAddr,
        axum_server::Handle,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        crate::tls::install_crypto_provider();
        let tls = crate::config::TlsConfig {
            enabled: true,
            cert_path: None,
            key_path: None,
        };
        let config = crate::tls::build_rustls_config(&tls).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = axum_server::Handle::new();
        let server = tokio::spawn(serve_tls(
            listener,
            config,
            handle.clone(),
            app(Default::default()),
            limits,
        ));
        (addr, handle, server)
    }

    fn tls_connect(
        addr: SocketAddr,
    ) -> rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream> {
        let client = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert))
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let conn = rustls::ClientConnection::new(Arc::new(client), name).unwrap();
        rustls::StreamOwned::new(conn, connect(addr))
    }

    /// Accepts the self-signed test certificate: the test measures latency,
    /// not trust.
    #[derive(Debug)]
    struct AnyCert;

    impl rustls::client::danger::ServerCertVerifier for AnyCert {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// Every listener goes through this module, so none can lose nodelay.
    #[test]
    fn listeners_are_served_only_through_serve_rs() {
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            if src.rel == "src/serve.rs" {
                continue;
            }
            // `axum::serve`, `axum_server::bind`, `axum_server::from_tcp`,
            // under any path, `use` or alias; the old scan wanted that text
            // on one line.
            for p in &src.prod.paths {
                let n = p.segs.len();
                let hit = n >= 2
                    && ((p.segs[n - 2] == "axum" && p.segs[n - 1] == "serve")
                        || (p.segs[n - 2] == "axum_server"
                            && matches!(p.segs[n - 1].as_str(), "bind" | "from_tcp")));
                if hit {
                    offenders.push(format!("{}:{}: {}", src.rel, p.line, p.segs.join("::")));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "serve listeners through crate::serve (serve_plain / serve_tls), \
             which set TCP_NODELAY:\n{}",
            offenders.join("\n")
        );
    }
}
