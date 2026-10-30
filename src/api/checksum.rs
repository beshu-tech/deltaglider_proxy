// SPDX-License-Identifier: BUSL-1.1

//! Flexible checksums (`x-amz-checksum-*`) of an uploaded body.
//!
//! A client names the checksum in a request header or, for an aws-chunked
//! body, in a trailer (announced by `x-amz-trailer`). Current AWS SDKs send
//! one on every PutObject and UploadPart. The proxy computes it over the
//! decoded body and refuses a mismatch, as S3 does:
//!
//! - a value that is not base64 of the right length: `InvalidRequest`;
//! - more than one checksum: `InvalidRequest`;
//! - a checksum announced in `x-amz-trailer` that never arrived: `InvalidRequest`;
//! - a value that does not match the body: `BadDigest`.

use axum::http::HeaderMap;
use base64::Engine as _;
use s3s::crypto::Checksum as _;

use super::S3Error;

/// One S3 flexible-checksum algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChecksumAlgorithm {
    Crc32,
    Crc32c,
    Crc64Nvme,
    Sha1,
    Sha256,
}

impl ChecksumAlgorithm {
    pub(crate) const ALL: [Self; 5] = [
        Self::Crc32,
        Self::Crc32c,
        Self::Crc64Nvme,
        Self::Sha1,
        Self::Sha256,
    ];

