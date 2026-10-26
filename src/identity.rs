//! Current-user CNG signing identity. Opening never creates/replaces a key;
//! first-run creation is a separate explicit operation and never overwrites one.
use crate::{
    bmh2::SigningIdentity,
    crypto::{self, Error},
};
use std::{marker::PhantomData, rc::Rc};
use windows::{
    Win32::Security::{Cryptography::*, OBJECT_SECURITY_INFORMATION},
    core::{PCWSTR, w},
};

fn key_name() -> PCWSTR {
    if crate::app_mode::current().is_review() {
        w!("io.github.meshlive.app.review.identity.v1")
    } else {
        w!("io.github.meshlive.app.identity.v1")
    }
}
/// NTE_BAD_KEYSET: the named key does not exist.
const NTE_BAD_KEYSET_CODE: i32 = 0x8009_0016_u32 as i32;
struct Provider(NCRYPT_PROV_HANDLE);
impl Provider {
    fn open() -> Result<Self, Error> {
        let mut handle = NCRYPT_PROV_HANDLE::default();
        native(unsafe { NCryptOpenStorageProvider(&mut handle, MS_KEY_STORAGE_PROVIDER, 0) })?;
        Ok(Self(handle))
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        unsafe {
            let _ = NCryptFreeObject(NCRYPT_HANDLE(self.0.0));
        }
    }
}
struct Key(NCRYPT_KEY_HANDLE);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = NCryptFreeObject(NCRYPT_HANDLE(self.0.0));
        }
    }
}
fn native(result: windows::core::Result<()>) -> Result<(), Error> {
    result.map_err(|e| Error::Native(e.code().0))
}
fn property(key: &Key, name: PCWSTR) -> Result<u32, Error> {
    let mut bytes = [0; 4];
    let mut written = 0;
    native(unsafe {
        NCryptGetProperty(
            NCRYPT_HANDLE(key.0.0),
            name,
            Some(&mut bytes),
            &mut written,
            OBJECT_SECURITY_INFORMATION(0),
        )
    })?;
    if written != 4 {
        return Err(Error::KeyPolicy);
    }
    Ok(u32::from_le_bytes(bytes))
}
// Native handles are not cloned. Release key before provider and keep signing
// on the protocol worker, alongside the non-Send BCrypt handshake/record keys.
pub struct Identity {
    key: Key,
    _provider: Provider,
    public: [u8; 91],
    _worker: PhantomData<Rc<()>>,
}
impl Identity {
    /// Reads only the existing app key. Access, corruption or missing-key errors
    /// are returned; callers must not silently regenerate an established identity.
    pub fn open_existing() -> Result<Self, Error> {
        let provider = Provider::open()?;
        let mut key = NCRYPT_KEY_HANDLE::default();
        native(unsafe {
            NCryptOpenKey(
                provider.0,
                &mut key,
                key_name(),
                CERT_KEY_SPEC(0),
                NCRYPT_SILENT_FLAG,
            )
        })?;
        Self::validate(provider, Key(key))
    }
    /// Only the explicit first-run identity owner should call this. No overwrite
    /// or machine-key flag, certificate, account or distribution signing is used.
    pub fn create_new() -> Result<Self, Error> {
        let provider = Provider::open()?;
        let key = Self::create(&provider, key_name())?;
        Self::validate(provider, key)
    }
    /// Android `getOrCreate`: the existing app key, or a new one only when
    /// none exists (NTE_BAD_KEYSET). Any other failure (access, corruption,
    /// policy) is returned and never replaced by a new identity. The bool is
    /// true when the key was created now.
    pub fn open_or_create() -> Result<(Self, bool), Error> {
        match Self::open_existing() {
            Ok(identity) => Ok((identity, false)),
            Err(Error::Native(NTE_BAD_KEYSET_CODE)) => Self::create_new().map(|i| (i, true)),
            Err(error) => Err(error),
        }
    }
    /// The emergency wipe: deletes the persisted app key (a new node ID is
    /// created on the next launch). Ok(false) when there was none.
    pub fn delete_persisted() -> Result<bool, Error> {
        Self::delete_named(key_name())
    }
    /// A persisted key under a test name (wipe tests never touch the app's key).
    #[cfg(test)]
    pub(crate) fn create_test_key(name: &str) -> Result<(), Error> {
        let provider = Provider::open()?;
        let name = windows::core::HSTRING::from(name);
        let key = Self::create(&provider, PCWSTR(name.as_ptr()))?;
        drop(key);
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn delete_test_key(name: &str) -> Result<bool, Error> {
        let name = windows::core::HSTRING::from(name);
        Self::delete_named(PCWSTR(name.as_ptr()))
    }
    fn delete_named(name: PCWSTR) -> Result<bool, Error> {
        let provider = Provider::open()?;
        let mut key = NCRYPT_KEY_HANDLE::default();
        match native(unsafe {
            NCryptOpenKey(provider.0, &mut key, name, CERT_KEY_SPEC(0), NCRYPT_SILENT_FLAG)
        }) {
            Ok(()) => {}
            Err(Error::Native(NTE_BAD_KEYSET_CODE)) => return Ok(false),
            Err(error) => return Err(error),
        }
        // NCryptDeleteKey frees the handle on success; free it only on failure.
        match native(unsafe { NCryptDeleteKey(key, NCRYPT_SILENT_FLAG.0) }) {
            Ok(()) => Ok(true),
            Err(error) => {
                drop(Key(key));
                Err(error)
            }
        }
    }
    /// An unnamed, non-persisted, non-exportable signing key for test tools:
    /// nothing is stored, and the key disappears with this value.
    pub fn ephemeral() -> Result<Self, Error> {
        let provider = Provider::open()?;
        let key = Self::create(&provider, PCWSTR::null())?;
        Self::validate(provider, key)
    }
    fn create(provider: &Provider, name: PCWSTR) -> Result<Key, Error> {
        let mut key = NCRYPT_KEY_HANDLE::default();
        native(unsafe {
            NCryptCreatePersistedKey(
                provider.0,
                &mut key,
                NCRYPT_ECDSA_P256_ALGORITHM,
                name,
                CERT_KEY_SPEC(0),
                NCRYPT_FLAGS(0),
            )
        })?;
        let key = Key(key);
        native(unsafe {
            NCryptSetProperty(
                NCRYPT_HANDLE(key.0.0),
                NCRYPT_EXPORT_POLICY_PROPERTY,
                &0_u32.to_le_bytes(),
                NCRYPT_FLAGS(0),
            )
        })?;
        native(unsafe {
            NCryptSetProperty(
                NCRYPT_HANDLE(key.0.0),
                NCRYPT_KEY_USAGE_PROPERTY,
                &NCRYPT_ALLOW_SIGNING_FLAG.to_le_bytes(),
                NCRYPT_FLAGS(0),
            )
        })?;
        native(unsafe { NCryptFinalizeKey(key.0, NCRYPT_SILENT_FLAG) })?;
        Ok(key)
    }
    fn validate(provider: Provider, key: Key) -> Result<Self, Error> {
        if property(&key, NCRYPT_EXPORT_POLICY_PROPERTY)? != 0
            || property(&key, NCRYPT_KEY_USAGE_PROPERTY)? != NCRYPT_ALLOW_SIGNING_FLAG
            || property(&key, NCRYPT_LENGTH_PROPERTY)? != 256
        {
            return Err(Error::KeyPolicy);
        }
        let mut blob = [0; 72];
        let mut written = 0;
        native(unsafe {
            NCryptExportKey(
                key.0,
                None,
                BCRYPT_ECCPUBLIC_BLOB,
                None,
                Some(&mut blob),
                &mut written,
                NCRYPT_SILENT_FLAG,
            )
        })?;
        if written != 72
            || blob[..4] != BCRYPT_ECDSA_PUBLIC_P256_MAGIC.to_le_bytes()
            || blob[4..8] != 32_u32.to_le_bytes()
        {
            return Err(Error::PublicKey);
        }
        let mut public = [0; 91];
        public[..27].copy_from_slice(&crypto::SPKI_PREFIX);
        public[27..].copy_from_slice(&blob[8..]);
        crypto::validate_agreement_public(&public)?;
        Ok(Self {
            key,
            _provider: provider,
            public,
            _worker: PhantomData,
        })
    }
}
impl SigningIdentity for Identity {
    fn public_key(&self) -> Result<[u8; 91], Error> {
        Ok(self.public)
    }
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let digest = crypto::sha256(message)?;
        let mut raw = [0; 64];
        let mut written = 0;
        // A UI requirement fails with NTE_SILENT_CONTEXT; never retry around a
        // permission/protection prompt or relax its key policy.
        native(unsafe {
            NCryptSignHash(
                self.key.0,
                None,
                &digest,
                Some(&mut raw),
                &mut written,
                NCRYPT_SILENT_FLAG,
            )
        })?;
        if written != 64 {
            return Err(Error::Signature);
        }
        crypto::raw_to_der(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bmh2::{Capabilities, Pending, Purpose, Role},
        bmh3::{
            Purpose as V3Purpose,
            guard::Guards,
            handshake::{
                Config, InitiatorOptions, InitiatorProgress, ResponderOptions, Waiting2, Waiting3,
            },
        },
    };
    use zeroize::Zeroizing;
    fn ephemeral() -> Identity {
        // Microsoft documents NULL key name as an unnamed, non-persisted key.
        Identity::ephemeral().unwrap()
    }
    #[test]
    fn unnamed_cng_identity_signs_with_canonical_public_key_and_refuses_private_export() {
        let identity = ephemeral();
        let public = identity.public_key().unwrap();
        let signature = identity.sign(b"neutral protocol fixture").unwrap();
        crypto::verify_p256(&public, b"neutral protocol fixture", &signature).unwrap();
        assert!(crypto::verify_p256(&public, b"changed fixture", &signature).is_err());
        assert_eq!(
            property(&identity.key, NCRYPT_EXPORT_POLICY_PROPERTY).unwrap(),
            0
        );
        assert_eq!(
            property(&identity.key, NCRYPT_KEY_USAGE_PROPERTY).unwrap(),
            NCRYPT_ALLOW_SIGNING_FLAG
        );
        let mut private = Zeroizing::new([0; 104]);
        let mut written = 0;
        assert!(
            unsafe {
                NCryptExportKey(
                    identity.key.0,
                    None,
                    BCRYPT_ECCPRIVATE_BLOB,
                    None,
                    Some(&mut private[..]),
                    &mut written,
                    NCRYPT_SILENT_FLAG,
                )
            }
            .is_err()
        );
    }
    #[test]
    fn an_exportable_key_is_rejected_even_when_its_curve_and_signature_are_valid() {
        let provider = Provider::open().unwrap();
        let mut handle = NCRYPT_KEY_HANDLE::default();
        native(unsafe {
            NCryptCreatePersistedKey(
                provider.0,
                &mut handle,
                NCRYPT_ECDSA_P256_ALGORITHM,
                PCWSTR::null(),
                CERT_KEY_SPEC(0),
                NCRYPT_FLAGS(0),
            )
        })
        .unwrap();
        let key = Key(handle);
        let policy = NCRYPT_ALLOW_EXPORT_FLAG | NCRYPT_ALLOW_PLAINTEXT_EXPORT_FLAG;
        native(unsafe {
            NCryptSetProperty(
                NCRYPT_HANDLE(key.0.0),
                NCRYPT_EXPORT_POLICY_PROPERTY,
                &policy.to_le_bytes(),
                NCRYPT_FLAGS(0),
            )
        })
        .unwrap();
        native(unsafe {
            NCryptSetProperty(
                NCRYPT_HANDLE(key.0.0),
                NCRYPT_KEY_USAGE_PROPERTY,
                &NCRYPT_ALLOW_SIGNING_FLAG.to_le_bytes(),
                NCRYPT_FLAGS(0),
            )
        })
        .unwrap();
        native(unsafe { NCryptFinalizeKey(key.0, NCRYPT_SILENT_FLAG) }).unwrap();
        let mut private = Zeroizing::new([0; 104]);
        let mut written = 0;
        // Positive control proves the same native private-blob API works when
        // explicitly permitted on this disposable, unnamed test key only.
        native(unsafe {
            NCryptExportKey(
                key.0,
                None,
                BCRYPT_ECCPRIVATE_BLOB,
                None,
                Some(&mut private[..]),
                &mut written,
                NCRYPT_SILENT_FLAG,
            )
        })
        .unwrap();
        assert_eq!(written, 104);
        assert!(matches!(
            Identity::validate(provider, key),
            Err(Error::KeyPolicy)
        ));
    }
    #[test]
    fn protected_cng_identities_complete_bmh2_and_bmh3_without_private_key_material() {
        let a = ephemeral();
        let b = ephemeral();
        let caps = Capabilities {
            transport: 4,
            encode_codecs: 1,
            decode_codecs: 1,
            maximum_bitrate_kbps: 1200,
            maximum_children: 0,
        };
        for purpose in [Purpose::Link, Purpose::Endpoint] {
            let aa = Pending::start(&a, Role::Initiator, caps, purpose).unwrap();
            let bb = Pending::start(&b, Role::Responder, caps, purpose).unwrap();
            let ah = aa.hello().clone();
            let bh = bb.hello().clone();
            let ak = aa
                .finish(&bh, |peer| {
                    if peer.fingerprint == crypto::sha256(&b.public).unwrap() {
                        Ok(())
                    } else {
                        Err(crate::bmh2::Error::PeerRejected)
                    }
                })
                .unwrap();
            let bk = bb
                .finish(&ah, |peer| {
                    if peer.fingerprint == crypto::sha256(&a.public).unwrap() {
                        Ok(())
                    } else {
                        Err(crate::bmh2::Error::PeerRejected)
                    }
                })
                .unwrap();
            assert_eq!(ak.send_key(), bk.receive_key());
            assert_eq!(ak.receive_key(), bk.send_key());
        }
        for purpose in [V3Purpose::Link, V3Purpose::Endpoint] {
            let (aa, m1) = Waiting2::start(
                Config::new(purpose, &a, caps).unwrap(),
                InitiatorOptions::default(),
            )
            .unwrap();
            let mut guards = Guards::default();
            let (bb, m2) = Waiting3::respond(
                Config::new(purpose, &b, caps).unwrap(),
                &m1,
                ResponderOptions::default(),
                &mut guards,
            )
            .unwrap();
            let (progress, m3) = aa
                .receive(&m2, |peer| {
                    if peer.public_key() == &b.public {
                        Ok(())
                    } else {
                        Err(crate::bmh3::Error::Policy)
                    }
                })
                .unwrap();
            let (bs, m4) = bb
                .receive(&m3, |peer, _| {
                    if peer.public_key() == &a.public {
                        Ok(())
                    } else {
                        Err(crate::bmh3::Error::Policy)
                    }
                })
                .unwrap();
            let InitiatorProgress::Complete(asession) = progress else {
                panic!("R_FIRST expected")
            };
            assert!(m4.is_none());
            assert_eq!(asession.keys().send_key(), bs.keys().receive_key());
            assert_eq!(asession.keys().receive_key(), bs.keys().send_key());
            assert_eq!(asession.keys().exporter(), bs.keys().exporter());
        }
    }
}
