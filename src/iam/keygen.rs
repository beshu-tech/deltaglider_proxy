// SPDX-License-Identifier: BUSL-1.1

//! Cryptographic key generation for IAM users.

use rand::rngs::OsRng;
use rand::Rng;

/// Generate an AWS-like access key ID (20 chars: "AK" + 18 uppercase alphanumeric).
pub fn generate_access_key_id() -> String {
    let mut rng = OsRng;
    let chars: Vec<char> = (0..18)
        .map(|_| {
            let idx = rng.gen_range(0..36);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'A' + idx - 10) as char
            }
        })
        .collect();
    format!("AK{}", chars.iter().collect::<String>())
}

/// Generate an AWS-like secret access key (40 chars, base64-alphabet).
pub fn generate_secret_access_key() -> String {
    let mut rng = OsRng;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    (0..40)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Whether `access_key_id` has a usable format: non-empty, ASCII, no
/// whitespace (a SigV4 `Credential=` field cannot carry anything else).
/// THE check of every admin path that sets a user's access key id: create,
/// clone and rotate (update cannot change the key).
pub fn access_key_id_is_valid(access_key_id: &str) -> bool {
    !access_key_id.is_empty()
        && access_key_id.is_ascii()
        && !access_key_id.contains(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_key_id_format_check() {
        for ok in ["AKIAEXAMPLE", "ci-uploader", "a", "AK_1.2/x"] {
            assert!(access_key_id_is_valid(ok), "{ok:?}");
        }
        for bad in ["", " ", "AK EXAMPLE", "AK\tX", "AKX\n", "AKé", " AK"] {
            assert!(!access_key_id_is_valid(bad), "{bad:?}");
        }
        assert!(access_key_id_is_valid(&generate_access_key_id()));
    }

    proptest::proptest! {
        #[test]
        fn access_key_id_check_matches_its_definition(s in "\\PC*|[ \t\na-zé]{0,4}") {
            let want = !s.is_empty() && s.chars().all(|c| c.is_ascii() && !c.is_whitespace());
            proptest::prop_assert_eq!(access_key_id_is_valid(&s), want);
        }
    }

    #[test]
    fn test_access_key_id_format() {
        let key = generate_access_key_id();
        assert_eq!(key.len(), 20);
        assert!(key.starts_with("AK"));
        assert!(key[2..]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn test_secret_access_key_format() {
        let key = generate_secret_access_key();
        assert_eq!(key.len(), 40);
        assert!(key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/'));
    }

    #[test]
    fn test_keys_are_unique() {
        let k1 = generate_access_key_id();
        let k2 = generate_access_key_id();
        assert_ne!(k1, k2);

        let s1 = generate_secret_access_key();
        let s2 = generate_secret_access_key();
        assert_ne!(s1, s2);
    }
}
