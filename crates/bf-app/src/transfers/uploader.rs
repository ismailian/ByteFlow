//! Upload primitives shared by the transfer engine and the encryption engine.
//!
//! * ≤ 8 MiB: one `PutObject`, body streamed in 64 KiB views.
//! * > 8 MiB: multipart. Part size 8 MiB, raised (rounded up to a MiB) so the
//!   count stays < 1000, never below 5 MiB. Parts upload with **bounded
//!   per-task concurrency** ([`PART_CONCURRENCY`]) — the `CLAUDE.md` target the
//!   mobile build left [PARTIAL]. Memory stays bounded at
//!   `PART_CONCURRENCY × part_size` per task.
//! * Resume is at part granularity: the upload id is persisted the moment it
//!   exists and confirmed parts after every part; progress is seeded only from
//!   confirmed parts (never from an in-flight part's partial bytes).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{StreamExt, stream};
use tokio_util::sync::CancellationToken;

use bf_core::{AppFailure, AppResult};
use bf_data::r2::{BodyProgress, CompletedPart, R2Client};

pub const MIB: u64 = 1024 * 1024;
pub const SINGLE_PUT_MAX: u64 = 8 * MIB;
pub const DEFAULT_PART: u64 = 8 * MIB;
pub const MIN_PART: u64 = 5 * MIB;
pub const MAX_PARTS: u64 = 1000;
pub const PART_CONCURRENCY: usize = 3;

pub fn part_size_for(total: u64) -> u64 {
    let mut ps = DEFAULT_PART;
    if total.div_ceil(ps) >= MAX_PARTS {
        ps = total.div_ceil(MAX_PARTS - 1).div_ceil(MIB) * MIB;
    }
    ps.max(MIN_PART)
}

pub fn part_count(total: u64, part: u64) -> u32 {
    total.div_ceil(part).max(1) as u32
}

/// Absolute bytes transferred so far.
pub type ProgressFn = Arc<dyn Fn(u64) + Send + Sync>;

pub trait ResumeSink: Send + Sync {
    fn on_upload_id(&self, id: String) -> BoxFuture<'_, AppResult<()>>;
    fn on_parts(&self, parts: Vec<CompletedPart>, confirmed: u64) -> BoxFuture<'_, AppResult<()>>;
}

pub struct UploadSpec<'a> {
    pub client: &'a R2Client,
    pub key: &'a str,
    pub path: &'a Path,
    pub content_type: Option<&'a str>,
    pub cancel: &'a CancellationToken,
    pub progress: ProgressFn,
}

async fn read_range(path: PathBuf, offset: u64, len: u64) -> AppResult<Bytes> {
    crate::blocking(move || {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path).map_err(|e| AppFailure::io("open upload source", e))?;
        f.seek(SeekFrom::Start(offset)).map_err(|e| AppFailure::io("seek", e))?;
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf).map_err(|e| AppFailure::io("read upload source", e))?;
        Ok(Bytes::from(buf))
    })
    .await
}

pub async fn upload_single(spec: &UploadSpec<'_>, total: u64) -> AppResult<()> {
    let data = read_range(spec.path.to_path_buf(), 0, total).await?;
    let sent = Arc::new(AtomicU64::new(0));
    let p = spec.progress.clone();
    let s2 = sent.clone();
    let body_progress: BodyProgress = Arc::new(move |n| p(s2.fetch_add(n, Ordering::Relaxed) + n));
    spec.client
        .put_object(spec.key, data, spec.content_type, Some(body_progress), Some(spec.cancel))
        .await?;
    (spec.progress)(total);
    Ok(())
}

pub struct MultipartResume {
    pub upload_id: Option<String>,
    pub parts: Vec<CompletedPart>,
}

/// Multipart upload. With `sink`, state is persisted for resume; with
/// `abort_on_failure`, any failure aborts the upload (encryption round trip).
pub async fn upload_multipart(
    spec: &UploadSpec<'_>,
    total: u64,
    resume: MultipartResume,
    sink: Option<&dyn ResumeSink>,
    abort_on_failure: bool,
) -> AppResult<()> {
    let part = part_size_for(total);
    let count = part_count(total, part);
    let upload_id = match resume.upload_id {
        Some(id) => id,
        None => {
            let id = spec.client.create_multipart(spec.key, spec.content_type).await?;
            if let Some(s) = sink {
                s.on_upload_id(id.clone()).await?;
            }
            id
        }
    };
    let result = run_parts(spec, total, part, count, &upload_id, resume.parts, sink).await;
    match result {
        Ok(parts) => {
            let done = spec.client.complete_multipart(spec.key, &upload_id, &parts).await;
            if done.is_err() && abort_on_failure {
                let _ = spec.client.abort_multipart(spec.key, &upload_id).await;
            }
            done?;
            (spec.progress)(total);
            Ok(())
        }
        Err(e) => {
            if abort_on_failure {
                let _ = spec.client.abort_multipart(spec.key, &upload_id).await;
            }
            Err(e)
        }
    }
}

