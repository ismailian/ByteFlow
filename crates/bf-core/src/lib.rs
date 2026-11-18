//! ByteFlow core: crypto primitives, the failure taxonomy, logging and
//! redaction, and formatting. No network I/O, no platform UI.
//!
//! Every fallible operation returns [`AppResult`]; nothing panics across a
//! layer boundary.

pub mod crypto;
pub mod failures;
pub mod failures_ui;
pub mod format;
pub mod logging;
pub mod media;

pub use failures::{AppFailure, AppResult, BiometricFailureReason};