    /// The header (and trailer, and form field) that carries the value.
    pub(crate) fn header(self) -> &'static str {
        match self {
            Self::Crc32 => "x-amz-checksum-crc32",
            Self::Crc32c => "x-amz-checksum-crc32c",
            Self::Crc64Nvme => "x-amz-checksum-crc64nvme",
            Self::Sha1 => "x-amz-checksum-sha1",
            Self::Sha256 => "x-amz-checksum-sha256",
        }
    }

    /// The name S3 uses in its error messages.
    fn label(self) -> &'static str {
        match self {
            Self::Crc32 => "CRC32",
            Self::Crc32c => "CRC32C",
            Self::Crc64Nvme => "CRC64NVME",
            Self::Sha1 => "SHA1",
            Self::Sha256 => "SHA256",
        }
    }

    fn from_header(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|a| a.header().eq_ignore_ascii_case(name.trim()))
    }

    fn digest(self, body: &[u8]) -> Vec<u8> {
        match self {
            Self::Crc32 => s3s::crypto::Crc32::checksum(body).to_vec(),
            Self::Crc32c => s3s::crypto::Crc32c::checksum(body).to_vec(),
            Self::Crc64Nvme => s3s::crypto::Crc64Nvme::checksum(body).to_vec(),
            Self::Sha1 => s3s::crypto::Sha1::checksum(body).as_ref().to_vec(),
            Self::Sha256 => s3s::crypto::Sha256::checksum(body).as_ref().to_vec(),
        }
    }

    fn digest_len(self) -> usize {
        match self {
            Self::Crc32 | Self::Crc32c => 4,
            Self::Crc64Nvme => 8,
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChecksumError {
    /// The value is not base64 of the algorithm's digest length.
    Invalid(ChecksumAlgorithm),
    /// More than one checksum value.
    Multiple,
    /// `x-amz-trailer` announced this checksum; no trailer carried it.
    MissingTrailer(ChecksumAlgorithm),
    /// The body does not hash to the value.
    Mismatch(ChecksumAlgorithm),
}

impl From<ChecksumError> for S3Error {
    fn from(err: ChecksumError) -> Self {
        match err {
            ChecksumError::Invalid(a) => {
                S3Error::InvalidRequest(format!("Value for {} header is invalid.", a.header()))
            }
            ChecksumError::Multiple => S3Error::InvalidRequest(
                "Expecting a single x-amz-checksum- header. Multiple checksum Types are not allowed."
                    .into(),
            ),
            ChecksumError::MissingTrailer(a) => S3Error::InvalidRequest(format!(
                "x-amz-trailer names {} but the body carried no such trailer.",
                a.header()
            )),
            ChecksumError::Mismatch(a) => S3Error::BadDigestReason(format!(
                "The {} you specified did not match the calculated checksum.",
                a.label()
            )),
        }
    }
}

/// Pure: does `body` hash to `expected_b64` under `algorithm`?
pub(crate) fn verify_checksum(
    algorithm: ChecksumAlgorithm,
    expected_b64: &str,
    body: &[u8],
) -> Result<(), ChecksumError> {
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected_b64.trim())
        .ok()
        .filter(|d| d.len() == algorithm.digest_len())
        .ok_or(ChecksumError::Invalid(algorithm))?;
    if algorithm.digest(body) != expected {
        return Err(ChecksumError::Mismatch(algorithm));
    }
    Ok(())
}

/// Pure: the one checksum the client sent, from the request headers or the
/// aws-chunked trailers (a form POST passes its fields as `headers`).
pub(crate) fn expected_checksum(
    headers: &HeaderMap,
    trailers: Option<&HeaderMap>,
) -> Result<Option<(ChecksumAlgorithm, String)>, ChecksumError> {
    let mut found = Vec::new();
    for algorithm in ChecksumAlgorithm::ALL {
        let values = headers.get_all(algorithm.header()).iter().chain(
            trailers
                .into_iter()
                .flat_map(|t| t.get_all(algorithm.header())),
        );
        for value in values {
            let value = value
                .to_str()
                .map_err(|_| ChecksumError::Invalid(algorithm))?;
            found.push((algorithm, value.to_string()));
        }
    }
    if found.len() > 1 {
        return Err(ChecksumError::Multiple);
    }
    if let Some(one) = found.pop() {
        return Ok(Some(one));
    }
    // A checksum announced for the trailer must arrive.
    let announced = headers
        .get_all("x-amz-trailer")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .find_map(ChecksumAlgorithm::from_header);
    match announced {
        Some(algorithm) => Err(ChecksumError::MissingTrailer(algorithm)),
        None => Ok(None),
    }
}

/// THE check of a collected body against the client's checksum, if any.
pub(crate) fn verify_body_checksum(
    headers: &HeaderMap,
    trailers: Option<&HeaderMap>,
    body: &[u8],
) -> Result<(), ChecksumError> {
    match expected_checksum(headers, trailers)? {
        Some((algorithm, expected)) => verify_checksum(algorithm, &expected, body),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChecksumAlgorithm::*;

    /// Known digests of `b"123456789"` (the CRC check input) and of the
    /// empty body, base64 as S3 sends them. CRC check values: CRC-32
    /// cbf43926, CRC-32C e3069283, CRC-64/NVME ae8b14860a799888.
    const VECTORS: &[(ChecksumAlgorithm, &[u8], &str)] = &[
        (Crc32, b"123456789", "y/Q5Jg=="),
        (Crc32c, b"123456789", "4waSgw=="),
        (Crc64Nvme, b"123456789", "rosUhgp5mIg="),
        (Sha1, b"123456789", "98O8HYCOBHMq32eZZczDTKeuNEE="),
        (
            Sha256,
            b"123456789",
            "FeKw08M4keuw8e9gnsQZQgwg4yDOlMZfvIwzEkSOsiU=",
        ),
        (Crc32, b"", "AAAAAA=="),
        (Crc32c, b"", "AAAAAA=="),
        (Crc64Nvme, b"", "AAAAAAAAAAA="),
        (Sha1, b"", "2jmj7l5rSw0yVb/vlWAYkK/YBwk="),
        (Sha256, b"", "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="),
    ];

    #[test]
    fn known_vectors_verify() {
        for &(algorithm, body, b64) in VECTORS {
            assert_eq!(
                verify_checksum(algorithm, b64, body),
                Ok(()),
                "{algorithm:?}"
            );
        }
    }

    #[test]
    fn a_changed_body_is_a_mismatch() {
        for &(algorithm, _, b64) in VECTORS.iter().filter(|v| !v.1.is_empty()) {
            assert_eq!(
                verify_checksum(algorithm, b64, b"123456780"),
                Err(ChecksumError::Mismatch(algorithm)),
                "{algorithm:?}"
            );
        }
    }

    #[test]
    fn a_malformed_value_is_invalid() {
        for algorithm in ChecksumAlgorithm::ALL {
            for bad in [
                "",
                "not base64!",
                "dGVz",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==",
            ] {
                assert_eq!(
                    verify_checksum(algorithm, bad, b"x"),
                    Err(ChecksumError::Invalid(algorithm)),
                    "{algorithm:?} {bad}"
                );
            }
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(*k, v.parse().unwrap());
        }
        map
    }

    #[test]
    fn the_checksum_comes_from_a_header_or_a_trailer() {
        let h = headers(&[("x-amz-checksum-crc32", "y/Q5Jg==")]);
        assert_eq!(verify_body_checksum(&h, None, b"123456789"), Ok(()));
        assert_eq!(
            verify_body_checksum(&h, None, b"12345678"),
            Err(ChecksumError::Mismatch(Crc32))
        );

        let announced = headers(&[("x-amz-trailer", "x-amz-checksum-crc32c")]);
        let t = headers(&[("x-amz-checksum-crc32c", "4waSgw==")]);
        assert_eq!(
            verify_body_checksum(&announced, Some(&t), b"123456789"),
            Ok(())
        );
        assert_eq!(
            verify_body_checksum(&announced, Some(&t), b"x"),
            Err(ChecksumError::Mismatch(Crc32c))
        );
    }

    #[test]
    fn no_checksum_passes() {
        assert_eq!(verify_body_checksum(&HeaderMap::new(), None, b"x"), Ok(()));
        let t = headers(&[("x-amz-meta-foo", "bar")]);
        let unrelated = headers(&[
            ("x-amz-checksum-algorithm", "CRC32"),
            ("x-amz-checksum-type", "FULL_OBJECT"),
            ("x-amz-trailer", "x-amz-meta-foo"),
        ]);
        assert_eq!(verify_body_checksum(&unrelated, Some(&t), b"x"), Ok(()));
    }

    #[test]
    fn two_checksums_are_refused() {
        let h = headers(&[
            ("x-amz-checksum-crc32", "y/Q5Jg=="),
            ("x-amz-checksum-crc32c", "4waSgw=="),
        ]);
        assert_eq!(
            verify_body_checksum(&h, None, b"123456789"),
            Err(ChecksumError::Multiple)
        );
        let h = headers(&[("x-amz-checksum-crc32", "y/Q5Jg==")]);
        let t = headers(&[("x-amz-checksum-crc32", "y/Q5Jg==")]);
        assert_eq!(
            verify_body_checksum(&h, Some(&t), b"123456789"),
            Err(ChecksumError::Multiple)
        );
    }

    #[test]
    fn an_announced_trailer_must_arrive() {
        let h = headers(&[("x-amz-trailer", "x-amz-checksum-crc64nvme")]);
        assert_eq!(
            verify_body_checksum(&h, None, b"x"),
            Err(ChecksumError::MissingTrailer(Crc64Nvme))
        );
        assert_eq!(
            verify_body_checksum(&h, Some(&HeaderMap::new()), b"x"),
            Err(ChecksumError::MissingTrailer(Crc64Nvme))
        );
    }

    #[test]
    fn errors_carry_the_s3_codes() {
        for (err, code) in [
            (ChecksumError::Invalid(Crc32), "InvalidRequest"),
            (ChecksumError::Multiple, "InvalidRequest"),
            (ChecksumError::MissingTrailer(Crc32), "InvalidRequest"),
            (ChecksumError::Mismatch(Crc32), "BadDigest"),
        ] {
            assert_eq!(S3Error::from(err).code(), code);
        }
    }
}
