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
//! not limit connections that send no complete request.
//!
//! `axum::serve` sets no timer, and the `axum-server` TLS acceptor wraps
//! `DefaultAcceptor`, which does not set nodelay (axum-server 0.7.3,
//! `accept.rs`). So both listeners go through `axum-server` with the
//! acceptor and the HTTP builder set here. Source guard:
//! `listeners_are_served_only_through_serve_rs`.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum_server::accept::{Accept, NoDelayAcceptor};
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The time a client has to send the head of a request (hyper's own
/// default, which it applies only with a timer).
pub const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenerLimits {
    /// The time a client has to send the head of a request, and the most
    /// time a keep-alive connection stays idle between requests.
    pub header_read_timeout: Duration,
    /// The most connections open at once.
    pub max_connections: usize,
}

impl ListenerLimits {
    /// The limits for a proxy that serves `max_concurrent_requests`
    /// requests at once.
    pub fn for_request_cap(max_concurrent_requests: usize) -> Self {
        Self {
            header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT,
            max_connections: max_concurrent_requests
                .saturating_mul(4)
                .max(MIN_MAX_CONNECTIONS),
        }
    }
}

impl ListenerLimits {
    /// The limits of a config snapshot: `DGP_HEADER_READ_TIMEOUT_SECS` and
    /// `DGP_MAX_CONNECTIONS` when set, else the defaults of
    /// [`Self::for_request_cap`]. The listener reads them once, at boot.
    pub fn from_tuning(tuning: &crate::config::RuntimeTuning) -> Self {
        let derived = Self::for_request_cap(tuning.max_concurrent_requests);
        Self {
            header_read_timeout: Duration::from_secs(tuning.header_read_timeout_secs),
            max_connections: tuning.max_connections.unwrap_or(derived.max_connections),
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
        .acceptor(Capped::new(NoDelayAcceptor::new(), limits))
        .handle(handle);
    configure(server.http_builder(), limits);
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
        .acceptor(RustlsAcceptor::new(config).acceptor(Capped::new(NoDelayAcceptor::new(), limits)))
        .handle(handle);
    configure(server.http_builder(), limits);
    server
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
}

/// The HTTP settings that both listeners share.
fn configure(
    builder: &mut hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor>,
    limits: ListenerLimits,
) {
    // hyper applies the header read timeout only with a timer.
    builder
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
}

/// An acceptor that holds one connection slot for the life of each
/// connection, and closes a connection that finds no slot free. It runs
/// before the inner acceptor (so before a TLS handshake).
#[derive(Clone)]
struct Capped<A> {
    inner: A,
    slots: Arc<Semaphore>,
}

impl<A> Capped<A> {
    fn new(inner: A, limits: ListenerLimits) -> Self {
        Self {
            inner,
            slots: Arc::new(Semaphore::new(limits.max_connections.max(1))),
        }
    }
}

impl<A, S> Accept<TcpStream, S> for Capped<A>
where
    A: Accept<TcpStream, S> + Clone + Send + 'static,
    A::Future: Send,
    S: Send + 'static,
{
    type Stream = Slotted<A::Stream>;
    type Service = A::Service;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let slot = self.slots.clone().try_acquire_owned();
        let inner = self.inner.clone();
        Box::pin(async move {
            let Ok(slot) = slot else {
                CONNECTIONS_REFUSED.inc();
                // Dropping the stream closes the connection.
                return Err(io::Error::other(
                    "the listener has its maximum of open connections",
                ));
            };
            let (stream, service) = inner.accept(stream, service).await?;
            Ok((
                Slotted {
                    inner: stream,
                    _slot: slot,
                },
                service,
            ))
        })
    }
}

/// A connection stream that holds its connection slot until it is dropped.
struct Slotted<T> {
    inner: T,
    _slot: OwnedSemaphorePermit,
}

impl<T: AsyncRead + Unpin> AsyncRead for Slotted<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
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
            max_connections,
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
        let zero = limits(&[
            ("DGP_HEADER_READ_TIMEOUT_SECS", "0"),
            ("DGP_MAX_CONNECTIONS", "0"),
        ]);
        assert_eq!(zero.header_read_timeout, Duration::from_secs(1));
        assert_eq!(zero.max_connections, 1);
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
        for (file, text) in crate::source_scan::prod_sources("src") {
            if file == "src/serve.rs" {
                continue;
            }
            for (n, line) in crate::source_scan::prod_lines(&text) {
                if line.contains("axum::serve(")
                    || line.contains("axum_server::bind")
                    || line.contains("axum_server::from_tcp")
                {
                    offenders.push(format!("{file}:{n}: {}", line.trim()));
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
