//! File-content encryption (spec §9): engine, persistent state, session keys.

pub mod engine;
pub mod keys;
pub mod state;

pub use engine::{BulkSplit, EncPhase, EncTaskView, EncryptionEngine, UploadTarget};
pub use state::{EXTERNAL_EPOCH, EncOp, EncStatus};
