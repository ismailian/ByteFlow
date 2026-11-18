//! Hardware KEK (`KEK_bio`) contract (spec §3.3), kept identical to the
//! mobile `r2vault/keystore` channel so `CredentialStore` and the auth
//! controller stay untouched when a backend is added.
//!
//! **[DESKTOP decision, §17.2]** No desktop backend currently meets the
//! required properties (OS-enforced biometric gate on a hardware key that
//! never leaves secure hardware, detectable re-enrollment). A DPAPI- or
//! session-only key explicitly does *not* qualify. The desktop build therefore
//! ships **password-only** (`dek_wrap_pwd`), which `isConfigured` already
//! accepts; the UI never offers biometrics while `is_available()` is false.
//! A Windows Hello / macOS Secure Enclave backend can implement this trait
//! later without touching anything else.

use bf_core::crypto::SecretBytes;
use bf_core::{AppFailure, AppResult, BiometricFailureReason};

pub trait KeyManager: Send + Sync {
    /// Encrypt the DEK with the hardware key (creating it if absent). May prompt.
    fn wrap(&self, data: &[u8]) -> AppResult<Vec<u8>>;
    /// Decrypt a wrap blob. Always prompts.
    fn unwrap(&self, blob: &[u8]) -> AppResult<SecretBytes>;
    /// Delete the alias. Idempotent; a missing alias is not an error.
    fn delete_key(&self) -> AppResult<()>;
    /// Strong biometrics usable right now.
    fn is_available(&self) -> bool;
}

pub struct NoHardwareKeyManager;

fn not_available() -> AppFailure {
    AppFailure::Biometric {
        reason: BiometricFailureReason::NotAvailable,
        message: "no hardware key backend on this platform".into(),
    }
}

impl KeyManager for NoHardwareKeyManager {
    fn wrap(&self, _: &[u8]) -> AppResult<Vec<u8>> {
        Err(not_available())
    }
    fn unwrap(&self, _: &[u8]) -> AppResult<SecretBytes> {
        Err(not_available())
    }
    fn delete_key(&self) -> AppResult<()> {
        Ok(())
    }
    fn is_available(&self) -> bool {
        false
    }
}
