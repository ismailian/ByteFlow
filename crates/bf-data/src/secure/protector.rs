//! OS-level confidentiality for the secure-store file (spec §17.1).
//!
//! * Windows: DPAPI (`CryptProtectData`, current-user scope, UI forbidden,
//!   app-specific entropy) — the same protection `flutter_secure_storage`
//!   uses on Windows. User-session-bound, not hardware/biometric-gated; every
//!   secret inside is additionally DEK-sealed (§3.1).
//! * macOS / Linux: a random 32-byte key kept in the Keychain / Secret Service
//!   (via `keyring`) seals the file with AES-256-GCM.

use bf_core::crypto::SecretBytes;
use bf_core::{AppFailure, AppResult};

pub trait Protector: Send + Sync {
    fn protect(&self, plain: &[u8]) -> AppResult<Vec<u8>>;
    fn unprotect(&self, blob: &[u8]) -> AppResult<SecretBytes>;
    /// Destroy any OS-held key material (wipe). Idempotent.
    fn destroy(&self) -> AppResult<()> {
        Ok(())
    }
}

pub fn platform() -> Box<dyn Protector> {
    #[cfg(windows)]
    {
        Box::new(dpapi::Dpapi)
    }
    #[cfg(not(windows))]
    {
        Box::new(keychain::Keychain)
    }
}

#[cfg(windows)]
mod dpapi {
    use super::*;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    const ENTROPY: &[u8] = b"ByteFlowDesktop.secure_store.v1";

    pub struct Dpapi;

    fn blob(b: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: b.len() as u32, pbData: b.as_ptr() as *mut u8 }
    }

    impl Protector for Dpapi {
        fn protect(&self, plain: &[u8]) -> AppResult<Vec<u8>> {
            let input = blob(plain);
            let entropy = blob(ENTROPY);
            let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
            // SAFETY: all pointers are valid for the call; `out` is freed below.
            let ok = unsafe {
                CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            };
            if ok == 0 || out.pbData.is_null() {
                return Err(AppFailure::crypto("DPAPI protect failed"));
            }
            // SAFETY: DPAPI returned a buffer of cbData bytes.
            let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
            unsafe { LocalFree(out.pbData as _) };
            Ok(v)
        }

        fn unprotect(&self, data: &[u8]) -> AppResult<SecretBytes> {
            let input = blob(data);
            let entropy = blob(ENTROPY);
            let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
            // SAFETY: as above.
            let ok = unsafe {
                CryptUnprotectData(&input, std::ptr::null_mut(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            };
            if ok == 0 || out.pbData.is_null() {
                return Err(AppFailure::crypto("DPAPI unprotect failed"));
            }
            // SAFETY: DPAPI returned a buffer of cbData bytes; zero it before freeing.
            let v = unsafe {
                let s = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
                let v = s.to_vec();
                s.fill(0);
                LocalFree(out.pbData as _);
                v
            };
            Ok(zeroize::Zeroizing::new(v))
        }
    }
}

#[cfg(not(windows))]
mod keychain {
    use super::*;
    use base64::Engine;
    use bf_core::crypto::envelope::{AAD_SECURE_STORE, Envelope};
    use bf_core::crypto::secure_bytes::random_bytes;

    const SERVICE: &str = "ByteFlowDesktop";
    const ACCOUNT: &str = "secure-store-key";

    pub struct Keychain;

    fn entry() -> AppResult<keyring::Entry> {
        keyring::Entry::new(SERVICE, ACCOUNT).map_err(|_| AppFailure::crypto("keychain unavailable"))
    }

    fn key(create: bool) -> AppResult<SecretBytes> {
        let e = entry()?;
        match e.get_password() {
            Ok(s) => base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .map(zeroize::Zeroizing::new)
                .map_err(|_| AppFailure::crypto("keychain key corrupt")),
            Err(keyring::Error::NoEntry) if create => {
                let k = random_bytes(32);
                e.set_password(&base64::engine::general_purpose::STANDARD.encode(&k))
                    .map_err(|_| AppFailure::crypto("keychain write failed"))?;
                Ok(zeroize::Zeroizing::new(k))
            }
            Err(_) => Err(AppFailure::crypto("keychain read failed")),
        }
    }

    impl Protector for Keychain {
        fn protect(&self, plain: &[u8]) -> AppResult<Vec<u8>> {
            Envelope::seal(plain, &key(true)?, AAD_SECURE_STORE)
        }
        fn unprotect(&self, blob: &[u8]) -> AppResult<SecretBytes> {
            Envelope::open(blob, &key(false)?, AAD_SECURE_STORE)
        }
        fn destroy(&self) -> AppResult<()> {
            match entry()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(_) => Err(AppFailure::crypto("keychain delete failed")),
            }
        }
    }
}

/// Test protector: XOR-free identity (tests only exercise the map logic).
pub struct NullProtector;
impl Protector for NullProtector {
    fn protect(&self, plain: &[u8]) -> AppResult<Vec<u8>> {
        Ok(plain.to_vec())
    }
    fn unprotect(&self, blob: &[u8]) -> AppResult<SecretBytes> {
        Ok(zeroize::Zeroizing::new(blob.to_vec()))
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn dpapi_round_trip() {
        let p = super::platform();
        let blob = p.protect(b"secret").unwrap();
        assert_ne!(blob, b"secret");
        assert_eq!(&**p.unprotect(&blob).unwrap(), b"secret");
        let mut bad = blob.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(p.unprotect(&bad).is_err());
    }
}
