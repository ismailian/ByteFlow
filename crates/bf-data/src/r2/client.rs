//! The R2 client (spec §4.2): path-style addressing, SigV4, streamed bodies,
//! byte-level send progress, and cooperative cancellation.
//!
//! The only host this ever talks to is the configured endpoint (§1.2).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use reqwest::{Method, Response};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::config::R2Config;
use super::failure;
use super::sigv4::{self, EMPTY_SHA256, Signer, UNSIGNED_PAYLOAD, uri_encode};
use super::xml;
use bf_core::{AppFailure, AppResult};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub last_modified_ms: Option<i64>,
}

impl ObjectInfo {
    /// Multipart ETags (`<md5>-<n>`) are never treated as an MD5 (§11).
    pub fn is_multipart_etag(&self) -> bool {
        self.etag.contains('-')
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListPage {
    pub objects: Vec<ObjectInfo>,
    pub common_prefixes: Vec<String>,
    pub next_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HeadInfo {
    pub size: u64,
    pub etag: String,
    pub content_type: Option<String>,
    pub last_modified_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletedPart {
    #[serde(rename = "n")]
    pub part_number: u32,
    #[serde(rename = "e")]
    pub etag: String,
}

#[derive(Debug, Clone)]
pub struct DeleteError {
    pub key: String,
    pub code: String,
    pub message: String,
}

/// Called with each slice's byte count as it is handed to the socket.
pub type BodyProgress = Arc<dyn Fn(u64) + Send + Sync>;

/// Upload bodies are fed as 64 KiB zero-copy views of the buffer (§15), which
/// is also what makes send progress track the real socket rate.
const UPLOAD_SLICE: usize = 64 * 1024;
pub const LIST_PAGE: u32 = 1000;
pub const DELETE_BATCH: usize = 1000;

static TLS_INIT: std::sync::Once = std::sync::Once::new();

pub struct R2Client {
    http: reqwest::Client,
    signer: Signer,
    base: String,
    host: String,
    bucket: String,
    clock: Arc<dyn Clock>,
}

impl R2Client {
    pub fn new(cfg: &R2Config) -> AppResult<Self> {
        Self::with_clock(cfg, Arc::new(SystemClock))
    }

    pub fn with_clock(cfg: &R2Config, clock: Arc<dyn Clock>) -> AppResult<Self> {
        TLS_INIT.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        let endpoint = cfg.effective_endpoint();
        let url = reqwest::Url::parse(&endpoint).map_err(|_| AppFailure::unknown("invalid endpoint"))?;
        if url.scheme() != "https" {
            return Err(AppFailure::unknown("endpoint must use https"));
        }
        let host = match (url.host_str(), url.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => return Err(AppFailure::unknown("endpoint has no host")),
        };
        let http = reqwest::Client::builder()
            .tls_backend_rustls()
            .https_only(true)
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(16)
            .tcp_nodelay(true)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("ByteFlowDesktop")
            .build()
            .map_err(|e| failure::from_transport(&e))?;
        Ok(Self {
            http,
            signer: Signer::r2(cfg.access_key_id.trim(), cfg.secret_access_key.trim()),
            base: format!("https://{host}"),
            host,
            bucket: cfg.bucket.trim().to_string(),
            clock,
        })
    }

    /// `/<bucket>/<key>`, encoded once — used for both signing and the wire.
    fn path_for(&self, key: Option<&str>) -> String {
        match key {
            Some(k) => format!("/{}/{}", uri_encode(&self.bucket, true), uri_encode(k, false)),
            None => format!("/{}", uri_encode(&self.bucket, true)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        method: Method,
        key: Option<&str>,
        query: Vec<(String, String)>,
        headers: Vec<(String, String)>,
        body: Option<(reqwest::Body, u64)>,
        payload_hash: &str,
        cancel: Option<&CancellationToken>,
    ) -> AppResult<Response> {
        let path = self.path_for(key);
        let cq = sigv4::canonical_query(&query);
        let url = if cq.is_empty() { format!("{}{path}", self.base) } else { format!("{}{path}?{cq}", self.base) };
        let signed = self.signer.sign(method.as_str(), &self.host, &path, &query, &headers, payload_hash, self.clock.now());
        let mut req = self.http.request(method.clone(), &url);
        for (k, v) in &signed {
            req = req.header(k.as_str(), v.as_str());
        }
        match body {
            Some((b, len)) => req = req.header("content-length", len.to_string()).body(b),
            None if method == Method::PUT || method == Method::POST => {
                req = req.header("content-length", "0")
            }
            None => {}
        }
        let fut = async {
            let resp = req.send().await.map_err(|e| failure::from_transport(&e))?;
            if resp.status().is_success() || resp.status().as_u16() == 206 {
                return Ok(resp);
            }
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            Err(match xml::parse_error(&text) {
                Some((code, msg)) => failure::from_s3(status, Some(&code), &msg),
                None => failure::from_s3(status, None, ""),
            })
        };
        with_cancel(cancel, fut).await
    }

    // ------------------------------------------------------------- list --

    pub async fn list_page(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
        max_keys: u32,
    ) -> AppResult<ListPage> {
        let mut q = vec![
            ("list-type".to_string(), "2".to_string()),
            ("max-keys".to_string(), max_keys.to_string()),
        ];
        if !prefix.is_empty() {
            q.push(("prefix".into(), prefix.into()));
        }
        if let Some(d) = delimiter {
            q.push(("delimiter".into(), d.into()));
        }
        if let Some(t) = token {
            q.push(("continuation-token".into(), t.into()));
        }
        let resp = self.execute(Method::GET, None, q, vec![], None, EMPTY_SHA256, None).await?;
        let text = resp.text().await.map_err(|e| failure::from_transport(&e))?;
        parse_list(&text)
    }

    /// Flat, paginated walk of everything under `prefix`.
    pub async fn list_all(&self, prefix: &str, cancel: Option<&CancellationToken>) -> AppResult<Vec<ObjectInfo>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            if cancel.is_some_and(|c| c.is_cancelled()) {
                return Err(AppFailure::Cancelled);
            }
            let page = self.list_page(prefix, None, token.as_deref(), LIST_PAGE).await?;
            out.extend(page.objects);
            match page.next_token {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
    }

    // ------------------------------------------------------------- read --

    pub async fn head(&self, key: &str) -> AppResult<HeadInfo> {
        let resp = self.execute(Method::HEAD, Some(key), vec![], vec![], None, EMPTY_SHA256, None).await?;
        let h = resp.headers();
        let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        Ok(HeadInfo {
            size: get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0),
            etag: get("etag").map(|e| e.trim_matches('"').to_string()).unwrap_or_default(),
            content_type: get("content-type"),
            last_modified_ms: get("last-modified")
                .and_then(|v| DateTime::parse_from_rfc2822(&v).ok())
                .map(|d| d.timestamp_millis()),
        })
    }

    /// Small ranged read (magic probe, key-header probe, text preview).
    pub async fn get_range(&self, key: &str, start: u64, len: u64) -> AppResult<Bytes> {
        let range = format!("bytes={}-{}", start, start + len.max(1) - 1);
        let resp = self
            .execute(Method::GET, Some(key), vec![], vec![("range".into(), range)], None, EMPTY_SHA256, None)
            .await?;
        resp.bytes().await.map_err(|e| failure::from_transport(&e))
    }

    /// Streamed GET, optionally resuming from `from` (`Range: bytes=from-`).
    pub async fn get_stream(&self, key: &str, from: u64, cancel: Option<&CancellationToken>) -> AppResult<Response> {
        let headers = if from > 0 { vec![("range".into(), format!("bytes={from}-"))] } else { vec![] };
        self.execute(Method::GET, Some(key), vec![], headers, None, EMPTY_SHA256, cancel).await
    }

    // ------------------------------------------------------------ write --

    /// Single PUT (≤ 8 MiB). The real SHA-256 is signed for small bodies.
    pub async fn put_object(
        &self,
        key: &str,
        data: Bytes,
        content_type: Option<&str>,
        progress: Option<BodyProgress>,
        cancel: Option<&CancellationToken>,
    ) -> AppResult<String> {
        let hash = sigv4::sha256_hex(&data);
        let mut headers = vec![];
        if let Some(ct) = content_type {
            headers.push(("content-type".to_string(), ct.to_string()));
        }
        let len = data.len() as u64;
        let resp = self
            .execute(Method::PUT, Some(key), vec![], headers, Some((progress_body(data, progress), len)), &hash, cancel)
            .await?;
        Ok(etag_of(&resp))
    }

    pub async fn create_multipart(&self, key: &str, content_type: Option<&str>) -> AppResult<String> {
        let mut headers = vec![];
        if let Some(ct) = content_type {
            headers.push(("content-type".to_string(), ct.to_string()));
        }
        let resp = self
            .execute(Method::POST, Some(key), vec![("uploads".into(), String::new())], headers, None, EMPTY_SHA256, None)
            .await?;
        let text = resp.text().await.map_err(|e| failure::from_transport(&e))?;
        let root = xml::parse(&text)?;
        root.text_of("UploadId")
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| AppFailure::unknown("no UploadId in response"))
    }

    /// One part, signed with `UNSIGNED-PAYLOAD` so it is never hashed (§4.1).
    pub async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        data: Bytes,
        progress: Option<BodyProgress>,
        cancel: Option<&CancellationToken>,
    ) -> AppResult<String> {
        let q = vec![("partNumber".into(), part_number.to_string()), ("uploadId".into(), upload_id.to_string())];
        let len = data.len() as u64;
        let resp = self
            .execute(Method::PUT, Some(key), q, vec![], Some((progress_body(data, progress), len)), UNSIGNED_PAYLOAD, cancel)
            .await?;
        let etag = etag_of(&resp);
        if etag.is_empty() {
            return Err(AppFailure::network("part upload returned no ETag"));
        }
        Ok(etag)
    }

    pub async fn complete_multipart(&self, key: &str, upload_id: &str, parts: &[CompletedPart]) -> AppResult<()> {
        let mut sorted = parts.to_vec();
        sorted.sort_by_key(|p| p.part_number);
        let mut body = String::from("<CompleteMultipartUpload>");
        for p in &sorted {
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>\"{}\"</ETag></Part>",
                p.part_number,
                xml_escape(&p.etag)
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let resp = self.post_xml(key, vec![("uploadId".into(), upload_id.into())], body).await?;
        // S3 can return 200 with an <Error> body for Complete.
        let text = resp.text().await.map_err(|e| failure::from_transport(&e))?;
        if let Some((code, msg)) = xml::parse_error(&text) {
            return Err(failure::from_s3(200, Some(&code), &msg));
        }
        Ok(())
    }

    /// Abort (avoids orphaned storage charges). A missing upload is success.
    pub async fn abort_multipart(&self, key: &str, upload_id: &str) -> AppResult<()> {
        match self
            .execute(Method::DELETE, Some(key), vec![("uploadId".into(), upload_id.into())], vec![], None, EMPTY_SHA256, None)
            .await
        {
            Ok(_) | Err(AppFailure::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn copy_object(&self, src: &str, dst: &str) -> AppResult<()> {
        let source = format!("/{}/{}", uri_encode(&self.bucket, true), uri_encode(src, false));
        let resp = self
            .execute(Method::PUT, Some(dst), vec![], vec![("x-amz-copy-source".into(), source)], None, EMPTY_SHA256, None)
            .await?;
        let text = resp.text().await.map_err(|e| failure::from_transport(&e))?;
        if let Some((code, msg)) = xml::parse_error(&text) {
            return Err(failure::from_s3(200, Some(&code), &msg));
        }
        Ok(())
    }

    pub async fn delete_object(&self, key: &str) -> AppResult<()> {
        match self.execute(Method::DELETE, Some(key), vec![], vec![], None, EMPTY_SHA256, None).await {
            Ok(_) | Err(AppFailure::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Batch delete, ≤ 1000 keys per call. Returns the keys that failed.
    pub async fn delete_objects(&self, keys: &[String]) -> AppResult<Vec<DeleteError>> {
        let mut failed = Vec::new();
        for batch in keys.chunks(DELETE_BATCH) {
            let mut body = String::from("<Delete><Quiet>true</Quiet>");
            for k in batch {
                body.push_str(&format!("<Object><Key>{}</Key></Object>", xml_escape(k)));
            }
            body.push_str("</Delete>");
            let resp = self.post_xml_md5(vec![("delete".into(), String::new())], body).await?;
            let text = resp.text().await.map_err(|e| failure::from_transport(&e))?;
            let root = xml::parse(&text)?;
            for e in root.children_named("Error") {
                failed.push(DeleteError {
                    key: e.text_of("Key").unwrap_or("").to_string(),
                    code: e.text_of("Code").unwrap_or("").to_string(),
                    message: e.text_of("Message").unwrap_or("").to_string(),
                });
            }
        }
        Ok(failed)
    }

    async fn post_xml(&self, key: &str, q: Vec<(String, String)>, body: String) -> AppResult<Response> {
        let bytes = Bytes::from(body);
        let hash = sigv4::sha256_hex(&bytes);
        let len = bytes.len() as u64;
        self.execute(
            Method::POST,
            Some(key),
            q,
            vec![("content-type".into(), "application/xml".into())],
            Some((reqwest::Body::from(bytes), len)),
            &hash,
            None,
        )
        .await
    }

    async fn post_xml_md5(&self, q: Vec<(String, String)>, body: String) -> AppResult<Response> {
        use md5::Digest;
        let bytes = Bytes::from(body);
        let hash = sigv4::sha256_hex(&bytes);
        let md5 = base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(&bytes));
        let len = bytes.len() as u64;
        self.execute(
            Method::POST,
            None,
            q,
            vec![("content-type".into(), "application/xml".into()), ("content-md5".into(), md5)],
            Some((reqwest::Body::from(bytes), len)),
            &hash,
            None,
        )
        .await
    }

    // ---------------------------------------------------------- presign --

    /// Local-only presigned GET (share links, media streaming). No network.
    pub fn presign_get(&self, key: &str, ttl_secs: u64, content_disposition: Option<&str>) -> String {
        let path = self.path_for(Some(key));
        let extra: Vec<(String, String)> = content_disposition
            .map(|d| vec![("response-content-disposition".to_string(), d.to_string())])
            .unwrap_or_default();
        let q = self.signer.presign("GET", &self.host, &path, &extra, ttl_secs, self.clock.now());
        format!("{}{path}?{q}", self.base)
    }
}

async fn with_cancel<T>(cancel: Option<&CancellationToken>, fut: impl Future<Output = AppResult<T>>) -> AppResult<T> {
    match cancel {
        Some(c) => tokio::select! {
            biased;
            _ = c.cancelled() => Err(AppFailure::Cancelled),
            r = fut => r,
        },
        None => fut.await,
    }
}

fn progress_body(data: Bytes, progress: Option<BodyProgress>) -> reqwest::Body {
    let stream = futures::stream::unfold(0usize, move |off| {
        let data = data.clone();
        let progress = progress.clone();
        async move {
            if off >= data.len() {
                return None;
            }
            let end = (off + UPLOAD_SLICE).min(data.len());
            let slice = data.slice(off..end);
            if let Some(p) = &progress {
                p((end - off) as u64);
            }
            Some((Ok::<Bytes, std::io::Error>(slice), end))
        }
    });
    reqwest::Body::wrap_stream(stream)
}

fn etag_of(resp: &Response) -> String {
    resp.headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|e| e.trim_matches('"').to_string())
        .unwrap_or_default()
}

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {
                o.push_str(&format!("&#{};", c as u32))
            }
            c => o.push(c),
        }
    }
    o
}

pub(crate) fn parse_list(text: &str) -> AppResult<ListPage> {
    let root = xml::parse(text)?;
    let objects = root
        .children_named("Contents")
        .filter_map(|c| {
            Some(ObjectInfo {
                key: c.text_of("Key")?.to_string(),
                size: c.text_of("Size").and_then(|s| s.trim().parse().ok()).unwrap_or(0),
                etag: c.text_of("ETag").unwrap_or("").trim_matches('"').to_string(),
                last_modified_ms: c
                    .text_of("LastModified")
                    .and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok())
                    .map(|d| d.timestamp_millis()),
            })
        })
        .collect();
    let common_prefixes = root
        .children_named("CommonPrefixes")
        .filter_map(|c| c.text_of("Prefix").map(str::to_string))
        .collect();
    let truncated = root.text_of("IsTruncated").map(|s| s.trim() == "true").unwrap_or(false);
    let next_token = root
        .text_of("NextContinuationToken")
        .filter(|t| truncated && !t.is_empty())
        .map(str::to_string);
    Ok(ListPage { objects, common_prefixes, next_token })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_parsing() {
        let xml = r#"<ListBucketResult><IsTruncated>false</IsTruncated>
<Contents><Key>a/b.txt</Key><Size>5</Size><ETag>"e1-3"</ETag><LastModified>2026-01-02T03:04:05.000Z</LastModified></Contents>
<NextContinuationToken>ignored</NextContinuationToken></ListBucketResult>"#;
        let p = parse_list(xml).unwrap();
        assert_eq!(p.objects.len(), 1);
        assert_eq!(p.objects[0].etag, "e1-3");
        assert!(p.objects[0].is_multipart_etag());
        assert!(p.objects[0].last_modified_ms.is_some());
        assert!(p.next_token.is_none(), "not truncated → no next page");
    }

    /// Live check of TLS + signing + XML error mapping against a real
    /// S3-compatible endpoint with fake keys. `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_endpoint_rejects_fake_keys() {
        let cfg = R2Config {
            account_id: "x".into(),
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            bucket: "byteflow-nonexistent-test-bucket".into(),
            public_url: None,
            endpoint: Some(std::env::var("BF_TEST_ENDPOINT").unwrap_or_else(|_| "https://s3.amazonaws.com".into())),
        };
        let c = R2Client::new(&cfg).unwrap();
        let e = c.list_page("", None, None, 1).await.unwrap_err();
        eprintln!("{e}");
        assert!(matches!(e, bf_core::AppFailure::Auth { .. } | bf_core::AppFailure::Permission { .. } | bf_core::AppFailure::NotFound { .. }), "{e}");
    }

    #[test]
    fn presign_shape() {
        let cfg = R2Config {
            account_id: "acc".into(),
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            bucket: "bkt".into(),
            public_url: None,
            endpoint: None,
        };
        let c = R2Client::new(&cfg).unwrap();
        let u = c.presign_get("dir/a b+c.txt", 3600, None);
        assert!(u.starts_with("https://acc.r2.cloudflarestorage.com/bkt/dir/a%20b%2Bc.txt?"));
        assert!(u.contains("X-Amz-Expires=3600"));
        assert!(u.contains("auto%2Fs3%2Faws4_request"));
        assert_eq!(xml_escape("a&<b>"), "a&amp;&lt;b&gt;");
    }
}
