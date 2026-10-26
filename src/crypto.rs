//! Windows CNG SHA-256, HKDF and P-256. Rust handles wire encodings only;
//! the OS performs hashing, key derivation, curve validation and arithmetic.
use windows::{Win32::Security::Cryptography::*, core::PCWSTR};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Length,
    PublicKey,
    Signature,
    KeyPolicy,
    Native(i32),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CNG protocol crypto error: {self:?}")
    }
}
impl std::error::Error for Error {}

fn check(status: i32) -> Result<(), Error> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Native(status))
    }
}
struct Algorithm(BCRYPT_ALG_HANDLE);
impl Algorithm {
    fn new(name: PCWSTR) -> Result<Self, Error> {
        Self::with_flags(name, BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0))
    }
    fn with_flags(
        name: PCWSTR,
        flags: BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS,
    ) -> Result<Self, Error> {
        let mut handle = BCRYPT_ALG_HANDLE::default();
        check(unsafe { BCryptOpenAlgorithmProvider(&mut handle, name, PCWSTR::null(), flags) }.0)?;
        Ok(Self(handle))
    }
}
impl Drop for Algorithm {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptCloseAlgorithmProvider(self.0, 0);
        }
    }
}
struct Key(BCRYPT_KEY_HANDLE);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroyKey(self.0);
        }
    }
}
struct Secret(BCRYPT_SECRET_HANDLE);
impl Drop for Secret {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroySecret(self.0);
        }
    }
}

pub fn sha256(input: &[u8]) -> Result<[u8; 32], Error> {
    if input.len() > u32::MAX as usize {
        return Err(Error::Length);
    }
    let algorithm = Algorithm::new(BCRYPT_SHA256_ALGORITHM)?;
    let mut output = [0; 32];
    check(unsafe { BCryptHash(algorithm.0, None, input, &mut output) }.0)?;
    Ok(output)
}

/// CF-HS-48: `h1 = SHA-512(seed || suffix)`, `h(i+1) = SHA-512(h(i) || suffix)`,
/// returning `h(rounds)` (one provider handle for the whole chain).
pub fn sha512_chain(seed: &[u8], suffix: &[u8], rounds: u32) -> Result<[u8; 64], Error> {
    if seed.len() + suffix.len() > u32::MAX as usize || rounds == 0 {
        return Err(Error::Length);
    }
    let algorithm = Algorithm::new(BCRYPT_SHA512_ALGORITHM)?;
    let mut input = [seed, suffix].concat();
    let mut output = [0u8; 64];
    for _ in 0..rounds {
        check(unsafe { BCryptHash(algorithm.0, None, &input, &mut output) }.0)?;
        input.clear();
        input.extend_from_slice(&output);
        input.extend_from_slice(suffix);
    }
    Ok(output)
}

