//! What may be written to a Wi-Fi trace.

use crate::crypto;
use std::sync::OnceLock;

/// `"h:" + 16 lowercase hex characters` of `SHA-256(process salt | value)[0..8]`:
/// A4's `WifiLog.hash` format, one hash format everywhere. The salt is random per
/// process and never written to disk.
pub fn tag(value: &[u8]) -> String {
    static SALT: OnceLock<Option<[u8; 32]>> = OnceLock::new();
    let Some(salt) = SALT.get_or_init(|| crypto::random_nonce().ok()) else {
        return "h:????????????????".to_string();
    };
    let mut input = zeroize::Zeroizing::new(Vec::with_capacity(salt.len() + value.len()));
    input.extend_from_slice(salt);
    input.extend_from_slice(value);
    let Ok(hash) = crypto::sha256(input.as_slice()) else {
        return "h:????????????????".to_string();
    };
    let mut out = String::with_capacity(18);
    out.push_str("h:");
    for byte in &hash[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// "BENCHMARK-compatible" detail strings for `radio_backend::trace`:
/// `key=value | key=value` joined with " | ".
pub fn detail(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_value_gets_the_same_tag_in_one_process_and_different_values_differ() {
        let first = tag(b"peer-address");
        assert_eq!(first, tag(b"peer-address"));
        assert_ne!(first, tag(b"peer-address-2"));
        assert_eq!(first.len(), 18);
        assert!(first.starts_with("h:"));
        assert!(first[2..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn a_tag_never_contains_the_value() {
        let value = [0x9e, 0x8d, 0x7c, 0x6b, 0x5a, 0x49, 0x38, 0x27];
        let text = tag(&value);
        let hex: String = value.iter().map(|byte| format!("{byte:02x}")).collect();
        // The whole value never appears; a short accidental substring of a random salted hash is not a disclosure.
        assert!(!text.contains(&hex), "{text}");
    }

    #[test]
    fn detail_joins_pairs_with_pipes() {
        assert_eq!(
            detail(&[("carrier", "WIFI_DIRECT"), ("reason", "switch")]),
            "carrier=WIFI_DIRECT | reason=switch"
        );
    }
}
