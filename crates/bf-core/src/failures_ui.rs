//! The single place a failure becomes a sentence (spec §14.3). Raw error
//! strings never reach the user.

use crate::failures::{AppFailure, BiometricFailureReason};

pub fn message_for(f: &AppFailure) -> String {
    match f {
        AppFailure::Network { is_timeout: true, .. } => {
            "The connection timed out. Check your network and try again.".into()
        }
        AppFailure::Network { .. } => {
            "Couldn't reach the bucket. Check your network and try again.".into()
        }
        AppFailure::Auth { clock_skew_suspected: true, .. } => {
            "The request signature was rejected. Check your computer's clock or your secret key."
                .into()
        }
        AppFailure::Auth { .. } => "The bucket rejected these credentials.".into(),
        AppFailure::Permission { .. } => {
            "Access denied. The key may not have permission for this.".into()
        }
        AppFailure::NotFound { .. } => "That item no longer exists.".into(),
        AppFailure::Conflict { .. } => "That conflicts with the current state. Try again.".into(),
        AppFailure::StorageFull { .. } => "Not enough space for this item.".into(),
        AppFailure::Crypto { message } if message.contains("legacy format") => message.clone(),
        AppFailure::Crypto { .. } => "This item could not be decrypted or verified.".into(),
        AppFailure::ExternalKey { .. } => "This object needs its passphrase to open.".into(),
        AppFailure::Biometric { reason, .. } => biometric_sentence(*reason).into(),
        AppFailure::Cancelled => "Cancelled.".into(),
        AppFailure::Unknown { .. } => "Unexpected error.".into(),
    }
}

pub fn biometric_sentence(r: BiometricFailureReason) -> &'static str {
    match r {
        BiometricFailureReason::PermanentlyInvalidated => {
            "Biometric unlock is no longer valid on this device. Use your password."
        }
        BiometricFailureReason::UserCanceled => "Biometric check cancelled.",
        BiometricFailureReason::NotAvailable => "Biometric unlock isn't available on this device.",
        BiometricFailureReason::NotEnrolled => "No biometrics are enrolled on this device.",
        BiometricFailureReason::LockedOut => {
            "Too many attempts. Try again later or use your password."
        }
        BiometricFailureReason::LockedOutPermanent => "Biometrics are locked. Use your password.",
        BiometricFailureReason::Unknown => "Biometric unlock failed. Use your password.",
    }
}

/// Action-oriented text for "Test connection" (spec §14.3).
pub fn connection_diagnostic(f: &AppFailure) -> String {
    match f {
        AppFailure::NotFound { .. } => "Credentials accepted, but the bucket was not found at \
            this endpoint. Check the bucket name, or set the jurisdiction endpoint (EU / FedRAMP)."
            .into(),
        AppFailure::Auth { clock_skew_suspected: true, .. } => {
            "Signature mismatch. Check the Secret Access Key and your computer's clock.".into()
        }
        AppFailure::Auth { .. } => {
            "The Access Key ID was not accepted. Check it was copied fully.".into()
        }
        AppFailure::Permission { .. } => {
            "Credentials are valid but lack permission to list this bucket.".into()
        }
        AppFailure::Network { is_timeout: true, .. } => {
            "The endpoint did not respond in time. Check your network.".into()
        }
        AppFailure::Network { .. } => "Couldn't connect to the endpoint. Check the Account ID \
            (or custom endpoint) and your network."
            .into(),
        other => message_for(other),
    }
}
