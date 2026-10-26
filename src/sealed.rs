//! At-rest sealing for the app's private stores (Android: one AndroidKeyStore
//! AES-256-GCM key seals verified contacts, claims and presence leases).
//! Windows: DPAPI in the current user's scope with the UI forbidden and a
//! per-store entropy label, so a blob for one store never opens as another.
//! Files are written atomically (temporary file, then replace) and are never
//! overwritten implicitly when they cannot be opened.
use std::{
    io,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    },
    core::PCWSTR,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The bytes do not open (wrong user, damaged, or another store's blob).
    Unseal(i32),
    Seal(i32),
    Io(io::ErrorKind),
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e.kind())
    }
}

/// Seals and opens one store's bytes.
pub trait Sealer: Send {
    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, Error>;
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, Error>;
}

/// DPAPI, current user, no UI, with `label` as the entropy.
pub struct Dpapi {
    label: Vec<u8>,
}
impl Dpapi {
    pub fn new(label: &str) -> Self {
        Self {
            label: label.as_bytes().to_vec(),
        }
    }
}
fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    }
}
fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() {
        return Vec::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
    }
    bytes
}
impl Sealer for Dpapi {
    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, Error> {
        let (input, entropy) = (blob(plain), blob(&self.label));
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
        .map_err(|e| Error::Seal(e.code().0))?;
        Ok(take(out))
    }
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, Error> {
        let (input, entropy) = (blob(sealed), blob(&self.label));
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
        .map_err(|e| Error::Unseal(e.code().0))?;
        Ok(take(out))
    }
}

/// The app's private data folder, or the separate executable-local review folder.
/// The owner creates it on first save; nothing here creates it.
pub fn default_dir() -> Option<PathBuf> {
    crate::app_mode::data_dir()
}

/// Reads and opens a sealed file; Ok(None) when it does not exist.
pub fn read(path: &Path, sealer: &dyn Sealer) -> Result<Option<Vec<u8>>, Error> {
    match std::fs::read(path) {
        Ok(bytes) => sealer.open(&bytes).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Seals and writes atomically: a sibling temporary file, flushed, then
/// renamed over the old file (a crash leaves the old or the new file whole).
pub fn write(path: &Path, sealer: &dyn Sealer, plain: &[u8]) -> Result<(), Error> {
    let sealed = sealer.seal(plain)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&sealed)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        Error::from(e)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A reversible test sealer (not secret): tags and flips the bytes.
    pub(crate) struct Plain;
    impl Sealer for Plain {
        fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, Error> {
            let mut out = b"TEST".to_vec();
            out.extend(plain.iter().map(|b| b ^ 0x5a));
            Ok(out)
        }
        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, Error> {
            let body = sealed.strip_prefix(b"TEST").ok_or(Error::Unseal(-1))?;
            Ok(body.iter().map(|b| b ^ 0x5a).collect())
        }
    }

    /// A fresh folder under the checkout's .artifacts (never the user's data).
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let nonce = crate::crypto::random_nonce().unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".artifacts/test-stores")
            .join(format!("{name}-{:02x}{:02x}{:02x}{:02x}", nonce[0], nonce[1], nonce[2], nonce[3]));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dpapi_round_trips_only_with_the_same_label() {
        let contacts = Dpapi::new("io.github.meshlive.app/test/contacts/1");
        let other = Dpapi::new("io.github.meshlive.app/test/claims/1");
        let sealed = contacts.seal(b"verified contacts").unwrap();
        assert!(!sealed.windows(8).any(|w| w == b"verified"));
        assert_eq!(contacts.open(&sealed).unwrap(), b"verified contacts");
        assert!(matches!(other.open(&sealed), Err(Error::Unseal(_))));
        let mut damaged = sealed.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert!(matches!(contacts.open(&damaged), Err(Error::Unseal(_))));
    }

    #[test]
    fn files_are_replaced_whole_and_a_missing_file_is_none() {
        let dir = scratch("sealed");
        let path = dir.join("store.bin");
        assert_eq!(read(&path, &Plain).unwrap(), None);
        write(&path, &Plain, b"one").unwrap();
        write(&path, &Plain, b"two").unwrap();
        assert_eq!(read(&path, &Plain).unwrap().as_deref(), Some(&b"two"[..]));
        assert!(!dir.join("store.bin.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
