//! Failure taxonomy (spec §14.1). `message` is developer detail and never a
//! secret. Human text lives only in [`crate::failures_ui`].

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiometricFailureReason {
    PermanentlyInvalidated,
    UserCanceled,
    NotAvailable,
    NotEnrolled,
    LockedOut,
    LockedOutPermanent,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppFailure {
    Network { is_timeout: bool, message: String },
    Auth { clock_skew_suspected: bool, message: String },
    Permission { message: String },
    NotFound { message: String },
    Conflict { message: String },
    StorageFull { message: String },
    Crypto { message: String },
    /// "This object needs a passphrase" — the single signal for §9.9.
    ExternalKey { message: String },
    Biometric { reason: BiometricFailureReason, message: String },
    /// Cooperative cancellation (pause/cancel/lock). Never shown as an error.
    Cancelled,
    Unknown { message: String },
}

pub type AppResult<T> = Result<T, AppFailure>;

impl AppFailure {
    pub fn network(message: impl Into<String>) -> Self {
        Self::Network { is_timeout: false, message: message.into() }
    }
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::Network { is_timeout: true, message: message.into() }
    }
    pub fn crypto(message: impl Into<String>) -> Self {
        Self::Crypto { message: message.into() }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound { message: message.into() }
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict { message: message.into() }
    }
    pub fn permission(message: impl Into<String>) -> Self {
        Self::Permission { message: message.into() }
    }
    pub fn unknown(message: impl Into<String>) -> Self {
        Self::Unknown { message: message.into() }
    }
    pub fn external_key() -> Self {
        Self::ExternalKey { message: "object needs a passphrase".into() }
    }
    /// Local I/O error → failure. The OS message can contain a path, which is
    /// developer detail (never a secret).
    pub fn io(context: &str, e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => Self::not_found(format!("{context}: {e}")),
            std::io::ErrorKind::PermissionDenied => Self::permission(format!("{context}: {e}")),
            std::io::ErrorKind::StorageFull => {
                Self::StorageFull { message: format!("{context}: {e}") }
            }
            _ => Self::unknown(format!("{context}: {e}")),
        }
    }

    /// Only network failures are retried (spec §7.2).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Network { .. })
    }
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    pub fn developer_message(&self) -> &str {
        match self {
            Self::Network { message, .. }
            | Self::Auth { message, .. }
            | Self::Permission { message }
            | Self::NotFound { message }
            | Self::Conflict { message }
            | Self::StorageFull { message }
            | Self::Crypto { message }
            | Self::ExternalKey { message }
            | Self::Biometric { message, .. }
            | Self::Unknown { message } => message,
            Self::Cancelled => "cancelled",
        }
    }

    fn kind_name(&self) -> &'static str {
        match self {
            Self::Network { .. } => "NetworkFailure",
            Self::Auth { .. } => "AuthFailure",
            Self::Permission { .. } => "PermissionFailure",
            Self::NotFound { .. } => "NotFoundFailure",
            Self::Conflict { .. } => "ConflictFailure",
            Self::StorageFull { .. } => "StorageFullFailure",
            Self::Crypto { .. } => "CryptoFailure",
            Self::ExternalKey { .. } => "ExternalKeyFailure",
            Self::Biometric { .. } => "BiometricFailure",
            Self::Cancelled => "Cancelled",
            Self::Unknown { .. } => "UnknownFailure",
        }
    }
}

impl fmt::Display for AppFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind_name(), self.developer_message())
    }
}

impl std::error::Error for AppFailure {}
