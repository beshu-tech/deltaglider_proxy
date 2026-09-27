// SPDX-License-Identifier: BUSL-1.1

use super::{content_type_needs_sandbox, decode_v2_token, encode_v2_token};
use proptest::prelude::*;

proptest! {
    /// Any engine cursor survives the opaque V2 token, and the token is
    /// XML-safe text (review2 #20).
    #[test]
    fn v2_token_round_trips(key in any::<String>()) {
        let token = encode_v2_token(&key);
        prop_assert!(token.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)));
        let cursor = decode_v2_token(Some(&token)).unwrap();
        prop_assert!(!cursor.legacy);
        prop_assert_eq!(cursor.key, key);
    }

    /// The CSP inert check never panics, and exempts a value only when
    /// the WHOLE value is one well-formed inert type (review2 #3): no
    /// list, no active essence anywhere, whatever the case or params.
    #[test]
    fn sandbox_exemption_only_for_one_inert_type(ct in prop_oneof![
        ".{0,40}",
        proptest::collection::vec(
            prop_oneof![
                Just("image/png"), Just("IMAGE/PNG"), Just("image/svg+xml"),
                Just("text/html"), Just("video/mp4"), Just("application/pdf"),
                Just("application/xhtml+xml"), Just(" "), Just(","), Just(";"),
                Just("charset=utf-8"), Just("/"), Just("\t"), Just("\""),
            ],
            0..6,
        ).prop_map(|v| v.concat()),
    ]) {
        if !content_type_needs_sandbox(&ct) {
            prop_assert!(!ct.contains(','));
            let essence = ct.split(';').next().unwrap().trim().to_ascii_lowercase();
            let inert = (essence.starts_with("image/") && !essence.contains("svg"))
                || essence.starts_with("video/")
                || essence.starts_with("audio/")
                || essence == "application/pdf";
            prop_assert!(inert, "{ct:?} exempt with essence {essence:?}");
            prop_assert!(!essence.contains("html") && !essence.contains("xml"));
        }
    }
}
