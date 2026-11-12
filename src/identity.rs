//! Current-user CNG signing identity. Opening never creates/replaces a key;
//! first-run creation is a separate explicit operation and never overwrites one.
//!
//! Experimental (SEC-11, off by default): with `MESHLIVE_TPM_IDENTITY=1` a
//! *new* identity's key is created in this PC's security chip (TPM 2.0, the
//! Microsoft Platform Crypto Provider), signing only and never exportable;
//! with no chip, or when the chip refuses, it is created by the software Key
//! Storage Provider as before. A record in the app folder (`PROVIDER_RECORD`,
//! wiped by Delete everything) says the key is in the chip; without it the
//! key is in the software provider. An existing identity keeps its key and
//! provider: moving it would change this PC's node ID.
use crate::{
    bmh2::SigningIdentity,
    crypto::{self, Error},
};
use std::{marker::PhantomData, path::PathBuf, rc::Rc, sync::{Arc, Mutex}};
use windows::{
    Win32::Security::{Cryptography::*, OBJECT_SECURITY_INFORMATION},
    core::{HSTRING, PCWSTR},
};

fn key_name() -> &'static str {
    if crate::app_mode::current().is_review() {
        "io.github.meshlive.app.review.identity.v1"
    } else {
        "io.github.meshlive.app.identity.v1"
    }
}
/// NTE_BAD_KEYSET: the named key does not exist.
const NTE_BAD_KEYSET_CODE: i32 = 0x8009_0016_u32 as i32;

/// Where an identity's private key is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyProvider {
    /// This PC's security chip (TPM), `MS_PLATFORM_CRYPTO_PROVIDER`.
    Platform,
    /// Windows' software Key Storage Provider, `MS_KEY_STORAGE_PROVIDER`.
    Software,
}
impl KeyProvider {
    fn cng(self) -> PCWSTR {
        match self {
            Self::Platform => MS_PLATFORM_CRYPTO_PROVIDER,
            Self::Software => MS_KEY_STORAGE_PROVIDER,
        }
    }
}

/// Experimental, off by default: new identities in the security chip.
pub const TPM_SWITCH: &str = "MESHLIVE_TPM_IDENTITY";
pub fn tpm_identity_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var(TPM_SWITCH).ok().as_deref().map(str::trim) == Some("1"))
}
/// The app-folder file that records a key held by the security chip.
pub const PROVIDER_RECORD: &str = "identity-key-provider";
const PLATFORM_RECORD: &[u8] = b"platform\n";

fn io_error(error: std::io::Error) -> Error {
    Error::Native(error.raw_os_error().unwrap_or(-1))
}

/// One named key and the record of its provider (the app's key, or a test's).
struct Slot {
    name: HSTRING,
    record: Option<PathBuf>,
}
impl Slot {
    fn app() -> Self {
        Self { name: HSTRING::from(key_name()), record: crate::sealed::default_dir().map(|dir| dir.join(PROVIDER_RECORD)) }
    }
    #[cfg(test)]
    fn name(&self) -> PCWSTR {
        PCWSTR(self.name.as_ptr())
    }
    /// The recorded provider: the chip only with the exact record; an
    /// unreadable or unknown record is an error (never a silent new key).
    fn recorded(&self) -> Result<KeyProvider, Error> {
        let Some(path) = &self.record else { return Ok(KeyProvider::Software) };
        match std::fs::read(path) {
            Ok(bytes) if bytes == PLATFORM_RECORD => Ok(KeyProvider::Platform),
            Ok(_) => Err(Error::KeyPolicy),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(KeyProvider::Software),
            Err(error) => Err(io_error(error)),
        }
    }
    fn write_record(&self) -> Result<(), Error> {
        let path = self.record.as_ref().ok_or(Error::KeyPolicy)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_error)?;
        }
        std::fs::write(path, PLATFORM_RECORD).map_err(io_error)
    }
    fn remove_record(&self) -> Result<(), Error> {
        let Some(path) = &self.record else { return Ok(()) };
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }
}

