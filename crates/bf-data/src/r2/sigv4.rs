//! Hand-rolled AWS Signature V4 (spec §4.1). A pure primitive: no I/O, an
//! injected time, verified against the AWS published S3 examples.
//!
//! Path encoding is RFC 3986 over UTF-8 bytes with `/` preserved, and **the
//! same encoder builds both the canonical URI and the wire URL**, so they
//! cannot disagree (historically the #1 bug class).

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;

pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const PRESIGN_MAX_TTL: u64 = 604_800;

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// RFC 3986 percent-encoding of every byte except unreserved characters.
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len() * 3 / 2);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if !encode_slash => out.push('/'),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// Canonical query: each key and value encoded, sorted by encoded key then value.
pub fn canonical_query(params: &[(String, String)]) -> String {
    let mut enc: Vec<(String, String)> =
        params.iter().map(|(k, v)| (uri_encode(k, true), uri_encode(v, true))).collect();
    enc.sort();
    enc.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

pub struct Signer {
    access_key_id: String,
    secret: Zeroizing<Vec<u8>>,
    region: String,
    service: String,
}

impl Signer {
    pub fn new(access_key_id: &str, secret_access_key: &str, region: &str, service: &str) -> Self {
        Self {
            access_key_id: access_key_id.to_string(),
            secret: Zeroizing::new(format!("AWS4{secret_access_key}").into_bytes()),
            region: region.to_string(),
            service: service.to_string(),
        }
    }

    /// R2: region is the literal `auto`, service `s3`.
    pub fn r2(access_key_id: &str, secret_access_key: &str) -> Self {
        Self::new(access_key_id, secret_access_key, "auto", "s3")
    }

    fn signing_key(&self, date: &str) -> Zeroizing<Vec<u8>> {
        let k = hmac(&self.secret, date.as_bytes());
        let k = hmac(&k, self.region.as_bytes());
        let k = hmac(&k, self.service.as_bytes());
        hmac(&k, b"aws4_request")
    }

    fn scope(&self, date: &str) -> String {
        format!("{date}/{}/{}/aws4_request", self.region, self.service)
    }

    /// Sign a request. `headers` are the extra headers that will be sent
    /// (e.g. `range`, `content-type`); `host`, `x-amz-date` and
    /// `x-amz-content-sha256` are added. Returns every header to send,
    /// including `authorization`.
    pub fn sign(
        &self,
        method: &str,
        host: &str,
        encoded_path: &str,
        query: &[(String, String)],
        headers: &[(String, String)],
        payload_hash: &str,
        now: DateTime<Utc>,
    ) -> Vec<(String, String)> {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let mut all: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        all.push(("host".into(), host.to_string()));
        all.push(("x-amz-content-sha256".into(), payload_hash.to_string()));
        all.push(("x-amz-date".into(), amz_date.clone()));
        all.sort();
        let canonical_headers: String = all.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let signed_headers = all.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
        let canonical = format!(
            "{method}\n{encoded_path}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            canonical_query(query)
        );
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{}\n{}",
            self.scope(&date),
            sha256_hex(canonical.as_bytes())
        );
        let sig = hex::encode(&*hmac(&self.signing_key(&date), sts.as_bytes()));
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{},SignedHeaders={signed_headers},Signature={sig}",
            self.access_key_id,
            self.scope(&date)
        );
        all.retain(|(k, _)| k != "host");
        all.push(("authorization".into(), auth));
        all
    }

    /// Query-string presign (only `host` signed). Returns the full query
    /// string (without `?`). TTL is clamped to `[1, 604800]`.
    pub fn presign(
        &self,
        method: &str,
        host: &str,
        encoded_path: &str,
        extra_query: &[(String, String)],
        ttl_secs: u64,
        now: DateTime<Utc>,
    ) -> String {
        let ttl = ttl_secs.clamp(1, PRESIGN_MAX_TTL);
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let mut q: Vec<(String, String)> = extra_query.to_vec();
        q.push(("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()));
        q.push(("X-Amz-Credential".into(), format!("{}/{}", self.access_key_id, self.scope(&date))));
        q.push(("X-Amz-Date".into(), amz_date.clone()));
        q.push(("X-Amz-Expires".into(), ttl.to_string()));
        q.push(("X-Amz-SignedHeaders".into(), "host".into()));
        let cq = canonical_query(&q);
        let canonical = format!("{method}\n{encoded_path}\n{cq}\nhost:{host}\n\nhost\n{UNSIGNED_PAYLOAD}");
        let sts = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{}\n{}",
            self.scope(&date),
            sha256_hex(canonical.as_bytes())
        );
        let sig = hex::encode(&*hmac(&self.signing_key(&date), sts.as_bytes()));
        format!("{cq}&X-Amz-Signature={sig}")
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(data);
    Zeroizing::new(m.finalize().into_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn t() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap()
    }

    /// AWS S3 SigV4 documentation example: "GET Object" with a Range header.
    #[test]
    fn aws_get_object_vector() {
        let s = Signer::new(AK, SK, "us-east-1", "s3");
        let h = s.sign(
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &[],
            &[("Range".into(), "bytes=0-9".into())],
            EMPTY_SHA256,
            t(),
        );
        let auth = &h.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert!(auth.ends_with(
            "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        ), "{auth}");
    }

    /// AWS S3 SigV4 documentation example: presigned GET, 86400 s.
    #[test]
    fn aws_presign_vector() {
        let s = Signer::new(AK, SK, "us-east-1", "s3");
        let q = s.presign("GET", "examplebucket.s3.amazonaws.com", "/test.txt", &[], 86_400, t());
        assert!(q.ends_with(
            "X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        ), "{q}");
    }

    /// AWS S3 example: "GET Bucket (List Objects)" with a query string.
    #[test]
    fn aws_list_vector() {
        let s = Signer::new(AK, SK, "us-east-1", "s3");
        let h = s.sign(
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/",
            &[("max-keys".into(), "2".into()), ("prefix".into(), "J".into())],
            &[],
            EMPTY_SHA256,
            t(),
        );
        let auth = &h.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert!(auth.ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ), "{auth}");
    }

    #[test]
    fn key_encoding_round_trips() {
        assert_eq!(uri_encode("a b+c/é#&%.txt", false), "a%20b%2Bc/%C3%A9%23%26%25.txt");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("~-_.", false), "~-_.");
        let q = canonical_query(&[("b".into(), "2".into()), ("a".into(), "x y".into()), ("a".into(), "1".into())]);
        assert_eq!(q, "a=1&a=x%20y&b=2");
    }

    #[test]
    fn presign_ttl_clamped() {
        let s = Signer::r2(AK, SK);
        let q = s.presign("GET", "h", "/b/k", &[], 10_000_000, t());
        assert!(q.contains("X-Amz-Expires=604800"));
        let q = s.presign("GET", "h", "/b/k", &[], 0, t());
        assert!(q.contains("X-Amz-Expires=1&"));
    }
}
