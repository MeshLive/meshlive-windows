//! Wi-Fi upgrade key derivations.
//!
//! All primitives come from `crate::crypto` (Windows CNG); no primitive is
//! reimplemented here. Every secret stays in `Zeroizing`.

use crate::crypto;
use zeroize::Zeroizing;

pub type Error = crypto::Error;

const INFO_KEY: &[u8] = b"BluetoothVideoMesh/WifiUpgrade/1/key";
const LABEL_BIND: &[u8] = b"BluetoothVideoMesh/WifiUpgrade/1/bind";
const INFO_G2H: &[u8] = b"BluetoothVideoMesh/WifiUpgrade/1/g2h";
const INFO_H2G: &[u8] = b"BluetoothVideoMesh/WifiUpgrade/1/h2g";

pub const SSID_ALPHABET: &[u8; 31] = b"abcdefghjkmnpqrstuvwxyz23456789";

fn hmac_parts(uw: &[u8; 32], parts: &[&[u8]]) -> Result<Zeroizing<[u8; 32]>, Error> {
    let mut input = Zeroizing::new(Vec::new());
    for part in parts {
        input.extend_from_slice(part);
    }
    crypto::hmac_sha256(uw, input.as_slice())
}

/// UW = HKDF-Expand(EXP_e2e, "BluetoothVideoMesh/WifiUpgrade/1/key", 32).
pub fn uw_for_call(exporter: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, Error> {
    crypto::hkdf_expand::<32>(exporter, INFO_KEY)
}

/// 8 random bytes: the first 8 of `crypto::random_nonce()`.
pub fn new_ticket() -> Result<[u8; 8], Error> {
    let nonce = crypto::random_nonce()?;
    Ok(nonce[..8].try_into().expect("eight bytes"))
}

/// "DIRECT-" | a2 | "-" | a6 (16 characters).
pub fn direct_ssid(uw: &[u8; 32], ticket: &[u8; 8]) -> Result<String, Error> {
    let hash = hmac_parts(uw, &[b"ssid", ticket])?;
    let mut chars = [0u8; 8];
    for (i, slot) in chars.iter_mut().enumerate() {
        let value = u16::from_be_bytes([hash[2 * i], hash[2 * i + 1]]) % 31;
        *slot = SSID_ALPHABET[value as usize];
    }
    let mut ssid = String::with_capacity(16);
    ssid.push_str("DIRECT-");
    ssid.push(chars[0] as char);
    ssid.push(chars[1] as char);
    ssid.push('-');
    for ch in &chars[2..] {
        ssid.push(*ch as char);
    }
    Ok(ssid)
}

/// base64url (no padding) of HMAC-SHA-256(UW, "psk" | ticket), 43 characters.
pub fn direct_psk(uw: &[u8; 32], ticket: &[u8; 8]) -> Result<Zeroizing<String>, Error> {
    let hash = hmac_parts(uw, &[b"psk", ticket])?;
    Ok(Zeroizing::new(base64url(&hash[..])))
}

/// pre = HMAC-SHA-256(UW, "pre" | ticket | challenge | e_G | n_G)[0..16].
pub fn pre(
    uw: &[u8; 32],
    ticket: &[u8; 8],
    challenge: &[u8; 16],
    e_g: &[u8; 65],
    n_g: &[u8; 32],
) -> Result<[u8; 16], Error> {
    let hash = hmac_parts(uw, &[b"pre", ticket, challenge, e_g, n_g])?;
    Ok(hash[..16].try_into().expect("sixteen bytes"))
}

/// th = SHA-256("BluetoothVideoMesh/WifiUpgrade/1/bind" | 0x00 | frame1 | frame2 | frame3).
pub fn transcript_hash(
    frame1: &[u8; 21],
    frame2: &[u8; 126],
    frame3: &[u8; 102],
) -> Result<[u8; 32], Error> {
    let mut input = Zeroizing::new(Vec::with_capacity(
        LABEL_BIND.len() + 1 + frame1.len() + frame2.len() + frame3.len(),
    ));
    input.extend_from_slice(LABEL_BIND);
    input.push(0);
    input.extend_from_slice(frame1);
    input.extend_from_slice(frame2);
    input.extend_from_slice(frame3);
    crypto::sha256(input.as_slice())
}

pub struct BindKeys {
    pub g2h: Zeroizing<[u8; 32]>,
    pub h2g: Zeroizing<[u8; 32]>,
}
impl std::fmt::Debug for BindKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindKeys")
            .field("g2h", &"<redacted>")
            .field("h2g", &"<redacted>")
            .finish()
    }
}