struct Provider(NCRYPT_PROV_HANDLE);
impl Provider {
    fn open(kind: KeyProvider) -> Result<Self, Error> {
        let mut handle = NCRYPT_PROV_HANDLE::default();
        native(unsafe { NCryptOpenStorageProvider(&mut handle, kind.cng(), 0) })?;
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
/// The native key and its provider, shared by every `Identity` handle of one
/// open. CNG key handles are plain tokens, not tied to a thread, and every use
/// of the key (signing) goes through `sign_lock`, so concurrent calls on
/// different threads never use the handle at the same time. Key is released
/// before its provider.
struct Opened {
    key: Key,
    _provider: Provider,
    sign_lock: Mutex<()>,
}
// Safety: see `Opened`; the handles are only used under `sign_lock`, and are
// freed once, when the last handle is dropped.
unsafe impl Send for Opened {}
unsafe impl Sync for Opened {}

/// A handle to this PC's identity key. Cloning is cheap: the process keeps
/// the opened key (the security chip needs about 46 ms per open), and
/// `Identity::open_or_create` hands out another handle to it, so a call never
/// opens the key again. Dropping a handle never closes the key; Delete
/// everything and process exit do (`Identity::release_cached`). The handle
/// itself stays with the protocol worker, alongside the non-Send BCrypt
/// handshake/record keys.
pub struct Identity {
    opened: Arc<Opened>,
    kind: KeyProvider,
    public: [u8; 91],
    _worker: PhantomData<Rc<()>>,
}
impl Clone for Identity {
    fn clone(&self) -> Self {
        Self { opened: self.opened.clone(), kind: self.kind, public: self.public, _worker: PhantomData }
    }
}

/// The process's open identity key. Reads of it (and the one open that fills
/// it) are serialized by the lock; a failed open is never cached.
struct OpenedCache(Mutex<Option<Identity>>);
// Safety: the `Identity` inside is only cloned or dropped under the lock, and
// each clone is moved to one worker; its handles are `Send + Sync` (`Opened`).
unsafe impl Send for OpenedCache {}
unsafe impl Sync for OpenedCache {}
impl OpenedCache {
    const fn new() -> Self {
        Self(Mutex::new(None))
    }
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Identity>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    /// The cached identity, or the one `open` returns (cached when it works).
    fn get_or_open(&self, open: impl FnOnce() -> Result<(Identity, bool), Error>) -> Result<(Identity, bool), Error> {
        let mut slot = self.slot();
        if let Some(identity) = slot.as_ref() {
            return Ok((identity.clone(), false));
        }
        let (identity, created) = open()?;
        *slot = Some(identity.clone());
        Ok((identity, created))
    }
    /// Lets go of the process's handle (the key is closed once every handle
    /// given out has been dropped).
    fn release(&self) {
        *self.slot() = None;
    }
}
static APP_IDENTITY: OpenedCache = OpenedCache::new();
/// winerror.h codes meaning this PC has no usable security chip for a P-256
/// key: none found, the TPM service missing or disabled, the provider DLL
/// missing or failing, or a chip without P-256. Only these let a new
/// identity fall back to software, or a wipe skip the chip when no record
/// says the key is there; every other chip error is reported.
fn chip_unavailable(error: Error) -> bool {
    use windows::Win32::Foundation::{
        NTE_BAD_ALGID, NTE_BAD_PROVIDER, NTE_DEVICE_NOT_FOUND, NTE_NOT_SUPPORTED, NTE_PROV_DLL_NOT_FOUND,
        NTE_PROVIDER_DLL_FAIL, TBS_E_SERVICE_DISABLED, TBS_E_SERVICE_NOT_RUNNING, TBS_E_TPM_NOT_FOUND,
    };
    let Error::Native(code) = error else { return false };
    [
        NTE_NOT_SUPPORTED, NTE_BAD_ALGID, NTE_DEVICE_NOT_FOUND, NTE_PROV_DLL_NOT_FOUND, NTE_PROVIDER_DLL_FAIL,
        NTE_BAD_PROVIDER, TBS_E_TPM_NOT_FOUND, TBS_E_SERVICE_NOT_RUNNING, TBS_E_SERVICE_DISABLED,
    ]
    .iter()
    .any(|known| known.0 == code)
}
fn missing(error: &Error) -> bool {
    *error == Error::Native(NTE_BAD_KEYSET_CODE)
}
const NTE_EXISTS_CODE: i32 = windows::Win32::Foundation::NTE_EXISTS.0;

/// The key operations the provider rules need: CNG in the app, a fake in
/// tests (so failing chips can be tested).
trait KeyStore {
    type Key;
    /// Opens and checks the named key; NTE_BAD_KEYSET when there is none.
    fn open(&mut self, kind: KeyProvider, name: &HSTRING) -> Result<Self::Key, Error>;
    /// Creates and finalizes a new named key; never overwrites one (NTE_EXISTS).
    fn create(&mut self, kind: KeyProvider, name: &HSTRING) -> Result<(), Error>;
    /// Deletes the named key; Ok(false) when there was none.
    fn delete(&mut self, kind: KeyProvider, name: &HSTRING) -> Result<bool, Error>;
}
struct Cng;
impl KeyStore for Cng {
    type Key = Identity;
    fn open(&mut self, kind: KeyProvider, name: &HSTRING) -> Result<Identity, Error> {
        Identity::open_named(kind, PCWSTR(name.as_ptr()))
    }
    fn create(&mut self, kind: KeyProvider, name: &HSTRING) -> Result<(), Error> {
        let provider = Provider::open(kind)?;
        Identity::create(&provider, PCWSTR(name.as_ptr()), kind).map(drop)
    }
    fn delete(&mut self, kind: KeyProvider, name: &HSTRING) -> Result<bool, Error> {
        Identity::delete_from(kind, PCWSTR(name.as_ptr()))
    }
}

/// The key from the provider its record names; errors are returned as they are.
fn open_slot<S: KeyStore>(store: &mut S, slot: &Slot) -> Result<S::Key, Error> {
    let kind = slot.recorded()?;
    store.open(kind, &slot.name)
}

/// Android `getOrCreate` with the provider rules. A new identity only when
/// there is no record, no software key and no chip key under the name. A
/// recorded chip identity is never replaced, even when its key is gone: that
/// needs Delete everything (Settings says so).
fn open_or_create_slot<S: KeyStore>(store: &mut S, slot: &Slot, chip: bool) -> Result<(S::Key, bool), Error> {
    let recorded = slot.recorded()?;
    match store.open(recorded, &slot.name) {
        Ok(key) => return Ok((key, false)),
        Err(error) if recorded == KeyProvider::Platform || !missing(&error) => return Err(error),
        Err(_) => {}
    }
    // With the switch on, a chip key under this name whose record was lost is
    // this PC's identity: record it again and keep it. With it off a first run
    // never touches the chip, as before (and a wipe that let an unrecorded chip
    // error pass can never bring a deleted identity back).
    if chip {
        match store.open(KeyProvider::Platform, &slot.name) {
            Ok(key) => {
                slot.write_record()?;
                return Ok((key, false));
            }
            Err(error) if missing(&error) || chip_unavailable(error) => {}
            Err(error) => return Err(error),
        }
    }
    create_slot(store, slot, chip).map(|key| (key, true))
}

/// Why the chip did not take a new key.
enum ChipFailure {
    /// No usable chip on this PC: a software key may follow.
    Unavailable,
    /// Anything else: an error, never a weaker key.
    Failed(Error),
}

/// A new key: in the chip when `chip` is asked for; in software otherwise, or
/// when this PC has no usable chip. Any other chip failure is returned.
fn create_slot<S: KeyStore>(store: &mut S, slot: &Slot, chip: bool) -> Result<S::Key, Error> {
    if chip {
        match create_in_chip(store, slot) {
            Ok(key) => return Ok(key),
            Err(ChipFailure::Unavailable) => {}
            Err(ChipFailure::Failed(error)) => return Err(error),
        }
    }
    store.create(KeyProvider::Software, &slot.name)?;
    store.open(KeyProvider::Software, &slot.name)
}

/// The record goes first, so a chip key is never left unrecorded; the record
/// goes again only once no chip key under the name is left.
fn create_in_chip<S: KeyStore>(store: &mut S, slot: &Slot) -> Result<S::Key, ChipFailure> {
    slot.write_record().map_err(ChipFailure::Failed)?;
    if let Err(error) = store.create(KeyProvider::Platform, &slot.name) {
        // A same-named chip key keeps its record: it may be this PC's identity.
        if error == Error::Native(NTE_EXISTS_CODE) {
            return Err(ChipFailure::Failed(error));
        }
        // Nothing was persisted (a key exists only once finalized).
        slot.remove_record().map_err(ChipFailure::Failed)?;
        return Err(if chip_unavailable(error) { ChipFailure::Unavailable } else { ChipFailure::Failed(error) });
    }
    match store.open(KeyProvider::Platform, &slot.name) {
        Ok(key) => Ok(key),
        Err(error) => {
            // The made key fails its check: delete it; a failed deletion keeps
            // the record (and reports that error), and no software key follows.
            store.delete(KeyProvider::Platform, &slot.name).map_err(ChipFailure::Failed)?;
            slot.remove_record().map_err(ChipFailure::Failed)?;
            Err(ChipFailure::Failed(error))
        }
    }
}

/// Deletes the key from both providers. A chip error keeps a wipe
/// incomplete (the record stays, and the wipe resumes at the next launch)
/// only where a chip key may exist: a record names the chip (or is
/// unreadable), or the switch is on and this PC has a usable chip. With no
/// record and the switch off, MeshLive never made a chip key: the chip is
/// asked once more, and a second error is logged and lets the wipe complete,
/// so a busy chip never holds a default user's wipe.
fn delete_slot<S: KeyStore>(store: &mut S, slot: &Slot, chip: bool) -> Result<bool, Error> {
    let software = store.delete(KeyProvider::Software, &slot.name);
    let recorded = slot.recorded() != Ok(KeyProvider::Software);
    let chip_deleted = match store.delete(KeyProvider::Platform, &slot.name) {
        Err(error) if !recorded && chip_unavailable(error) => Ok(false),
        Err(_) if !recorded && !chip => store.delete(KeyProvider::Platform, &slot.name).or_else(|error| {
            eprintln!("wipe: the security chip did not answer ({error:?}); no record names it, so the wipe goes on");
            Ok(false)
        }),
        other => other,
    };
    Ok(software? | chip_deleted?)
}

impl Identity {
    /// Reads only the existing app key. Access, corruption or missing-key errors
    /// are returned; callers must not silently regenerate an established identity.
    pub fn open_existing() -> Result<Self, Error> {
        APP_IDENTITY.get_or_open(|| open_slot(&mut Cng, &Slot::app()).map(|identity| (identity, false))).map(|(identity, _)| identity)
    }
    fn open_named(kind: KeyProvider, name: PCWSTR) -> Result<Self, Error> {
        let provider = Provider::open(kind)?;
        let mut key = NCRYPT_KEY_HANDLE::default();
        native(unsafe {
            NCryptOpenKey(
                provider.0,
                &mut key,
                name,
                CERT_KEY_SPEC(0),
                NCRYPT_SILENT_FLAG,
            )
        })?;
        Self::validate(provider, Key(key), kind)
    }
    /// Only the explicit first-run identity owner should call this. No overwrite
    /// or machine-key flag, certificate, account or distribution signing is used.
    /// Refused while a record says the identity is in the chip.
    pub fn create_new() -> Result<Self, Error> {
        let slot = Slot::app();
        if slot.recorded()? == KeyProvider::Platform {
            return Err(Error::KeyPolicy);
        }
        create_slot(&mut Cng, &slot, tpm_identity_enabled())
    }
    /// Android `getOrCreate`: the existing app key, or a new one only when
    /// none exists anywhere (NTE_BAD_KEYSET and no chip record). Any other
    /// failure (access, corruption, policy, an unreadable record, a recorded
    /// chip identity that cannot be opened) is returned and never replaced by
    /// a new identity. The bool is true when the key was created now.
    /// The key is opened once per process (SEC-11: a chip open costs about
    /// 46 ms); later calls get another handle to it.
    pub fn open_or_create() -> Result<(Self, bool), Error> {
        APP_IDENTITY.get_or_open(|| open_or_create_slot(&mut Cng, &Slot::app(), tpm_identity_enabled()))
    }
    /// Lets go of the process's open identity key: Delete everything does it
    /// before deleting the key, and the app does it on exit. The next
    /// `open_or_create` opens the key again. A handle already given out keeps
    /// working until it is dropped.
    pub fn release_cached() {
        APP_IDENTITY.release();
    }
    /// Where this identity's key is kept.
    pub fn key_provider(&self) -> KeyProvider {
        self.kind
    }
    /// A record says (or a damaged record may say) the app's identity is in
    /// the chip: when it cannot be opened, Settings says so; nothing replaces it.
    pub fn recorded_in_chip() -> bool {
        Slot::app().recorded() != Ok(KeyProvider::Software)
    }
    /// The emergency wipe: deletes the persisted app key from the software
    /// provider and from the security chip (a new node ID is created on the
    /// next launch). Ok(false) when there was none.
    pub fn delete_persisted() -> Result<bool, Error> {
        Self::release_cached();
        delete_slot(&mut Cng, &Slot::app(), tpm_identity_enabled())
    }
    /// A persisted key under a test name (wipe tests never touch the app's key).
    #[cfg(test)]
    pub(crate) fn create_test_key(name: &str) -> Result<(), Error> {
        Cng.create(KeyProvider::Software, &HSTRING::from(name))
    }
    #[cfg(test)]
    pub(crate) fn delete_test_key(name: &str) -> Result<bool, Error> {
        delete_slot(&mut Cng, &Slot { name: HSTRING::from(name), record: None }, false)
    }
    /// A key under a test name made as `create_new` makes it (the chip when
    /// asked and available), its record in `dir`; for the wipe tests.
    #[cfg(test)]
    pub(crate) fn create_test_key_in(name: &str, dir: &std::path::Path, chip: bool) -> Result<KeyProvider, Error> {
        create_slot(&mut Cng, &Slot { name: HSTRING::from(name), record: Some(dir.join(PROVIDER_RECORD)) }, chip).map(|i| i.kind)
    }
    #[cfg(test)]
    pub(crate) fn delete_test_key_in(name: &str, dir: &std::path::Path) -> Result<bool, Error> {
        delete_slot(&mut Cng, &Slot { name: HSTRING::from(name), record: Some(dir.join(PROVIDER_RECORD)) }, false)
    }
    fn delete_from(kind: KeyProvider, name: PCWSTR) -> Result<bool, Error> {
        let provider = Provider::open(kind)?;
        let mut key = NCRYPT_KEY_HANDLE::default();
        match native(unsafe {
            NCryptOpenKey(provider.0, &mut key, name, CERT_KEY_SPEC(0), NCRYPT_SILENT_FLAG)
        }) {
            Ok(()) => {}
            Err(Error::Native(NTE_BAD_KEYSET_CODE)) => return Ok(false),
            Err(error) => return Err(error),
        }
        // NCryptDeleteKey frees the handle on success; free it only on failure.
        // The chip's provider refuses NCRYPT_SILENT_FLAG here (NTE_BAD_FLAGS,
        // measured 2026-11-10); its keys carry no UI policy, so 0 shows none.
        let flags = if kind == KeyProvider::Platform { 0 } else { NCRYPT_SILENT_FLAG.0 };
        match native(unsafe { NCryptDeleteKey(key, flags) }) {
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
        let provider = Provider::open(KeyProvider::Software)?;
        let key = Self::create(&provider, PCWSTR::null(), KeyProvider::Software)?;
        Self::validate(provider, key, KeyProvider::Software)
    }
    fn create(provider: &Provider, name: PCWSTR, kind: KeyProvider) -> Result<Key, Error> {
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
        let export = native(unsafe {
            NCryptSetProperty(
                NCRYPT_HANDLE(key.0.0),
                NCRYPT_EXPORT_POLICY_PROPERTY,
                &0_u32.to_le_bytes(),
                NCRYPT_FLAGS(0),
            )
        });
        // A chip key can never be exported, and the chip may refuse the
        // property; `validate` still requires it to read back as none.
        if kind == KeyProvider::Software {
            export?;
        }
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
    fn validate(provider: Provider, key: Key, kind: KeyProvider) -> Result<Self, Error> {
        // The chip reports length 0 for its ECC keys (measured 2026-11-10 on
        // the development laptop's TPM); the P-256 public blob checked below
        // proves the curve for both providers.
        let length = property(&key, NCRYPT_LENGTH_PROPERTY)?;
        if property(&key, NCRYPT_EXPORT_POLICY_PROPERTY)? != 0
            || property(&key, NCRYPT_KEY_USAGE_PROPERTY)? != NCRYPT_ALLOW_SIGNING_FLAG
            || !(length == 256 || kind == KeyProvider::Platform && length == 0)
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
            opened: Arc::new(Opened { key, _provider: provider, sign_lock: Mutex::new(()) }),
            kind,
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
        let _one_at_a_time = self.opened.sign_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        native(unsafe {
            NCryptSignHash(
                self.opened.key.0,
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
            property(&identity.opened.key, NCRYPT_EXPORT_POLICY_PROPERTY).unwrap(),
            0
        );
        assert_eq!(
            property(&identity.opened.key, NCRYPT_KEY_USAGE_PROPERTY).unwrap(),
            NCRYPT_ALLOW_SIGNING_FLAG
        );
        let mut private = Zeroizing::new([0; 104]);
        let mut written = 0;
        assert!(
            unsafe {
                NCryptExportKey(
                    identity.opened.key.0,
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
        let provider = Provider::open(KeyProvider::Software).unwrap();
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
            Identity::validate(provider, key, KeyProvider::Software),
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

    /// A test-named key and its record in a scratch app folder; whatever a
    /// test leaves is deleted from both providers when it ends.
    struct TestSlot(Slot, PathBuf);
    impl TestSlot {
        fn new(label: &str) -> Self {
            let dir = crate::sealed::tests::scratch(&format!("identity-{label}"));
            let nonce: String = crypto::random_nonce().unwrap().iter().map(|b| format!("{b:02x}")).collect();
            let name = HSTRING::from(format!("io.github.meshlive.app.identity-test.{label}.{nonce}"));
            Self(Slot { name, record: Some(dir.join(PROVIDER_RECORD)) }, dir)
        }
    }
    impl Drop for TestSlot {
        fn drop(&mut self) {
            let _ = Identity::delete_from(KeyProvider::Software, self.0.name());
            let _ = Identity::delete_from(KeyProvider::Platform, self.0.name());
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    fn private_export_refused(identity: &Identity) -> bool {
        let mut private = Zeroizing::new([0; 104]);
        let mut written = 0;
        unsafe {
            NCryptExportKey(identity.opened.key.0, None, BCRYPT_ECCPRIVATE_BLOB, None, Some(&mut private[..]), &mut written, NCRYPT_SILENT_FLAG)
        }
        .is_err()
    }

    /// SEC-11: with the switch on, a new identity's key is made in the chip
    /// when this PC has one (the development laptop does), signs, cannot be exported and
    /// is found there again; without a chip the same call gives a software key.
    #[test]
    fn a_new_identity_with_the_switch_on_lives_in_the_security_chip_when_there_is_one() {
        let slot = TestSlot::new("chip");
        let (identity, created) = open_or_create_slot(&mut Cng, &slot.0, true).unwrap();
        assert!(created);
        let signature = identity.sign(b"neutral protocol fixture").unwrap();
        crypto::verify_p256(&identity.public, b"neutral protocol fixture", &signature).unwrap();
        assert!(private_export_refused(&identity));
        let kind = identity.key_provider();
        assert_eq!(slot.0.recorded(), Ok(kind), "the record says where the key is");
        let public = identity.public;
        drop(identity);
        let (again, created) = open_or_create_slot(&mut Cng, &slot.0, true).unwrap();
        // A reopened chip identity signs too (call setup opens it every call).
        crypto::verify_p256(&public, b"fixture", &again.sign(b"fixture").unwrap()).unwrap();
        assert!(!created);
        assert_eq!((again.key_provider(), again.public), (kind, public), "the same key, from the same provider");
        eprintln!("SEC-11 new identity with the switch on: {kind:?}");
        // A chip that can make a P-256 key at all must hold the identity:
        // the software fallback is only for a PC without one.
        let chip_works = Provider::open(KeyProvider::Platform).and_then(|chip| {
            let probe = TestSlot::new("probe");
            Identity::create(&chip, probe.0.name(), KeyProvider::Platform).map(drop)
        }).is_ok();
        if chip_works {
            assert_eq!(kind, KeyProvider::Platform, "this PC's chip makes keys, so the identity is there");
        }
    }

    /// A key store whose chip fails as scripted (one key name per test).
    #[derive(Default)]
    struct Fake {
        keys: std::collections::HashMap<KeyProvider, u32>,
        made: u32,
        chip_create: Option<Error>,
        chip_open: std::collections::VecDeque<Error>,
        chip_delete: std::collections::VecDeque<Error>,
    }
    impl KeyStore for Fake {
        type Key = (KeyProvider, u32);
        fn open(&mut self, kind: KeyProvider, _: &HSTRING) -> Result<Self::Key, Error> {
            if kind == KeyProvider::Platform && let Some(error) = self.chip_open.pop_front() {
                return Err(error);
            }
            self.keys.get(&kind).map(|id| (kind, *id)).ok_or(Error::Native(NTE_BAD_KEYSET_CODE))
        }
        fn create(&mut self, kind: KeyProvider, _: &HSTRING) -> Result<(), Error> {
            if kind == KeyProvider::Platform && let Some(error) = self.chip_create {
                return Err(error);
            }
            if self.keys.contains_key(&kind) {
                return Err(Error::Native(NTE_EXISTS_CODE));
            }
            self.made += 1;
            self.keys.insert(kind, self.made);
            Ok(())
        }
        fn delete(&mut self, kind: KeyProvider, _: &HSTRING) -> Result<bool, Error> {
            if kind == KeyProvider::Platform && let Some(error) = self.chip_delete.pop_front() {
                return Err(error);
            }
            Ok(self.keys.remove(&kind).is_some())
        }
    }
    /// A chip that is there but busy or refusing (NTE_DEVICE_NOT_READY).
    const BUSY: Error = Error::Native(0x8009_0030_u32 as i32);
    /// No chip on this PC (TBS_E_TPM_NOT_FOUND).
    const NO_CHIP: Error = Error::Native(windows::Win32::Foundation::TBS_E_TPM_NOT_FOUND.0);
    fn fake_slot(label: &str) -> (Slot, PathBuf) {
        let dir = crate::sealed::tests::scratch(&format!("identity-fake-{label}"));
        (Slot { name: HSTRING::from("fake"), record: Some(dir.join(PROVIDER_RECORD)) }, dir)
    }

    /// Review finding 1: Delete everything of a chip identity whose chip
    /// fails deletion twice stays incomplete, keeps the record (so the next
    /// launch retries the chip) and completes once the key is really gone.
    #[test]
    fn a_failed_chip_deletion_keeps_the_record_and_the_wipe_resumes_until_the_key_is_gone() {
        let (slot, dir) = fake_slot("wipe");
        let mut fake = Fake::default();
        assert_eq!(create_slot(&mut fake, &slot, true), Ok((KeyProvider::Platform, 1)));
        fake.chip_delete.extend([BUSY, BUSY]);
        for _ in 0..2 {
            let report = crate::wipe::run(&dir, &mut || delete_slot(&mut fake, &slot, false).map_err(|e| format!("{e:?}")));
            assert!(!report.complete && report.key_error.is_some(), "{report:?}");
            assert!(crate::wipe::pending(&dir), "resumed at the next launch");
            assert_eq!(slot.recorded(), Ok(KeyProvider::Platform), "the record stays while the chip key does");
            assert!(fake.keys.contains_key(&KeyProvider::Platform));
            assert_eq!(open_or_create_slot(&mut fake, &slot, false), Ok(((KeyProvider::Platform, 1), false)),
                "never a new identity meanwhile");
        }
        let report = crate::wipe::run(&dir, &mut || delete_slot(&mut fake, &slot, false).map_err(|e| format!("{e:?}")));
        assert!(report.complete && report.key_deleted, "{report:?}");
        assert!(fake.keys.is_empty() && !crate::wipe::pending(&dir));
        assert_eq!(slot.recorded(), Ok(KeyProvider::Software), "the record goes with the key");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Review finding 2: a made chip key that fails its check is deleted
    /// before its record goes; if that deletion fails the record stays and the
    /// error is returned. No software key follows either way.
    #[test]
    fn a_chip_key_that_fails_its_check_is_rolled_back_only_with_a_confirmed_deletion() {
        for delete_fails in [true, false] {
            let (slot, dir) = fake_slot("rollback");
            let mut fake = Fake::default();
            fake.chip_open.push_back(Error::PublicKey);
            if delete_fails {
                fake.chip_delete.push_back(BUSY);
            }
            let error = create_slot(&mut fake, &slot, true).unwrap_err();
            assert_eq!(error, if delete_fails { BUSY } else { Error::PublicKey });
            assert!(!fake.keys.contains_key(&KeyProvider::Software), "no weaker key");
            assert_eq!(fake.keys.contains_key(&KeyProvider::Platform), delete_fails);
            assert_eq!(slot.recorded(), Ok(if delete_fails { KeyProvider::Platform } else { KeyProvider::Software }));
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Review finding 3: a recorded chip identity whose key cannot be opened,
    /// even because it is gone, is an error with either switch value: never a
    /// new identity.
    #[test]
    fn a_recorded_chip_identity_that_cannot_be_opened_is_never_replaced() {
        for failure in [Error::Native(NTE_BAD_KEYSET_CODE), NO_CHIP, BUSY] {
            for chip in [false, true] {
                let (slot, dir) = fake_slot("recorded");
                let mut fake = Fake::default();
                slot.write_record().unwrap();
                if failure != Error::Native(NTE_BAD_KEYSET_CODE) {
                    fake.keys.insert(KeyProvider::Platform, 7);
                    fake.chip_open.push_back(failure);
                }
                assert_eq!(open_or_create_slot(&mut fake, &slot, chip).unwrap_err(), failure);
                assert!(!fake.keys.contains_key(&KeyProvider::Software), "no new identity");
                assert_eq!(slot.recorded(), Ok(KeyProvider::Platform));
                std::fs::remove_dir_all(&dir).unwrap();
            }
        }
    }

    /// Review finding 4: with the switch on, only a PC without a usable chip
    /// gets a software key; a busy or refusing chip is an error; a same-named
    /// chip key whose record was lost is this PC's identity, recorded again.
    #[test]
    fn only_a_missing_chip_lets_a_new_identity_fall_back_to_software() {
        let (slot, dir) = fake_slot("classify");
        let mut fake = Fake { chip_create: Some(NO_CHIP), ..Fake::default() };
        assert_eq!(open_or_create_slot(&mut fake, &slot, true), Ok(((KeyProvider::Software, 1), true)));
        assert_eq!(slot.recorded(), Ok(KeyProvider::Software), "no record for a software key");
        std::fs::remove_dir_all(&dir).unwrap();

        let (slot, dir) = fake_slot("busy");
        let mut fake = Fake { chip_create: Some(BUSY), ..Fake::default() };
        assert_eq!(open_or_create_slot(&mut fake, &slot, true).unwrap_err(), BUSY);
        assert!(fake.keys.is_empty() && slot.recorded() == Ok(KeyProvider::Software));
        std::fs::remove_dir_all(&dir).unwrap();

        let (slot, dir) = fake_slot("lost-record");
        let mut fake = Fake::default();
        fake.keys.insert(KeyProvider::Platform, 9);
        assert_eq!(open_or_create_slot(&mut fake, &slot, true), Ok(((KeyProvider::Platform, 9), false)));
        assert_eq!(slot.recorded(), Ok(KeyProvider::Platform), "the record is written again");
        assert!(!fake.keys.contains_key(&KeyProvider::Software));
        std::fs::remove_dir_all(&dir).unwrap();
        // With the switch off a first run never asks the chip (as before).
        let (slot, dir) = fake_slot("switch-off");
        let mut fake = Fake::default();
        fake.keys.insert(KeyProvider::Platform, 9);
        fake.chip_open.push_back(BUSY);
        assert_eq!(open_or_create_slot(&mut fake, &slot, false), Ok(((KeyProvider::Software, 1), true)));
        assert_eq!(fake.chip_open.len(), 1, "the chip was not asked");
        std::fs::remove_dir_all(&dir).unwrap();
        // A creation that meets a same-named chip key keeps its record and fails.
        let (slot, dir) = fake_slot("exists");
        let mut fake = Fake::default();
        fake.keys.insert(KeyProvider::Platform, 3);
        assert!(matches!(create_in_chip(&mut fake, &slot), Err(ChipFailure::Failed(Error::Native(NTE_EXISTS_CODE)))));
        assert_eq!(slot.recorded(), Ok(KeyProvider::Platform));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Deletion rule: a chip deletion error keeps the wipe incomplete only where
    /// a chip key may exist (a record names the chip, or the switch is on).
    /// A default user (switch off, no record) with a busy chip gets a
    /// complete wipe after one more try.
    #[test]
    fn a_chip_deletion_error_holds_the_wipe_only_where_a_chip_key_may_exist() {
        // Switch off, no record, chip busy every time: the wipe completes.
        let (slot, dir) = fake_slot("default-busy");
        let mut fake = Fake::default();
        fake.keys.insert(KeyProvider::Software, 1);
        fake.chip_delete.extend([BUSY, BUSY]);
        let report = crate::wipe::run(&dir, &mut || delete_slot(&mut fake, &slot, false).map_err(|e| format!("{e:?}")));
        assert!(report.complete && report.key_deleted, "{report:?}");
        assert!(fake.keys.is_empty() && !crate::wipe::pending(&dir));
        assert!(fake.chip_delete.is_empty(), "the chip was asked twice");
        std::fs::remove_dir_all(&dir).unwrap();
        // Switch off, no record: a chip that answers the second time is deleted from.
        let (slot, dir) = fake_slot("default-retry");
        let mut fake = Fake::default();
        fake.keys.insert(KeyProvider::Platform, 4);
        fake.chip_delete.push_back(BUSY);
        assert_eq!(delete_slot(&mut fake, &slot, false), Ok(true));
        assert!(fake.keys.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
        // Switch on, no record: a busy chip is reported; no usable chip is not.
        let (slot, dir) = fake_slot("switch-on");
        let mut fake = Fake::default();
        fake.chip_delete.push_back(BUSY);
        assert_eq!(delete_slot(&mut fake, &slot, true), Err(BUSY));
        fake.chip_delete.push_back(NO_CHIP);
        assert_eq!(delete_slot(&mut fake, &slot, true), Ok(false));
        // A record names the chip: any chip error is reported, with either switch value.
        slot.write_record().unwrap();
        for chip in [false, true] {
            for error in [BUSY, NO_CHIP] {
                fake.chip_delete.push_back(error);
                assert_eq!(delete_slot(&mut fake, &slot, chip), Err(error), "a recorded chip key must be confirmed gone");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An existing software identity keeps its key with the switch on: a move
    /// would change the node ID. Nothing is created in the chip.
    #[test]
    fn an_existing_software_identity_is_never_moved_to_the_chip() {
        let slot = TestSlot::new("keep");
        let (first, created) = open_or_create_slot(&mut Cng, &slot.0, false).unwrap();
        assert!(created && first.key_provider() == KeyProvider::Software);
        let public = first.public;
        drop(first);
        for _ in 0..2 {
            let (identity, created) = open_or_create_slot(&mut Cng, &slot.0, true).unwrap();
            assert!(!created);
            assert_eq!((identity.key_provider(), identity.public), (KeyProvider::Software, public));
        }
        assert_eq!(slot.0.recorded(), Ok(KeyProvider::Software), "no record written");
        assert_ne!(Identity::delete_from(KeyProvider::Platform, slot.0.name()), Ok(true), "no chip key made");
    }

    /// An unreadable record never turns into a silent new identity.
    #[test]
    fn a_damaged_provider_record_is_an_error_and_never_a_new_identity() {
        let slot = TestSlot::new("damaged");
        std::fs::write(slot.0.record.as_ref().unwrap(), b"something else").unwrap();
        assert!(matches!(open_or_create_slot(&mut Cng, &slot.0, true), Err(Error::KeyPolicy)));
        assert!(matches!(open_or_create_slot(&mut Cng, &slot.0, false), Err(Error::KeyPolicy)));
        assert_eq!(Identity::delete_from(KeyProvider::Software, slot.0.name()), Ok(false), "no software key made");
    }

    /// Delete everything removes the identity key wherever it is: from the
    /// chip and from the software provider; afterwards a new identity starts.
    #[test]
    fn deleting_the_identity_removes_a_chip_key_and_a_software_key() {
        for chip in [true, false] {
            let slot = TestSlot::new(if chip { "delete-chip" } else { "delete-soft" });
            let identity = create_slot(&mut Cng, &slot.0, chip).unwrap();
            let public = identity.public;
            drop(identity);
            assert_eq!(delete_slot(&mut Cng, &slot.0, chip), Ok(true));
            assert!(matches!(open_slot(&mut Cng, &slot.0), Err(Error::Native(NTE_BAD_KEYSET_CODE))), "the key is gone");
            assert_eq!(delete_slot(&mut Cng, &slot.0, chip), Ok(false), "nothing left anywhere");
            // The wipe removes the record with the key; then a new identity starts.
            slot.0.remove_record().unwrap();
            let (fresh, created) = open_or_create_slot(&mut Cng, &slot.0, chip).unwrap();
            assert!(created && fresh.public != public, "a new identity, not the old key");
        }
    }

    /// SEC-11 follow-up: the opened key is shared, so a call never opens the
    /// security chip again; every thread signs through the same handle.
    #[test]
    fn the_identity_key_is_opened_once_and_every_thread_signs_through_it() {
        let cache = OpenedCache::new();
        let opens = std::sync::atomic::AtomicUsize::new(0);
        let make = || {
            opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((ephemeral(), true))
        };
        let gate = std::sync::Barrier::new(8);
        let public = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|n| {
                    let (cache, make, gate) = (&cache, &make, &gate);
                    scope.spawn(move || {
                        gate.wait();
                        let (identity, _) = cache.get_or_open(make).unwrap();
                        let message = [n as u8; 64];
                        for _ in 0..5 {
                            let signature = identity.sign(&message).unwrap();
                            let public = identity.public_key().unwrap();
                            crypto::verify_p256(&public, &message, &signature).unwrap();
                        }
                        identity.public_key().unwrap()
                    })
                })
                .collect();
            workers.into_iter().map(|worker| worker.join().unwrap()).collect::<Vec<_>>()
        });
        assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), 1, "eight calls opened the key once");
        assert!(public.iter().all(|key| key == &public[0]), "every thread got the same identity");
        let (again, created) = cache.get_or_open(make).unwrap();
        assert!(!created && opens.load(std::sync::atomic::Ordering::SeqCst) == 1);
        assert_eq!(again.public_key().unwrap(), public[0]);
    }
    #[test]
    fn a_handle_dropped_after_a_call_leaves_the_key_open_and_release_closes_it() {
        let cache = OpenedCache::new();
        let opens = std::cell::Cell::new(0);
        let make = || {
            opens.set(opens.get() + 1);
            Ok((ephemeral(), false))
        };
        let (call, _) = cache.get_or_open(make).unwrap();
        let weak = Arc::downgrade(&call.opened);
        drop(call);
        assert!(weak.upgrade().is_some(), "the call ended, the process still holds the key");
        let (next, _) = cache.get_or_open(make).unwrap();
        assert_eq!(opens.get(), 1, "the next call did not open it");
        cache.release();
        next.sign(b"a call still in progress keeps working").unwrap();
        drop(next);
        assert!(weak.upgrade().is_none(), "released and unused: the key is closed");
        cache.get_or_open(make).unwrap();
        assert_eq!(opens.get(), 2, "after a release the key is opened again");
    }
    #[test]
    fn a_failed_open_is_not_remembered() {
        let cache = OpenedCache::new();
        assert!(cache.get_or_open(|| Err(Error::Native(NTE_BAD_KEYSET_CODE))).is_err());
        let (identity, created) = cache.get_or_open(|| Ok((ephemeral(), true))).unwrap();
        assert!(created);
        identity.sign(b"after the failure").unwrap();
    }

    /// SEC-11 measurement (run once by hand, `--ignored --nocapture`): the
    /// time to open the identity and to make one handshake signature, for
    /// both providers. Call setup opens the identity and signs on every call.
    #[test]
    #[ignore]
    fn signing_latency_for_both_providers() {
        fn summary(mut ms: Vec<f64>) -> String {
            ms.sort_by(|a, b| a.total_cmp(b));
            let median = (ms[ms.len() / 2 - 1] + ms[ms.len() / 2]) / 2.0;
            let p95 = ms[(ms.len() * 95).div_ceil(100) - 1];
            format!("median {median:.2} ms, p95 {p95:.2} ms, max {:.2} ms (n={})", ms[ms.len() - 1], ms.len())
        }
        for chip in [false, true] {
            let slot = TestSlot::new(if chip { "latency-chip" } else { "latency-soft" });
            let started = std::time::Instant::now();
            let identity = create_slot(&mut Cng, &slot.0, chip).unwrap();
            let create = started.elapsed().as_secs_f64() * 1e3;
            let kind = identity.key_provider();
            let message = [0x5a_u8; 200];
            let signs: Vec<f64> = (0..50).map(|_| {
                let at = std::time::Instant::now();
                identity.sign(&message).unwrap();
                at.elapsed().as_secs_f64() * 1e3
            }).collect();
            drop(identity);
            let opens: Vec<f64> = (0..20).map(|_| {
                let at = std::time::Instant::now();
                drop(open_slot(&mut Cng, &slot.0).unwrap());
                at.elapsed().as_secs_f64() * 1e3
            }).collect();
            println!("SEC-11 {kind:?}: create {create:.1} ms; sign {}; open {}", summary(signs), summary(opens));
            // A call's setup: the key open plus one handshake signature, before
            // (open on every call) and now (the open is shared by every call).
            let cache = OpenedCache::new();
            let first = std::time::Instant::now();
            drop(cache.get_or_open(|| open_slot(&mut Cng, &slot.0).map(|identity| (identity, false))).unwrap());
            let first = first.elapsed().as_secs_f64() * 1e3;
            let before: Vec<f64> = (0..20).map(|_| {
                let at = std::time::Instant::now();
                let identity = open_slot(&mut Cng, &slot.0).unwrap();
                identity.sign(&message).unwrap();
                at.elapsed().as_secs_f64() * 1e3
            }).collect();
            let now: Vec<f64> = (0..20).map(|_| {
                let at = std::time::Instant::now();
                let (identity, _) = cache.get_or_open(|| panic!("opened again")).unwrap();
                identity.sign(&message).unwrap();
                at.elapsed().as_secs_f64() * 1e3
            }).collect();
            println!("SEC-11 {kind:?}: call setup (open + one signature) per call {}; open once {first:.1} ms, then per call {}", summary(before), summary(now));
        }
    }
}
