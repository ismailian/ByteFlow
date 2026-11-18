//! Crypto primitives. All formats here are byte-identical with the mobile
//! app and the Dart CLI (spec §2.3): a blob sealed on one platform must open
//! on every other.

pub mod envelope;
pub mod file_crypto;
pub mod kdf;
pub mod secure_bytes;

pub use envelope::Envelope;
pub use kdf::{Argon2Params, PasswordHasher};
pub use secure_bytes::{SecretBytes, constant_time_eq, random_bytes};