/// HMAC-SHA-256(UW, "bind" | ticket | th).
pub fn bind_salt(uw: &[u8; 32], ticket: &[u8; 8], th: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, Error> {
    hmac_parts(uw, &[b"bind", ticket, th])
}

/// K = HKDF-Extract(salt, dh) = `crypto::hmac_sha256(salt, dh)`.
pub fn bind_prk(salt: &[u8; 32], dh: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, Error> {
    crypto::hmac_sha256(salt, dh)
}

pub fn bind_keys(
    uw: &[u8; 32],
    ticket: &[u8; 8],
    th: &[u8; 32],
    dh: &[u8; 32],
) -> Result<BindKeys, Error> {
    let salt = bind_salt(uw, ticket, th)?;
    let prk = bind_prk(&salt, dh)?;
    Ok(BindKeys {
        g2h: crypto::hkdf_expand::<32>(&prk, INFO_G2H)?,
        h2g: crypto::hkdf_expand::<32>(&prk, INFO_H2G)?,
    })
}

/// RFC 4648 base64url without padding.
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4 + 2) / 3);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        out.push(ALPHABET[(a >> 2) as usize] as char);
        if chunk.len() == 1 {
            out.push(ALPHABET[((a & 0x03) << 4) as usize] as char);
            break;
        }
        let b = chunk[1];
        out.push(ALPHABET[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() == 2 {
            out.push(ALPHABET[((b & 0x0f) << 2) as usize] as char);
            break;
        }
        let c = chunk[2];
        out.push(ALPHABET[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        out.push(ALPHABET[(c & 0x3f) as usize] as char);
    }
    out
}

/// P-256 point from the 91-byte SPKI the CNG wrapper uses: the last 65 bytes.
pub fn point_of(spki: &[u8; 91]) -> [u8; 65] {
    spki[26..].try_into().expect("sixty-five bytes")
}

/// SPKI from a 65-byte point; None if point[0] != 4.
pub fn spki_of(point: &[u8; 65]) -> Option<[u8; 91]> {
    if point[0] != 4 {
        return None;
    }
    let mut out = [0; 91];
    out[..26].copy_from_slice(&crypto::SPKI_PREFIX[..26]);
    out[26..].copy_from_slice(point);
    Some(out)
}

/// Constant-time compare of two 16-byte values.
pub fn eq16(a: &[u8; 16], b: &[u8; 16]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_uw() -> [u8; 32] {
        [0x42; 32]
    }
    fn fixed_ticket() -> [u8; 8] {
        [0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18]
    }

    #[test]
    fn base64url_matches_the_rfc_4648_test_vectors_without_padding() {
        for (input, expected) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg"),
            (&b"fo"[..], "Zm8"),
            (&b"foo"[..], "Zm9v"),
            (&b"foob"[..], "Zm9vYg"),
            (&b"fooba"[..], "Zm9vYmE"),
            (&b"foobar"[..], "Zm9vYmFy"),
        ] {
            assert_eq!(base64url(input), expected);
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn a_psk_is_43_url_safe_characters_and_an_ssid_matches_the_android_pattern() {
        let psk = direct_psk(&fixed_uw(), &fixed_ticket()).unwrap();
        assert_eq!(psk.len(), 43);
        assert!(psk.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        let ssid = direct_ssid(&fixed_uw(), &fixed_ticket()).unwrap();
        assert_eq!(ssid.len(), 16);
        assert!(ssid.starts_with("DIRECT-"));
        assert_eq!(ssid.as_bytes()[9], b'-');
        for index in [7, 8, 10, 11, 12, 13, 14, 15] {
            assert!(
                SSID_ALPHABET.contains(&ssid.as_bytes()[index]),
                "{ssid}"
            );
        }
        assert!(ssid.as_bytes()[7].is_ascii_alphanumeric());
        assert!(ssid.as_bytes()[8].is_ascii_alphanumeric());
    }

    #[test]
    fn ssid_and_psk_change_with_the_ticket_and_with_uw() {
        let uw = fixed_uw();
        let other_uw = [0x43; 32];
        let first = (
            direct_ssid(&uw, &fixed_ticket()).unwrap(),
            direct_psk(&uw, &fixed_ticket()).unwrap().to_string(),
        );
        let second = (
            direct_ssid(&uw, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap(),
            direct_psk(&uw, &[1, 2, 3, 4, 5, 6, 7, 8])
                .unwrap()
                .to_string(),
        );
        let third = (
            direct_ssid(&other_uw, &fixed_ticket()).unwrap(),
            direct_psk(&other_uw, &fixed_ticket()).unwrap().to_string(),
        );
        assert_ne!(first.0, second.0);
        assert!(first.1 != second.1, "mismatch");
        assert_ne!(first.0, third.0);
        assert!(first.1 != third.1, "mismatch");
    }

    #[test]
    fn point_and_spki_convert_back_and_forth() {
        let spki = crypto::Ephemeral::generate().unwrap().public_key().unwrap();
        let point = point_of(&spki);
        assert_eq!(spki_of(&point), Some(spki));
        let mut off_curve = point;
        off_curve[0] = 0x02;
        assert_eq!(spki_of(&off_curve), None);
    }

    #[test]
    fn an_ephemeral_agreement_feeds_the_same_bind_keys_on_both_sides() {
        let g = crypto::Ephemeral::generate().unwrap();
        let h = crypto::Ephemeral::generate().unwrap();
        let g_spki = g.public_key().unwrap();
        let h_spki = h.public_key().unwrap();
        let dh_g = g.agree(&h_spki).unwrap();
        let dh_h = h.agree(&g_spki).unwrap();
        assert!(dh_g == dh_h, "mismatch");
        let uw = fixed_uw();
        let ticket = fixed_ticket();
        let frame1 = [1; 21];
        let frame2 = [2; 126];
        let frame3 = [3; 102];
        let th = transcript_hash(&frame1, &frame2, &frame3).unwrap();
        let keys_g = bind_keys(&uw, &ticket, &th, &dh_g).unwrap();
        let keys_h = bind_keys(&uw, &ticket, &th, &dh_h).unwrap();
        assert!(keys_g.g2h == keys_h.g2h, "mismatch");
        assert!(keys_g.h2g == keys_h.h2g, "mismatch");
        assert!(keys_g.g2h != keys_g.h2g, "mismatch");
    }

    #[test]
    fn pre_depends_on_every_input() {
        let uw = fixed_uw();
        let ticket = fixed_ticket();
        let challenge = [0x11; 16];
        let mut e_g = [0x22; 65];
        e_g[0] = 4;
        let n_g = [0x33; 32];
        let base = pre(&uw, &ticket, &challenge, &e_g, &n_g).unwrap();

        let mut other = ticket;
        other[0] ^= 1;
        assert!(pre(&uw, &other, &challenge, &e_g, &n_g).unwrap() != base, "mismatch");
        let mut other = challenge;
        other[0] ^= 1;
        assert!(pre(&uw, &ticket, &other, &e_g, &n_g).unwrap() != base, "mismatch");
        let mut other = e_g;
        other[1] ^= 1;
        assert!(pre(&uw, &ticket, &challenge, &other, &n_g).unwrap() != base, "mismatch");
        let mut other = n_g;
        other[0] ^= 1;
        assert!(pre(&uw, &ticket, &challenge, &e_g, &other).unwrap() != base, "mismatch");
        let other = [0x44; 32];
        assert!(pre(&other, &ticket, &challenge, &e_g, &n_g).unwrap() != base, "mismatch");

        let mut same = base;
        assert!(eq16(&base, &same));
        same[15] ^= 1;
        assert!(!eq16(&base, &same));
    }
}
