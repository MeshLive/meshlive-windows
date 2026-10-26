//! Identity fingerprint text (CF-HS-47) and the 60-digit safety code
//! (CF-HS-48), Android `IdentityVerification`: the read-aloud comparison a
//! PC without a camera uses instead of scanning. A contact confirmed only by
//! this code is "Checked by code" (no shield) and does not exchange leases
//! until scanned both ways (PROTOCOL "Code-only comparison").

const DOMAIN: &[u8] = b"BluetoothVideoMesh/IdentityVerification/SafetyCode/2";
pub const ITERATIONS: u32 = 5200;

/// 64 lowercase hex digits in 16 groups of 4.
pub fn fingerprint_text(fingerprint: &[u8; 32]) -> String {
    let hex: String = fingerprint.iter().map(|b| format!("{b:02x}")).collect();
    hex.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(" ")
}

/// One party's half: six 5-digit groups from `h5200`.
pub fn half(fingerprint: &[u8; 32]) -> Result<String, crate::crypto::Error> {
    let h = crate::crypto::sha512_chain(DOMAIN, fingerprint, ITERATIONS)?;
    Ok((0..6)
        .map(|g| {
            let value = h[5 * g..5 * g + 5].iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
            format!("{:05}", value % 100_000)
        })
        .collect::<Vec<_>>()
        .join(" "))
}

/// The pair code both phones show: the two halves sorted as strings.
pub fn pair_code(a: &[u8; 32], b: &[u8; 32]) -> Result<String, crate::crypto::Error> {
    let (x, y) = (half(a)?, half(b)?);
    Ok(if x <= y { format!("{x} {y}") } else { format!("{y} {x}") })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex32(text: &str) -> [u8; 32] {
        std::array::from_fn(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap())
    }

    #[test]
    fn the_shared_vectors_give_the_same_fingerprint_text_halves_and_pair_code() {
        let file: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/conformance/bvmid2.json")).unwrap();
        let vector = |id: &str| file["vectors"].as_array().unwrap().iter().find(|v| v["id"] == id).unwrap().clone();
        // bvmid2-001: fingerprint = SHA-256 of the key encoding.
        let v = vector("bvmid2-001");
        let fingerprint = crate::crypto::sha256(b"abc").unwrap();
        assert_eq!(fingerprint, hex32(v["expected"]["fingerprint"].as_str().unwrap()));
        assert_eq!(fingerprint_text(&fingerprint).split(' ').count(), 16);
        assert!(fingerprint_text(&fingerprint).starts_with("ba78 16bf 8f01"));
        // bvmid2-003/004: halves.
        for id in ["bvmid2-003", "bvmid2-004"] {
            let v = vector(id);
            let fp = hex32(v["inputs"]["fingerprint"].as_str().unwrap());
            assert_eq!(half(&fp).unwrap(), v["expected"]["half"].as_str().unwrap(), "{id}");
        }
        // bvmid2-005: the pair code, the same from both sides.
        let v = vector("bvmid2-005");
        let a = hex32(v["inputs"]["fingerprint_a"].as_str().unwrap());
        let b = hex32(v["inputs"]["fingerprint_b"].as_str().unwrap());
        let code = pair_code(&a, &b).unwrap();
        assert_eq!(code, v["expected"]["pair_code"].as_str().unwrap());
        assert_eq!(code, pair_code(&b, &a).unwrap());
        assert_eq!(code.split(' ').count(), 12);
    }
}