pub fn random_nonce() -> Result<[u8; 32], Error> {
    let mut nonce = [0; 32];
    check(unsafe { BCryptGenRandom(None, &mut nonce, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.0)?;
    Ok(nonce)
}

/// CNG supplies HMAC itself; protocol MACs and HKDF-Extract use this operation.
pub fn hmac_sha256(key: &[u8], input: &[u8]) -> Result<Zeroizing<[u8; 32]>, Error> {
    if key.len() > u32::MAX as usize || input.len() > u32::MAX as usize {
        return Err(Error::Length);
    }
    let algorithm = Algorithm::with_flags(BCRYPT_SHA256_ALGORITHM, BCRYPT_ALG_HANDLE_HMAC_FLAG)?;
    let mut output = Zeroizing::new([0; 32]);
    check(unsafe { BCryptHash(algorithm.0, Some(key), input, &mut output[..]) }.0)?;
    Ok(output)
}

pub(crate) fn validate_agreement_public(spki: &[u8]) -> Result<(), Error> {
    EccKey::import_public(spki, false).map(drop)
}

/// Complete RFC 5869 extract/expand in CNG, not an HMAC-based Rust substitute.
pub fn hkdf_sha256<const N: usize>(
    input: &[u8],
    salt: &[u8],
    info: &[u8],
) -> Result<Zeroizing<[u8; N]>, Error> {
    hkdf(input, Some(salt), info)
}

/// Expands an already extracted 32-byte PRK without extracting it a second time.
pub fn hkdf_expand<const N: usize>(
    prk: &[u8; 32],
    info: &[u8],
) -> Result<Zeroizing<[u8; N]>, Error> {
    hkdf(prk, None, info)
}

fn hkdf<const N: usize>(
    input: &[u8],
    salt: Option<&[u8]>,
    info: &[u8],
) -> Result<Zeroizing<[u8; N]>, Error> {
    if !(1..=255 * 32).contains(&N)
        || input.len() > u32::MAX as usize
        || salt.is_some_and(|value| value.len() > u32::MAX as usize)
        || info.len() > u32::MAX as usize
    {
        return Err(Error::Length);
    }
    let algorithm = Algorithm::new(BCRYPT_HKDF_ALGORITHM)?;
    let mut key = BCRYPT_KEY_HANDLE::default();
    check(unsafe { BCryptGenerateSymmetricKey(algorithm.0, &mut key, None, input, 0) }.0)?;
    let key = Key(key);
    let hash_name: Vec<u8> = "SHA256\0"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    check(
        unsafe {
            BCryptSetProperty(
                BCRYPT_HANDLE(key.0.0),
                BCRYPT_HKDF_HASH_ALGORITHM,
                &hash_name,
                0,
            )
        }
        .0,
    )?;
    // The public entry points distinguish IKM from an already extracted PRK.
    if let Some(salt) = salt {
        let salt = if salt.is_empty() { &[0; 32][..] } else { salt };
        check(
            unsafe {
                BCryptSetProperty(
                    BCRYPT_HANDLE(key.0.0),
                    BCRYPT_HKDF_SALT_AND_FINALIZE,
                    salt,
                    0,
                )
            }
            .0,
        )?;
    } else {
        // This property requires a NULL input, not a non-null zero-length slice.
        // The generated slice-only binding cannot express that distinction.
        #[link(name = "bcrypt")]
        unsafe extern "system" {
            #[link_name = "BCryptSetProperty"]
            fn finalize_prk(
                handle: BCRYPT_HANDLE,
                name: PCWSTR,
                input: *const u8,
                size: u32,
                flags: u32,
            ) -> windows::Win32::Foundation::NTSTATUS;
        }
        check(
            unsafe {
                finalize_prk(
                    BCRYPT_HANDLE(key.0.0),
                    BCRYPT_HKDF_PRK_AND_FINALIZE,
                    std::ptr::null(),
                    0,
                    0,
                )
            }
            .0,
        )?;
    }
    let mut buffer = BCryptBuffer {
        cbBuffer: info.len() as u32,
        BufferType: KDF_HKDF_INFO,
        pvBuffer: info.as_ptr().cast_mut().cast(),
    };
    let parameters = BCryptBufferDesc {
        ulVersion: BCRYPTBUFFER_VERSION,
        cBuffers: 1,
        pBuffers: &mut buffer,
    };
    let mut output = Zeroizing::new([0; N]);
    let mut written = 0;
    // Both descriptor and borrowed info remain live through this synchronous call.
    check(
        unsafe { BCryptKeyDerivation(key.0, Some(&parameters), &mut output[..], &mut written, 0) }
            .0,
    )?;
    if written as usize != N {
        return Err(Error::Length);
    }
    Ok(output)
}

pub(crate) const SPKI_PREFIX: [u8; 27] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04,
];
// The P-256 subgroup order, used only for integer-format range checks.
const ORDER: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63, 0x25, 0x51,
];
const PRIME: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];
// Key field first: release the native key before its algorithm provider.
struct EccKey {
    key: Key,
    _algorithm: Algorithm,
}
impl EccKey {
    fn import_public(spki: &[u8], signing: bool) -> Result<Self, Error> {
        if spki.len() != 91
            || spki[..27] != SPKI_PREFIX
            || spki[27..59] >= PRIME[..]
            || spki[59..91] >= PRIME[..]
        {
            return Err(Error::PublicKey);
        }
        let algorithm = Algorithm::new(if signing {
            BCRYPT_ECDSA_P256_ALGORITHM
        } else {
            BCRYPT_ECDH_P256_ALGORITHM
        })?;
        let mut blob = [0; 72];
        let magic = if signing {
            BCRYPT_ECDSA_PUBLIC_P256_MAGIC
        } else {
            BCRYPT_ECDH_PUBLIC_P256_MAGIC
        };
        blob[..4].copy_from_slice(&magic.to_le_bytes());
        blob[4..8].copy_from_slice(&32_u32.to_le_bytes());
        blob[8..].copy_from_slice(&spki[27..]);
        let mut key = BCRYPT_KEY_HANDLE::default();
        // Flags stay zero: never use BCRYPT_NO_KEY_VALIDATION for peer points.
        check(
            unsafe {
                BCryptImportKeyPair(algorithm.0, None, BCRYPT_ECCPUBLIC_BLOB, &mut key, &blob, 0)
            }
            .0,
        )?;
        Ok(Self {
            key: Key(key),
            _algorithm: algorithm,
        })
    }
    fn spki(&self) -> Result<[u8; 91], Error> {
        let mut blob = [0; 72];
        let mut written = 0;
        check(
            unsafe {
                BCryptExportKey(
                    self.key.0,
                    None,
                    BCRYPT_ECCPUBLIC_BLOB,
                    Some(&mut blob),
                    &mut written,
                    0,
                )
            }
            .0,
        )?;
        if written != 72 || blob[4..8] != 32_u32.to_le_bytes() {
            return Err(Error::PublicKey);
        }
        let mut output = [0; 91];
        output[..27].copy_from_slice(&SPKI_PREFIX);
        output[27..].copy_from_slice(&blob[8..]);
        Ok(output)
    }
}