async fn run_parts(
    spec: &UploadSpec<'_>,
    total: u64,
    part: u64,
    count: u32,
    upload_id: &str,
    already: Vec<CompletedPart>,
    sink: Option<&dyn ResumeSink>,
) -> AppResult<Vec<CompletedPart>> {
    let len_of = |n: u32| -> u64 {
        let start = (n as u64 - 1) * part;
        (total - start).min(part)
    };
    let done_set: HashSet<u32> = already.iter().map(|p| p.part_number).collect();
    let mut parts: Vec<CompletedPart> = already.into_iter().filter(|p| p.part_number <= count).collect();
    let confirmed = Arc::new(AtomicU64::new(parts.iter().map(|p| len_of(p.part_number)).sum()));
    let inflight = Arc::new(AtomicU64::new(0));
    (spec.progress)(confirmed.load(Ordering::Relaxed));

    let pending: Vec<u32> = (1..=count).filter(|n| !done_set.contains(n)).collect();
    let mut results = stream::iter(pending)
        .map(|n| {
            let confirmed = confirmed.clone();
            let inflight = inflight.clone();
            let progress = spec.progress.clone();
            async move {
                if spec.cancel.is_cancelled() {
                    return Err(AppFailure::Cancelled);
                }
                let len = len_of(n);
                let data = read_range(spec.path.to_path_buf(), (n as u64 - 1) * part, len).await?;
                let sent = Arc::new(AtomicU64::new(0));
                let (s2, c2, i2) = (sent.clone(), confirmed.clone(), inflight.clone());
                let bp: BodyProgress = Arc::new(move |k| {
                    s2.fetch_add(k, Ordering::Relaxed);
                    let inf = i2.fetch_add(k, Ordering::Relaxed) + k;
                    progress(c2.load(Ordering::Relaxed) + inf);
                });
                let r = spec.client.upload_part(spec.key, upload_id, n, data, Some(bp), Some(spec.cancel)).await;
                inflight.fetch_sub(sent.load(Ordering::Relaxed), Ordering::Relaxed);
                let etag = r?;
                confirmed.fetch_add(len, Ordering::Relaxed);
                Ok(CompletedPart { part_number: n, etag })
            }
        })
        .buffer_unordered(PART_CONCURRENCY);

    while let Some(r) = results.next().await {
        let p = r?;
        parts.push(p);
        if let Some(s) = sink {
            s.on_parts(parts.clone(), confirmed.load(Ordering::Relaxed)).await?;
        }
    }
    Ok(parts)
}

/// Upload a local file (single or multipart, no resume, abort on failure).
pub async fn upload_file_once(spec: &UploadSpec<'_>) -> AppResult<()> {
    let total = tokio::fs::metadata(spec.path).await.map_err(|e| AppFailure::io("stat", e))?.len();
    if total <= SINGLE_PUT_MAX {
        upload_single(spec, total).await
    } else {
        upload_multipart(spec, total, MultipartResume { upload_id: None, parts: vec![] }, None, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn part_maths() {
        assert_eq!(part_size_for(9 * MIB), 8 * MIB);
        assert_eq!(part_count(9 * MIB, 8 * MIB), 2);
        assert!(part_size_for(100 * 1024 * MIB) > 8 * MIB, "12800 parts would be too many");
    }

    #[test]
    fn part_count_stays_under_1000() {
        for total in [8 * MIB + 1, 7_999 * MIB, 8_000 * MIB, 50 * 1024 * MIB, 5 * 1024 * 1024 * MIB] {
            let ps = part_size_for(total);
            assert!(ps >= MIN_PART);
            assert!(part_count(total, ps) < 1000, "total={total} ps={ps}");
            assert_eq!(ps % MIB, 0);
        }
    }
}
