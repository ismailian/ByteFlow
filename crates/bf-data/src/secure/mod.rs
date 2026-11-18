//! Secure storage (spec §3, §19.2).

pub mod credential_store;
pub mod key_manager;
pub mod protector;
pub mod store;
pub mod vault_state_store;

pub use credential_store::CredentialStore;
pub use key_manager::{KeyManager, NoHardwareKeyManager};
pub use store::{FileSecureStore, MemorySecureStore, SecureStore};
pub use vault_state_store::VaultStateStore;
