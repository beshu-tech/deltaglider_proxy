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
/// The first byte picks whether an expected decoded length is passed.
pub fn aws_chunked(data: &[u8]) {
    let (expected, body) = match data.split_first() {
        Some((&flag, rest)) if flag % 2 == 1 && rest.len() >= 2 => {
            let n = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            (Some(n), &rest[2..])
        }
        Some((_, rest)) => (None, rest),
        None => (None, data),
    };
    let body = Bytes::copy_from_slice(body);
    if let Some(decoded) = crate::api::aws_chunked::decode_aws_chunked(&body, expected) {
        // The framing only removes bytes.
        assert!(decoded.len() <= body.len());
        if let Some(n) = expected {
            assert_eq!(decoded.len(), n);
        }
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
    fn admission_spec_seeds() {
        for s in seeds("admission_spec") {
            super::admission_spec(&s);
        }
    }
}
