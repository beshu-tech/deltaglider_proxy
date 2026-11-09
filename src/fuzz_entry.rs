// SPDX-License-Identifier: BUSL-1.1

//! Entry points for the `fuzz/` targets (cargo-fuzz / libFuzzer).
//!
//! Each function feeds one untrusted-input parser and asserts the
//! invariants a crash would break. They are compiled in every build (not
//! behind `cfg(fuzzing)`) so a refactor that breaks one fails `cargo test
//! --lib`, and the seed tests below run each on its seed corpus.

use axum::body::{Body, Bytes};
use axum::http::Request;

/// The aws-chunked body decoder (pre-auth input on an open-access server).
/// The first byte picks whether an expected decoded length is passed (and
/// seeds the split points). The streaming decoder must give the same
/// result whatever way the body is split, through the stream adapter
/// too, and the buffered wrapper must agree with it.
pub fn aws_chunked(data: &[u8]) {
    use crate::api::aws_chunked::{
        decode_aws_chunked, decode_aws_chunked_with_trailers, AwsChunkedStream,
    };
    let (seed, expected, body) = match data.split_first() {
        Some((&flag, rest)) if flag % 2 == 1 && rest.len() >= 2 => {
            let n = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            (flag, Some(n), &rest[2..])
        }
        Some((&flag, rest)) => (flag, None, rest),
        None => (0, None, data),
    };
    let body = Bytes::copy_from_slice(body);
    let wrapper = decode_aws_chunked_with_trailers(&body, expected);
    assert_eq!(
        decode_aws_chunked(&body, expected),
        wrapper.as_ref().map(|(b, _)| b.clone())
    );
    if let Some((decoded, _)) = &wrapper {
        // The framing only removes bytes.
        assert!(decoded.len() <= body.len());
        if let Some(n) = expected {
            assert_eq!(decoded.len(), n);
        }
    }
    // Split points derived from the data: piece sizes 1..=8 from a
    // generator seeded by the flag byte and the body.
    let mut x = (u64::from(seed) ^ (body.len() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1;
    let mut pieces = Vec::new();
    let mut at = 0;
    while at < body.len() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let end = (at + 1 + (x % 8) as usize).min(body.len());
        pieces.push(body.slice(at..end));
        at = end;
    }
    let expected = expected.map(|n| n as u64);
    let split = {
        let mut decoder = crate::api::aws_chunked::ChunkedDecoder::new(expected);
        let mut out = Vec::new();
        pieces
            .iter()
            .try_for_each(|p| decoder.feed(p.clone(), &mut out))
            .and_then(|()| decoder.finish())
            .map(|trailers| (out.concat(), trailers))
    };
    assert_eq!(
        split.as_ref().ok().map(|(b, t)| (b.as_slice(), t)),
        wrapper.as_ref().map(|(b, t)| (b.as_ref(), t)),
        "a split feed disagrees with one feed"
    );
    // The stream adapter over the same pieces (a ready source: one poll
    // per item, no runtime).
    let source = futures::stream::iter(pieces.into_iter().map(Ok::<_, std::convert::Infallible>));
    let mut stream = AwsChunkedStream::new(source, expected);
    let slot = stream.trailers();
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut streamed = Vec::new();
    let mut failed = false;
    loop {
        match futures::Stream::poll_next(std::pin::Pin::new(&mut stream), &mut cx) {
            std::task::Poll::Ready(Some(Ok(piece))) => streamed.extend_from_slice(&piece),
            std::task::Poll::Ready(Some(Err(_))) => failed = true,
            std::task::Poll::Ready(None) => break,
            std::task::Poll::Pending => unreachable!("a ready source never pends"),
        }
    }
    match &split {
        Ok((decoded, trailers)) => {
            assert!(!failed, "the stream failed where the decoder did not");
            assert_eq!(&streamed, decoded);
            assert_eq!(slot.get().as_ref(), Some(trailers));
        }
        Err(_) => assert!(
            failed && slot.get().is_none(),
            "the stream accepted a bad body"
        ),
    }
}

/// SigV4 identity parsing: the Authorization header and the presigned
/// query, as the SigV4 middleware reads them before s3s. Input: the header
/// value, then `\n`, then the raw path-and-query.
pub fn sigv4(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let (header, path_and_query) = text.split_once('\n').unwrap_or((&text, "/b/k"));
    let pq = if path_and_query.starts_with('/') {
        path_and_query.to_string()
    } else {
        format!("/{path_and_query}")
    };
    let mut builder = Request::builder().uri(pq.as_str());
    if !header.is_empty() {
        builder = builder.header("authorization", header.as_bytes());
    }
    let Ok(request) = builder.body(Body::empty()) else {
        return;
    };
    let _ = crate::api::auth::fuzz_sigv4_identity(&request);
    if let Ok(target) = crate::api::request_target::RequestTarget::from_uri(request.uri()) {
        let _ = target.bucket_and_key();
        let _ = target.bucket_only();
        let _ = target.is_presigned_v4();
        let _ = crate::iam::middleware::authz_target(request.method(), &target);
    }
}

/// The YAML config loader, with `${env:...}` expansion against a fixed
/// environment. An accepted config must survive its canonical export: the
/// export is what an operator applies back (the IaC round trip).
pub fn config_yaml(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let lookup = |name: &str| match name {
        "HOME" => Some("/home/dgp".to_string()),
        "SECRET" => Some("s3cr3t-value".to_string()),
        "EMPTY" => Some(String::new()),
        _ => None,
    };
    let Ok(expanded) = crate::config::expand_env_with(text, lookup) else {
        return;
    };
    let Ok(config) = crate::config::Config::from_yaml_str(&expanded) else {
        return;
    };
    let _ = config.check_fatal();
    if let Ok(exported) = config.to_canonical_yaml() {
        if let Err(e) = crate::config::Config::from_yaml_str(&exported) {
            panic!("the canonical export of an accepted config does not load: {e}\n{exported}");
        }
    }
}

/// The admission spec parser and chain: parse, validate, compile, and
/// evaluate a few requests. A spec `validate` accepts must compile into
/// the chain with every block (compile errors are silently skipped there).
pub fn admission_spec(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(spec) = serde_yaml::from_str::<crate::admission::AdmissionSpec>(text) else {
        return;
    };
    if spec.validate().is_err() {
        return;
    }
    let chain = crate::admission::AdmissionChain::from_config_parts(
        &std::collections::BTreeMap::new(),
        &spec.blocks,
    );
    assert_eq!(
        chain.blocks().len(),
        spec.blocks.len(),
        "a validated admission block was dropped at chain build"
    );
    for (method, path, query) in [
        ("GET", "/b/k", ""),
        ("PUT", "/pro%64/secre%74", ""),
        ("GET", "/b", "prefix=a%2Fb"),
        ("DELETE", "//b", ""),
    ] {
        let info = crate::admission::middleware::OwnedRequestInfo::from_raw(
            method,
            path,
            query,
            false,
            Some(std::net::IpAddr::from([203, 0, 113, 7])),
        );
        let _ = crate::admission::evaluate(&chain, &info.as_ref());
    }
}

/// The at-rest encryption v2 parsers: the `dg-sealed` value, the sealed
/// facts, listing tokens and a stored body. A backend writer controls all
/// of them, so none may panic.
pub fn encryption_v2(data: &[u8]) {
    crate::storage::encrypting::v2::fuzz_sealed_and_frames(data);
}

#[cfg(test)]
mod tests {
    //! Run each entry on its checked-in seeds (`fuzz/seeds/<target>`)
    //! so the seeds and the entries stay valid without a fuzzer.

    fn seeds(target: &str) -> Vec<Vec<u8>> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fuzz/seeds")
            .join(target);
        let mut out: Vec<Vec<u8>> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("seed corpus {dir:?}: {e}"))
            .map(|e| std::fs::read(e.unwrap().path()).unwrap())
            .collect();
        out.push(Vec::new());
        out
    }

    #[test]
    fn aws_chunked_seeds() {
        for s in seeds("aws_chunked") {
            super::aws_chunked(&s);
        }
    }

    #[test]
    fn sigv4_seeds() {
        for s in seeds("sigv4") {
            super::sigv4(&s);
        }
    }

    #[test]
    fn config_yaml_seeds() {
        for s in seeds("config_yaml") {
            super::config_yaml(&s);
        }
    }

    #[test]
    fn encryption_v2_seeds() {
        for s in seeds("encryption_v2") {
            super::encryption_v2(&s);
        }
    }

    #[test]
    fn admission_spec_seeds() {
        for s in seeds("admission_spec") {
            super::admission_spec(&s);
        }
    }
}
