// SPDX-License-Identifier: BUSL-1.1

//! THE two ways the proxy serves its listener: plain HTTP and HTTPS.
//!
//! Both set `TCP_NODELAY` on every accepted connection. Without it, hyper
//! writes the response head and the body in two writes, and Nagle holds the
//! second write until the client ACKs the first. The client delays that ACK
//! (about 40 ms on Linux), so every response after the first on a
//! keep-alive connection waited about 40 ms.
//!
//! `axum::serve` does not set nodelay unless asked, and the `axum-server`
//! TLS acceptor wraps `DefaultAcceptor`, which does not set it either
//! (axum-server 0.7.3, `accept.rs`). Source guard:
//! `listeners_are_served_only_through_serve_rs`.

use std::future::Future;
use std::io;
use std::net::SocketAddr;

use axum::Router;
use axum_server::accept::NoDelayAcceptor;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use tokio::net::TcpListener;

/// Serve `app` over plain HTTP on `listener` until `shutdown` resolves.
/// Middlewares see the peer address through `ConnectInfo<SocketAddr>`.
pub async fn serve_plain<F>(listener: TcpListener, app: Router, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .tcp_nodelay(true)
    .with_graceful_shutdown(shutdown)
    .await
}

/// Serve `app` over HTTPS on `listener` until `handle` shuts it down.
/// Middlewares see the peer address through `ConnectInfo<SocketAddr>`.
pub async fn serve_tls(
    listener: TcpListener,
    config: RustlsConfig,
    handle: axum_server::Handle,
    app: Router,
) -> io::Result<()> {
    axum_server::from_tcp(listener.into_std()?)
        .acceptor(RustlsAcceptor::new(config).acceptor(NoDelayAcceptor::new()))
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
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
