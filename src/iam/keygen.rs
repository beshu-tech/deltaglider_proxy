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

#[cfg(test)]
mod tests {
    use super::*;

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
