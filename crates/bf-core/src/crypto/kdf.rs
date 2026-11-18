//! Argon2id (spec §3.1, §19.3): 64 MiB, t=3, p=4, 32-byte output, 16-byte
//! salt. The four lanes are filled in parallel (rayon), the equivalent of the
//! mobile worker isolates. This is CPU-bound: callers run it off the UI
//! thread (`spawn_blocking`).

use argon2::{Algorithm, Argon2, Params, Version};

use super::secure_bytes::{SecretBytes, constant_time_eq, secret};
use crate::failures::{AppFailure, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Params {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
    pub hash_length: usize,
}

impl Default for Argon2Params {
    fn default() -> Self {
        Self { memory_kib: 65_536, iterations: 3, parallelism: 4, hash_length: 32 }
    }
}

impl Argon2Params {
    /// Cheap parameters for tests only.
    pub const fn test_weak() -> Self {
        Self { memory_kib: 256, iterations: 1, parallelism: 1, hash_length: 32 }
    }

    /// `mem:it:par` — the stored form of `enc_master_kdf_params`.
    pub fn encode(&self) -> String {
        format!("{}:{}:{}", self.memory_kib, self.iterations, self.parallelism)
    }

    pub fn decode(s: &str) -> Option<Self> {
        let mut it = s.split(':');
        let m = it.next()?.parse().ok()?;
        let t = it.next()?.parse().ok()?;
        let p = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some(Self { memory_kib: m, iterations: t, parallelism: p, hash_length: 32 })
    }
}

pub struct PasswordHasher;

impl PasswordHasher {
    pub fn derive_key(password: &[u8], salt: &[u8], params: Argon2Params) -> AppResult<SecretBytes> {
        let p = Params::new(
            params.memory_kib,
            params.iterations,
            params.parallelism,
            Some(params.hash_length),
        )
        .map_err(|_| AppFailure::crypto("invalid KDF parameters"))?;
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, p);
        let mut out = secret(vec![0u8; params.hash_length]);
        argon
            .hash_password_into(password, salt, &mut out)
            .map_err(|_| AppFailure::crypto("key derivation failed"))?;
        Ok(out)
    }

    pub fn verify(
        password: &[u8],
        salt: &[u8],
        expected: &[u8],
        params: Argon2Params,
    ) -> AppResult<bool> {
        let candidate = Self::derive_key(password, salt, params)?;
        Ok(constant_time_eq(&candidate, expected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let p = Argon2Params::test_weak();
        let a = PasswordHasher::derive_key(b"pw", b"0123456789abcdef", p).unwrap();
        let b = PasswordHasher::derive_key(b"pw", b"0123456789abcdef", p).unwrap();
        let c = PasswordHasher::derive_key(b"pw", b"0123456789abcdeX", p).unwrap();
        assert_eq!(*a, *b);
        assert_ne!(*a, *c);
        assert!(PasswordHasher::verify(b"pw", b"0123456789abcdef", &a, p).unwrap());
        assert!(!PasswordHasher::verify(b"px", b"0123456789abcdef", &a, p).unwrap());
    }

    #[test]
    fn params_codec() {
        assert_eq!(Argon2Params::decode("65536:3:4"), Some(Argon2Params::default()));
        assert_eq!(Argon2Params::default().encode(), "65536:3:4");
        assert_eq!(Argon2Params::decode("1:2"), None);
    }
}
