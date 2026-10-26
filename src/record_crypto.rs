//! Windows CNG AES-256-GCM adapter; no cryptographic primitive is implemented here.
use crate::records::Error;
use windows::{Win32::Security::Cryptography::*, core::PCWSTR};
use zeroize::Zeroize;

struct Algorithm(BCRYPT_ALG_HANDLE);
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
// Field order releases the native key before its provider. Neither handle is cloned or exported.
pub(crate) struct AesGcm {
    key: Key,
    _algorithm: Algorithm,
}
impl AesGcm {
    pub fn new(secret: &[u8; 32]) -> Result<Self, Error> {
        let mut algorithm = BCRYPT_ALG_HANDLE::default();
        check(
            unsafe {
                BCryptOpenAlgorithmProvider(
                    &mut algorithm,
                    BCRYPT_AES_ALGORITHM,
                    PCWSTR::null(),
                    BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
                )
            }
            .0,
        )?;
        let algorithm = Algorithm(algorithm);
        let mode: Vec<u8> = "ChainingModeGCM\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        check(
            unsafe {
                BCryptSetProperty(BCRYPT_HANDLE(algorithm.0.0), BCRYPT_CHAINING_MODE, &mode, 0)
            }
            .0,
        )?;
        let mut key = BCRYPT_KEY_HANDLE::default();
        // Since Windows 7, a null key-object buffer lets CNG own and free that storage.
        check(unsafe { BCryptGenerateSymmetricKey(algorithm.0, &mut key, None, secret, 0) }.0)?;
        Ok(Self {
            key: Key(key),
            _algorithm: algorithm,
        })
    }
    pub fn encrypt(
        &mut self,
        sequence: u64,
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<[u8; 16], Error> {
        let mut tag = [0; 16];
        self.crypt(false, sequence, aad, input, output, &mut tag)?;
        Ok(tag)
    }
    pub fn decrypt(
        &mut self,
        sequence: u64,
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
        mut tag: [u8; 16],
    ) -> Result<(), Error> {
        self.crypt(true, sequence, aad, input, output, &mut tag)
    }
    fn crypt(
        &mut self,
        decrypt: bool,
        sequence: u64,
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
        tag: &mut [u8; 16],
    ) -> Result<(), Error> {
        if sequence > i64::MAX as u64
            || input.len() != output.len()
            || input.len() > 4 * 1024 * 1024 + 128
            // Record callers supply exactly 17/18 bytes. BMH3 additionally uses
            // its 8-byte header or 105-byte msg2 cleartext as AAD.
            || aad.len() > 105
        {
            return Err(Error::Size);
        }
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&sequence.to_be_bytes());
        let mut info = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
            cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
            dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
            pbNonce: nonce.as_mut_ptr(),
            cbNonce: 12,
            pbAuthData: aad.as_ptr().cast_mut(),
            cbAuthData: aad.len() as u32,
            pbTag: tag.as_mut_ptr(),
            cbTag: 16,
            ..Default::default()
        };
        let mut written = 0;
        // All pointers reference live nonoverlapping buffers for this synchronous call.
        // CNG reads AAD/input, writes output/tag, and may update the local mode-info structure.
        let status = unsafe {
            if decrypt {
                BCryptDecrypt(
                    self.key.0,
                    Some(input),
                    Some((&mut info as *mut BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO).cast()),
                    None,
                    Some(output),
                    &mut written,
                    BCRYPT_FLAGS(0),
                )
            } else {
                BCryptEncrypt(
                    self.key.0,
                    Some(input),
                    Some((&mut info as *mut BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO).cast()),
                    None,
                    Some(output),
                    &mut written,
                    BCRYPT_FLAGS(0),
                )
            }
        };
        if status.0 != 0 || written as usize != input.len() {
            output.zeroize();
            return Err(Error::Crypto(status.0));
        }
        Ok(())
    }
}
fn check(status: i32) -> Result<(), Error> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Crypto(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_authentication_clears_the_entire_caller_output() {
        let mut cipher = AesGcm::new(&[0; 32]).unwrap();
        let aad = [1; 17];
        let plain = b"fixture";
        let mut encrypted = [0; 7];
        let mut tag = cipher.encrypt(0, &aad, plain, &mut encrypted).unwrap();
        tag[0] ^= 1;
        let mut output = [0xa5; 7];
        assert!(
            cipher
                .decrypt(0, &aad, &encrypted, &mut output, tag)
                .is_err()
        );
        assert_eq!(output, [0; 7]);
    }
}