/// Fresh, process-only ECDH key. It has no private export API and is consumed by
/// agreement, preventing accidental reuse as a second handshake's ephemeral.
pub struct Ephemeral(EccKey);
impl Ephemeral {
    pub fn generate() -> Result<Self, Error> {
        let algorithm = Algorithm::new(BCRYPT_ECDH_P256_ALGORITHM)?;
        let mut key = BCRYPT_KEY_HANDLE::default();
        check(unsafe { BCryptGenerateKeyPair(algorithm.0, &mut key, 256, 0) }.0)?;
        let key = Key(key);
        check(unsafe { BCryptFinalizeKeyPair(key.0, 0) }.0)?;
        Ok(Self(EccKey {
            key,
            _algorithm: algorithm,
        }))
    }
    pub fn public_key(&self) -> Result<[u8; 91], Error> {
        self.0.spki()
    }
    pub fn agree(self, peer_spki: &[u8]) -> Result<Zeroizing<[u8; 32]>, Error> {
        let peer = EccKey::import_public(peer_spki, false)?;
        let mut secret = BCRYPT_SECRET_HANDLE::default();
        check(unsafe { BCryptSecretAgreement(self.0.key.0, peer.key.0, &mut secret, 0) }.0)?;
        let secret = Secret(secret);
        let mut output = Zeroizing::new([0; 32]);
        let mut written = 0;
        check(
            unsafe {
                BCryptDeriveKey(
                    secret.0,
                    BCRYPT_KDF_RAW_SECRET,
                    None,
                    Some(&mut output[..]),
                    &mut written,
                    0,
                )
            }
            .0,
        )?;
        if written != 32 {
            return Err(Error::Length);
        }
        // Microsoft documents RAW_SECRET as little-endian; Java ECDH and this
        // protocol use a fixed-width big-endian x coordinate before HKDF.
        output.reverse();
        Ok(output)
    }
}

