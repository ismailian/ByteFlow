//! Transfer engine (spec §7) and auto-delete after upload (§8).

pub mod engine;
pub mod retry;
pub mod uploader;

pub use engine::{
    EnqueueUpload, SourceDeletion, SourceDeletionOutcome, TransferEngine, TransferEvent, TransferHooks,
    TransferSnapshot, TransferView,
};
pub use bf_data::local::app_db::{TransferKind, TransferStatus};
