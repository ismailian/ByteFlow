//! Update checker.
//!
//! On launch the app fetches [`MANIFEST_URL`] (`latest.json`, attached to
//! every GitHub release; GitHub redirects `releases/latest/download/` to the
//! newest one) and offers the newer version, if any. The installer is
//! downloaded over HTTPS from this repository's releases only,
//! checked against the manifest's SHA-256, and handed to the platform layer
//! to run. Nothing about the vault or the user is sent: a plain GET with a
//! generic user agent.

use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;

use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use bf_core::{AppFailure, AppResult};

pub const MANIFEST_URL: &str = "https://github.com/ismailian/ByteFlow/releases/latest/download/latest.json";
/// Installers are only ever fetched from here.
const DOWNLOAD_PREFIX: &str = "https://github.com/ismailian/ByteFlow/releases/download/";
/// Refuse anything absurd (the setup is ~8 MB).
const MAX_INSTALLER_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
}

impl Release {
    /// The installer's file name (last URL segment, sanitised).
    pub fn file_name(&self) -> String {
        let raw = self.url.rsplit('/').next().unwrap_or_default();
        let clean: String = raw.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
        if clean.ends_with(".exe") && clean.len() > 4 { clean } else { format!("ByteFlow-Setup-{}.exe", self.version) }
    }

    fn validate(&self) -> AppResult<()> {
        if parse_version(&self.version).is_none() {
            return Err(AppFailure::unknown("update manifest: bad version"));
        }
        if !self.url.starts_with(DOWNLOAD_PREFIX) || self.url.contains("..") {
            return Err(AppFailure::unknown("update manifest: unexpected download location"));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AppFailure::unknown("update manifest: bad checksum"));
        }
        Ok(())
    }
}

/// `x.y.z` (an optional leading `v`; pre-release/build suffixes ignored).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

fn client(read_timeout: Duration) -> AppResult<reqwest::Client> {
    static TLS_INIT: Once = Once::new();
    TLS_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::builder()
        .tls_backend_rustls()
        .https_only(true)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(read_timeout)
        .redirect(reqwest::redirect::Policy::limited(3))
        .user_agent("ByteFlowDesktop")
        .build()
        .map_err(|_| AppFailure::network("update: http client"))
}

/// The newer release, if the site offers one. Errors (offline, site down)
/// are for the caller to ignore quietly.
pub async fn check(current: &str) -> AppResult<Option<Release>> {
    let resp = client(Duration::from_secs(15))?
        .get(MANIFEST_URL)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .await
        .map_err(|_| AppFailure::network("update check failed"))?;
    if !resp.status().is_success() {
        return Err(AppFailure::network(format!("update check: HTTP {}", resp.status().as_u16())));
    }
    let body = resp.bytes().await.map_err(|_| AppFailure::network("update check failed"))?;
    if body.len() > 16 * 1024 {
        return Err(AppFailure::unknown("update manifest too large"));
    }
    let rel: Release = serde_json::from_slice(&body).map_err(|_| AppFailure::unknown("update manifest unreadable"))?;
    rel.validate()?;
    Ok(is_newer(&rel.version, current).then_some(rel))
}

/// Download the installer into `dir`, reporting `(done, total)` bytes, and
/// verify its SHA-256. A mismatch deletes the file.
pub async fn download(rel: &Release, dir: &Path, progress: impl Fn(u64, Option<u64>)) -> AppResult<PathBuf> {
    rel.validate()?;
    tokio::fs::create_dir_all(dir).await.map_err(|e| AppFailure::io("create update folder", e))?;
    let path = dir.join(rel.file_name());
    let part = dir.join(format!("{}.part", rel.file_name()));
    let resp = client(Duration::from_secs(60))?.get(&rel.url).send().await.map_err(|_| AppFailure::network("download failed"))?;
    if !resp.status().is_success() {
        return Err(AppFailure::network(format!("download: HTTP {}", resp.status().as_u16())));
    }
    let total = resp.content_length().or(rel.size);
    if total.is_some_and(|t| t > MAX_INSTALLER_BYTES) {
        return Err(AppFailure::unknown("installer too large"));
    }
    let mut file = tokio::fs::File::create(&part).await.map_err(|e| AppFailure::io("create installer", e))?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut stream = resp.bytes_stream();
    progress(0, total);
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => {
                drop(file);
                let _ = tokio::fs::remove_file(&part).await;
                return Err(AppFailure::network("download interrupted"));
            }
        };
        done += chunk.len() as u64;
        if done > MAX_INSTALLER_BYTES {
            drop(file);
            let _ = tokio::fs::remove_file(&part).await;
            return Err(AppFailure::unknown("installer too large"));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| AppFailure::io("write installer", e))?;
        progress(done, total);
    }
    file.flush().await.map_err(|e| AppFailure::io("write installer", e))?;
    drop(file);
    if !hex::encode(hasher.finalize()).eq_ignore_ascii_case(&rel.sha256) {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(AppFailure::unknown("the download is damaged (checksum mismatch)"));
    }
    let _ = tokio::fs::remove_file(&path).await;
    tokio::fs::rename(&part, &path).await.map_err(|e| AppFailure::io("save installer", e))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert_eq!(parse_version("0.4.1"), Some((0, 4, 1)));
        assert_eq!(parse_version("v1.2.3-beta"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert!(is_newer("0.4.1", "0.4.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.4.0", "0.4.0"));
        assert!(!is_newer("0.3.9", "0.4.0"));
        assert!(!is_newer("garbage", "0.4.0"));
    }

    #[test]
    fn manifest_is_validated() {
        let good = Release {
            version: "0.5.0".into(),
            url: format!("{DOWNLOAD_PREFIX}v0.5.0/ByteFlow-Setup-0.5.0-windows-x64.exe"),
            sha256: "a".repeat(64),
            size: None,
        };
        assert!(good.validate().is_ok());
        assert_eq!(good.file_name(), "ByteFlow-Setup-0.5.0-windows-x64.exe");
        let mut bad = good.clone();
        bad.url = "https://evil.example/ByteFlow.exe".into();
        assert!(bad.validate().is_err());
        bad.url = "https://github.com/someone-else/ByteFlow/releases/download/v1.0.0/x.exe".into();
        assert!(bad.validate().is_err());
        let mut bad = good.clone();
        bad.sha256 = "xyz".into();
        assert!(bad.validate().is_err());
    }
}