pub fn der_to_raw(der: &[u8]) -> Result<[u8; 64], Error> {
    if !(8..=72).contains(&der.len()) || der[0] != 0x30 || der[1] as usize != der.len() - 2 {
        return Err(Error::Signature);
    }
    let mut cursor = 2;
    let mut output = [0; 64];
    for integer in output.chunks_exact_mut(32) {
        if der.get(cursor) != Some(&2) {
            return Err(Error::Signature);
        }
        let size = *der.get(cursor + 1).ok_or(Error::Signature)? as usize;
        cursor += 2;
        if !(1..=33).contains(&size) {
            return Err(Error::Signature);
        }
        let value = der.get(cursor..cursor + size).ok_or(Error::Signature)?;
        cursor += size;
        if value[0] & 0x80 != 0 || size > 1 && value[0] == 0 && value[1] & 0x80 == 0 {
            return Err(Error::Signature);
        }
        let value = if size > 1 && value[0] == 0 {
            &value[1..]
        } else {
            value
        };
        if value.len() > 32 {
            return Err(Error::Signature);
        }
        integer[32 - value.len()..].copy_from_slice(value);
        if integer.iter().all(|b| *b == 0) || &*integer >= ORDER.as_slice() {
            return Err(Error::Signature);
        }
    }
    if cursor != der.len() {
        return Err(Error::Signature);
    }
    Ok(output)
}

/// Accepts both valid high-S and low-S DER signatures, as Android does.
pub fn verify_p256(spki: &[u8], message: &[u8], der_signature: &[u8]) -> Result<(), Error> {
    let signature = der_to_raw(der_signature)?;
    verify_p256_raw(spki, message, &signature)
}

