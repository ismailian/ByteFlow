//! The R2 configuration — the plaintext of the credential blob (§3.1).

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// `{ accountId, accessKeyId, secretAccessKey, bucket, publicUrl?, endpoint? }`
/// — byte-compatible JSON with the mobile blob. Wiped from memory on drop;
/// `Debug` redacts the secret.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize, ZeroizeOnDrop, Default)]
#[serde(rename_all = "camelCase")]
pub struct R2Config {
    pub account_id: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub bucket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl R2Config {
    pub fn effective_endpoint(&self) -> String {
        match self.endpoint.as_deref().map(str::trim) {
            Some(e) if !e.is_empty() => e.trim_end_matches('/').to_string(),
            _ => format!("https://{}.r2.cloudflarestorage.com", self.account_id.trim()),
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.account_id.trim().is_empty() && self.endpoint.as_deref().unwrap_or("").is_empty() {
            return Err("Account ID is required");
        }
        if self.access_key_id.trim().is_empty() {
            return Err("Access Key ID is required");
        }
        if self.secret_access_key.is_empty() {
            return Err("Secret Access Key is required");
        }
        if self.bucket.trim().is_empty() {
            return Err("Bucket is required");
        }
        if let Some(e) = self.endpoint.as_deref().filter(|e| !e.trim().is_empty())
            && !e.trim().starts_with("https://")
        {
            return Err("Custom endpoint must start with https://");
        }
        Ok(())
    }

    /// Trimmed copy (pasted values often carry whitespace).
    pub fn normalized(&self) -> Self {
        let opt = |o: &Option<String>| {
            o.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        };
        Self {
            account_id: self.account_id.trim().into(),
            access_key_id: self.access_key_id.trim().into(),
            secret_access_key: self.secret_access_key.trim().into(),
            bucket: self.bucket.trim().into(),
            public_url: opt(&self.public_url),
            endpoint: opt(&self.endpoint),
        }
    }
}

impl std::fmt::Debug for R2Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("R2Config")
            .field("account_id", &bf_core::logging::redact(&self.account_id))
            .field("access_key_id", &bf_core::logging::redact(&self.access_key_id))
            .field("secret_access_key", &"<redacted>")
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_shape_and_redaction() {
        let c = R2Config {
            account_id: "acc".into(),
            access_key_id: "AKID1234".into(),
            secret_access_key: "supersecret".into(),
            bucket: "b".into(),
            public_url: None,
            endpoint: None,
        };
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, r#"{"accountId":"acc","accessKeyId":"AKID1234","secretAccessKey":"supersecret","bucket":"b"}"#);
        assert!(!format!("{c:?}").contains("supersecret"));
        assert_eq!(c.effective_endpoint(), "https://acc.r2.cloudflarestorage.com");
    }
}
