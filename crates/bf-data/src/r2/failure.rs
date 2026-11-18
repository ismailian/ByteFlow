//! S3 / transport → [`AppFailure`] mapping (spec §14.2).

use bf_core::AppFailure;

pub fn from_s3(status: u16, code: Option<&str>, message: &str) -> AppFailure {
    let detail = format!("HTTP {status} {}: {message}", code.unwrap_or("-"));
    match code {
        Some("SignatureDoesNotMatch") => AppFailure::Auth { clock_skew_suspected: true, message: detail },
        Some("RequestTimeTooSkewed") => AppFailure::Auth { clock_skew_suspected: true, message: detail },
        Some("InvalidAccessKeyId" | "InvalidSecurity" | "AuthorizationHeaderMalformed") => {
            AppFailure::Auth { clock_skew_suspected: false, message: detail }
        }
        Some("AccessDenied") => AppFailure::Permission { message: detail },
        Some("NoSuchKey" | "NoSuchUpload" | "NoSuchBucket") => AppFailure::NotFound { message: detail },
        Some("EntityTooLarge" | "MaxMessageLengthExceeded") => AppFailure::StorageFull { message: detail },
        Some(
            "PreconditionFailed" | "InvalidPart" | "InvalidPartOrder" | "BucketAlreadyExists"
            | "BucketAlreadyOwnedByYou",
        ) => AppFailure::Conflict { message: detail },
        Some("SlowDown" | "ServiceUnavailable" | "RequestTimeout" | "InternalError") => {
            AppFailure::Network { is_timeout: false, message: detail }
        }
        _ => from_status(status, detail),
    }
}

fn from_status(status: u16, detail: String) -> AppFailure {
    match status {
        401 => AppFailure::Auth { clock_skew_suspected: false, message: detail },
        403 => AppFailure::Permission { message: detail },
        404 => AppFailure::NotFound { message: detail },
        409 | 412 => AppFailure::Conflict { message: detail },
        413 => AppFailure::StorageFull { message: detail },
        408 | 429 | 500..=599 => AppFailure::Network { is_timeout: status == 408, message: detail },
        _ => AppFailure::Unknown { message: detail },
    }
}

/// Transport errors. reqwest error text can embed the URL; for presigned
/// requests that would include a signature, so the URL is stripped.
pub fn from_transport(e: &reqwest::Error) -> AppFailure {
    let msg = e.without_url_ref();
    // Connection errors, bad certs, TLS/socket errors (e.g. a wrong Account
    // ID) are all NetworkFailure; timeouts are flagged.
    if e.is_timeout() { AppFailure::timeout(msg) } else { AppFailure::network(msg) }
}

trait WithoutUrl {
    fn without_url_ref(&self) -> String;
}
impl WithoutUrl for reqwest::Error {
    fn without_url_ref(&self) -> String {
        let mut s = String::from("transport error");
        let mut src: Option<&dyn std::error::Error> = std::error::Error::source(self);
        while let Some(e) = src {
            s.push_str(": ");
            s.push_str(&bf_core::logging::redact_url(&e.to_string()));
            src = e.source();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapping() {
        assert!(matches!(from_s3(403, Some("SignatureDoesNotMatch"), ""), AppFailure::Auth { clock_skew_suspected: true, .. }));
        assert!(matches!(from_s3(404, None, ""), AppFailure::NotFound { .. }));
        assert!(from_s3(503, Some("SlowDown"), "").is_retryable());
        assert!(from_s3(429, None, "").is_retryable());
        assert!(from_s3(500, None, "").is_retryable());
        assert!(!from_s3(400, Some("InvalidArgument"), "").is_retryable());
        assert!(matches!(from_s3(400, Some("EntityTooLarge"), ""), AppFailure::StorageFull { .. }));
        assert!(matches!(from_s3(409, None, ""), AppFailure::Conflict { .. }));
    }
}