pub fn verify_p256_raw(spki: &[u8], message: &[u8], signature: &[u8; 64]) -> Result<(), Error> {
    Verifier::new(spki)?.verify(message, signature)
}
/// Retain the validated native public key for a stream instead of importing it
/// for every signed audio record. No key validation flags or checks are relaxed.
pub(crate) struct Verifier(EccKey);
impl Verifier {
    pub(crate) fn new(spki: &[u8]) -> Result<Self, Error> {
        Ok(Self(EccKey::import_public(spki, true)?))
    }
    pub(crate) fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<(), Error> {
        validate_raw_signature(signature)?;
        let digest = sha256(message)?;
        check(
            unsafe {
                BCryptVerifySignature(self.0.key.0, None, &digest, signature, BCRYPT_FLAGS(0))
            }
            .0,
        )
    }
}
fn validate_raw_signature(signature: &[u8; 64]) -> Result<(), Error> {
    for scalar in signature.chunks_exact(32) {
        if scalar.iter().all(|value| *value == 0) || scalar >= ORDER.as_slice() {
            return Err(Error::Signature);
        }
    }
    Ok(())
}
pub fn raw_to_der(signature: &[u8; 64]) -> Result<Vec<u8>, Error> {
    validate_raw_signature(signature)?;
    let mut body = Vec::with_capacity(70);
    for scalar in signature.chunks_exact(32) {
        let first = scalar
            .iter()
            .position(|b| *b != 0)
            .ok_or(Error::Signature)?;
        let scalar = &scalar[first..];
        let pad = scalar[0] & 0x80 != 0;
        body.extend_from_slice(&[2, (scalar.len() + usize::from(pad)) as u8]);
        if pad {
            body.push(0);
        }
        body.extend_from_slice(scalar);
    }
    let mut output = vec![0x30, body.len() as u8];
    output.extend(body);
    Ok(output)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;
    fn vectors() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/android-vectors.json")).unwrap()
    }
    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }
    fn bytes(v: &Value, field: &str) -> Vec<u8> {
        hex(v[field].as_str().unwrap())
    }
    fn fixture_key(public: &[u8], private: &[u8], signing: bool) -> EccKey {
        let algorithm = Algorithm::new(if signing {
            BCRYPT_ECDSA_P256_ALGORITHM
        } else {
            BCRYPT_ECDH_P256_ALGORITHM
        })
        .unwrap();
        let mut blob = Zeroizing::new([0; 104]);
        let magic = if signing {
            BCRYPT_ECDSA_PRIVATE_P256_MAGIC
        } else {
            BCRYPT_ECDH_PRIVATE_P256_MAGIC
        };
        blob[..4].copy_from_slice(&magic.to_le_bytes());
        blob[4..8].copy_from_slice(&32_u32.to_le_bytes());
        blob[8..72].copy_from_slice(&public[27..]);
        blob[72..].copy_from_slice(private);
        let mut key = BCRYPT_KEY_HANDLE::default();
        check(
            unsafe {
                BCryptImportKeyPair(
                    algorithm.0,
                    None,
                    BCRYPT_ECCPRIVATE_BLOB,
                    &mut key,
                    &blob[..],
                    0,
                )
            }
            .0,
        )
        .unwrap();
        EccKey {
            key: Key(key),
            _algorithm: algorithm,
        }
    }
    pub(crate) fn fixture_ephemeral(public: &[u8], private: &[u8]) -> Ephemeral {
        Ephemeral(fixture_key(public, private, false))
    }
    pub(crate) struct FixtureIdentity(EccKey);
    impl FixtureIdentity {
        pub(crate) fn new(public: &[u8], private: &[u8]) -> Self {
            Self(fixture_key(public, private, true))
        }
    }
    impl crate::bmh2::SigningIdentity for FixtureIdentity {
        fn public_key(&self) -> Result<[u8; 91], Error> {
            self.0.spki()
        }
        fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
            let mut raw = [0; 64];
            let mut written = 0;
            let digest = sha256(message)?;
            check(
                unsafe {
                    BCryptSignHash(
                        self.0.key.0,
                        None,
                        &digest,
                        Some(&mut raw),
                        &mut written,
                        BCRYPT_FLAGS(0),
                    )
                }
                .0,
            )?;
            if written != 64 {
                return Err(Error::Signature);
            }
            raw_to_der(&raw)
        }
    }
    #[test]
    fn cng_sha256_and_hkdf_match_rfc5869_case_one_and_empty_salt_case_three() {
        assert_eq!(
            sha256(b"abc").unwrap().as_slice(),
            hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        let salt = hex("000102030405060708090a0b0c");
        let info = hex("f0f1f2f3f4f5f6f7f8f9");
        assert_eq!(
            hkdf_sha256::<42>(&[0x0b; 22], &salt, &info)
                .unwrap()
                .as_slice(),
            hex(
                "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
            )
        );
        assert_eq!(
            hkdf_sha256::<42>(&[0x0b; 22], &[], &[]).unwrap().as_slice(),
            hex(
                "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"
            )
        );
        assert!(matches!(
            hkdf_sha256::<0>(b"key", b"salt", b"info"),
            Err(Error::Length)
        ));
        assert!(matches!(
            hkdf_sha256::<8161>(b"key", b"salt", b"info"),
            Err(Error::Length)
        ));
    }
    #[test]
    fn all_reference_hello_signatures_and_full_identity_fingerprints_verify() {
        let v = vectors();
        for (purpose, context) in [
            ("link", "BluetoothVideoMesh/LinkHandshake/1"),
            ("endpoint", "BluetoothVideoMesh/EndToEndHandshake/1"),
        ] {
            for name in ["initiatorHello", "responderHello"] {
                let hello = &v[purpose][name];
                let mut message = context.as_bytes().to_vec();
                message.extend(bytes(hello, "unsigned"));
                verify_p256(
                    &bytes(hello, "identityKey"),
                    &message,
                    &bytes(hello, "signature"),
                )
                .unwrap();
                let hash = sha256(&bytes(hello, "identityKey")).unwrap();
                assert_eq!(&hash[..16], bytes(hello, "nodeId"));
                let fingerprint = if name == "initiatorHello" {
                    "callerFingerprint"
                } else {
                    "calleeFingerprint"
                };
                assert_eq!(hash.as_slice(), bytes(&v["identity"], fingerprint));
                message[0] ^= 1;
                assert!(
                    verify_p256(
                        &bytes(hello, "identityKey"),
                        &message,
                        &bytes(hello, "signature")
                    )
                    .is_err()
                );
            }
        }
    }
    #[test]
    fn raw_ecdh_endianness_and_hkdf_match_both_reference_directions_and_domains() {
        let v = vectors();
        for (purpose, context) in [
            ("link", "BluetoothVideoMesh/LinkSession/1"),
            ("endpoint", "BluetoothVideoMesh/EndToEndSession/1"),
        ] {
            let v = &v[purpose];
            let a = &v["initiatorHello"];
            let b = &v["responderHello"];
            let public_a = bytes(a, "ephemeralKey");
            let public_b = bytes(b, "ephemeralKey");
            let private_a = bytes(v, "initiatorEphemeralPrivate");
            let private_b = bytes(v, "responderEphemeralPrivate");
            let ephemeral_a = fixture_ephemeral(&public_a, &private_a);
            assert_eq!(ephemeral_a.public_key().unwrap().as_slice(), public_a);
            let secret_a = ephemeral_a.agree(&public_b).unwrap();
            let secret_b = fixture_ephemeral(&public_b, &private_b)
                .agree(&public_a)
                .unwrap();
            assert_eq!(*secret_a, *secret_b);
            let salt = sha256(&[bytes(a, "nonce"), bytes(b, "nonce")].concat()).unwrap();
            let transcript = sha256(&[bytes(a, "encoded"), bytes(b, "encoded")].concat()).unwrap();
            let info = [context.as_bytes(), &transcript].concat();
            let material = hkdf_sha256::<64>(&secret_a[..], &salt, &info).unwrap();
            assert_eq!(&material[..32], bytes(v, "initiatorToResponderKey"));
            assert_eq!(&material[32..], bytes(v, "responderToInitiatorKey"));
        }
    }
    #[test]
    fn generated_ephemerals_agree_and_peer_points_are_validated() {
        let a = Ephemeral::generate().unwrap();
        let b = Ephemeral::generate().unwrap();
        let pa = a.public_key().unwrap();
        let pb = b.public_key().unwrap();
        assert_ne!(pa, pb);
        assert_eq!(*a.agree(&pb).unwrap(), *b.agree(&pa).unwrap());
        let mut invalid = pa;
        invalid[27..].fill(0);
        assert!(Ephemeral::generate().unwrap().agree(&invalid).is_err());
        invalid[0] ^= 1;
        assert!(matches!(
            Ephemeral::generate().unwrap().agree(&invalid),
            Err(Error::PublicKey)
        ));
        for cut in 0..91 {
            assert!(matches!(
                Ephemeral::generate().unwrap().agree(&pa[..cut]),
                Err(Error::PublicKey)
            ));
        }
        let trailing = [pa.as_slice(), &[0]].concat();
        assert!(matches!(
            Ephemeral::generate().unwrap().agree(&trailing),
            Err(Error::PublicKey)
        ));
    }
    #[test]
    fn der_integers_reject_zero_negative_overlong_out_of_range_and_trailing_bytes() {
        for signature in [
            vec![0x30, 6, 2, 1, 0, 2, 1, 1],
            vec![0x30, 6, 2, 1, 0x80, 2, 1, 1],
            vec![0x30, 7, 2, 2, 0, 1, 2, 1, 1],
            [vec![0x30, 38, 2, 33, 0], ORDER.to_vec(), vec![2, 1, 1]].concat(),
            vec![0x30, 6, 2, 1, 1, 2, 1, 1, 0],
            vec![0x30, 0x81, 6, 2, 1, 1, 2, 1, 1],
        ] {
            assert_eq!(der_to_raw(&signature), Err(Error::Signature));
        }
        assert!(der_to_raw(&[0x30, 6, 2, 1, 1, 2, 1, 1]).is_ok());
        for size in 0..8 {
            assert_eq!(der_to_raw(&[0; 8][..size]), Err(Error::Signature));
        }
    }
}
