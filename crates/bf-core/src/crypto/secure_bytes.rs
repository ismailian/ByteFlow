//! Secret byte handling. Secrets are bytes, never `String`, and are zeroed on
//! drop (`Zeroizing`), the Rust equivalent of the mobile `fillRange(0)` in
//! `finally` blocks.

use zeroize::Zeroizing;

/// A heap buffer that is wiped when dropped.
pub type SecretBytes = Zeroizing<Vec<u8>>;

pub fn secret(v: Vec<u8>) -> SecretBytes {
    Zeroizing::new(v)
}

/// CSPRNG bytes from the OS.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    // The OS CSPRNG failing is unrecoverable and never expected; refusing to
    // continue is the only safe option (a zero key would be catastrophic).
    getrandom::fill(&mut out).expect("OS random number generator unavailable");
    out
}

pub fn random_secret(n: usize) -> SecretBytes {
    Zeroizing::new(random_bytes(n))
}

/// Constant-time equality. Only a length difference can leak.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ct_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert_ne!(random_bytes(32), random_bytes(32));
    }
}
