//! Media streaming (0.4.0): random-access reads of a bucket object for the
//! built-in player, without downloading the whole file first.
//!
//! The mobile app hands its player a presigned URL. Desktop does better on
//! two counts: nothing is written to disk (no temp copy, no OS/HTTP cache
//! holding a URL or plaintext), and **encrypted** objects stream too — their
//! 64 KiB segments are authenticated independently (`SegmentReader`), so the
//! ranges the player asks for are fetched with ranged GETs and decrypted in
//! memory.
//!
//! Reads are served from a small cache of ~1 MiB blocks; reading block *n*
//! also prefetches the next few in the background, so sequential playback
//! runs ahead of the decoder while a seek costs one block's round trip.
//! [`MediaStream::read_at`] blocks the calling (player) thread; it must not
//! be called from a tokio worker.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub use bytes::Bytes;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use bf_core::crypto::file_crypto::{KEY_HEADER_PROBE_BYTES, SegmentReader};
use bf_core::{AppFailure, AppResult};

use crate::session::{ClientHandle, Session};

/// Ranged reads of one object (R2 in the app; in memory in tests).
pub trait RangeFetch: Send + Sync {
    fn fetch(&self, start: u64, len: u64) -> BoxFuture<'_, AppResult<Bytes>>;
}

/// An object held in memory (tests and tools).
pub struct MemoryFetch(pub Vec<u8>);

impl RangeFetch for MemoryFetch {
    fn fetch(&self, start: u64, len: u64) -> BoxFuture<'_, AppResult<Bytes>> {
        let s = (start as usize).min(self.0.len());
        let e = ((start + len) as usize).min(self.0.len());
        let b = Bytes::copy_from_slice(&self.0[s..e]);
        Box::pin(async move { Ok(b) })
    }
}

struct R2Fetch {
    client: ClientHandle,
    key: String,
}

impl RangeFetch for R2Fetch {
    fn fetch(&self, start: u64, len: u64) -> BoxFuture<'_, AppResult<Bytes>> {
        Box::pin(async move { self.client.get().get_range(&self.key, start, len).await })
    }
}

/// Plaintext block size for plain objects (encrypted objects use whole
/// segments, as close to this as the chunk size allows).
const BLOCK: u64 = 1024 * 1024;
/// Blocks fetched ahead of the one being read.
const READ_AHEAD: u64 = 4;
/// Blocks kept in memory (≈ 32 MiB).
const MAX_BLOCKS: usize = 32;
const ATTEMPTS: usize = 3;

type Block = Arc<Zeroizing<Vec<u8>>>;
type Slot = Arc<OnceCell<Block>>;

enum Source {
    Plain,
    Encrypted { reader: Arc<SegmentReader>, segs_per_block: u64 },
}

#[derive(Default)]
struct Cache {
    slots: HashMap<u64, Slot>,
    order: VecDeque<u64>,
}

pub struct MediaStream {
    len: u64,
    block: u64,
    source: Source,
    fetch: Arc<dyn RangeFetch>,
    cancel: CancellationToken,
    rt: tokio::runtime::Handle,
    cache: Mutex<Cache>,
    /// Bytes fetched from R2 so far (for the UI).
    fetched: AtomicU64,
}

impl MediaStream {
    /// Probe the object (size, and for an encrypted one its header and key —
    /// which can fail with the passphrase prompt's `external_key` error).
    pub async fn open(session: &Session, key: &str) -> AppResult<Arc<Self>> {
        let client = session.client.clone();
        let size = client.get().head(key).await?.size;
        let master = if session.encryption.is_encrypted(key) { Some(session.encryption.resolve_key(key).await?) } else { None };
        let fetch = Arc::new(R2Fetch { client, key: key.to_string() });
        Self::new(fetch, size, master.as_deref().map(|m| &m[..]), session.cancel.child_token()).await
    }

    /// A stream over any ranged source of `size` stored bytes; with
    /// `master_key` the object is an encrypted one (its header is fetched).
    pub async fn new(fetch: Arc<dyn RangeFetch>, size: u64, master_key: Option<&[u8]>, cancel: CancellationToken) -> AppResult<Arc<Self>> {
        let (source, len, block) = match master_key {
            Some(master) => {
                let head = fetch.fetch(0, KEY_HEADER_PROBE_BYTES.min(size.max(1))).await?;
                let reader = SegmentReader::new(&head, size, master)?;
                let segs_per_block = (BLOCK / reader.chunk_size()).max(1);
                let (len, block) = (reader.plain_len(), segs_per_block * reader.chunk_size());
                (Source::Encrypted { reader: Arc::new(reader), segs_per_block }, len, block)
            }
            None => (Source::Plain, size, BLOCK),
        };
        Ok(Arc::new(Self {
            len,
            block,
            source,
            fetch,
            cancel,
            rt: tokio::runtime::Handle::current(),
            cache: Mutex::new(Cache::default()),
            fetched: AtomicU64::new(0),
        }))
    }

    /// Plaintext length.
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn fetched(&self) -> u64 {
        self.fetched.load(Ordering::Relaxed)
    }

    /// Stop all reads (they fail from now on).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Blocking: fill `buf` from plaintext offset `off`. Returns the bytes
    /// read (short only at the end of the object).
    pub fn read_at(self: &Arc<Self>, off: u64, buf: &mut [u8]) -> AppResult<usize> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            if pos >= self.len {
                break;
            }
            let b = pos / self.block;
            let this = self.clone();
            let data = self.rt.block_on(async move { this.get(b).await })?;
            self.prefetch(b);
            let inner = (pos - b * self.block) as usize;
            if inner >= data.len() {
                break;
            }
            let n = (buf.len() - done).min(data.len() - inner);
            buf[done..done + n].copy_from_slice(&data[inner..inner + n]);
            done += n;
        }
        Ok(done)
    }

    fn blocks(&self) -> u64 {
        self.len.div_ceil(self.block)
    }

    fn slot(&self, b: u64) -> Slot {
        let mut c = self.cache.lock();
        if let Some(s) = c.slots.get(&b) {
            let s = s.clone();
            c.order.retain(|x| *x != b);
            c.order.push_back(b);
            return s;
        }
        let s: Slot = Arc::new(OnceCell::new());
        c.slots.insert(b, s.clone());
        c.order.push_back(b);
        while c.order.len() > MAX_BLOCKS {
            if let Some(old) = c.order.pop_front() {
                c.slots.remove(&old);
            }
        }
        s
    }

    fn prefetch(self: &Arc<Self>, b: u64) {
        let last = self.blocks().saturating_sub(1);
        for n in (b + 1)..=(b + READ_AHEAD).min(last) {
            let present = self.cache.lock().slots.get(&n).is_some_and(|s| s.initialized());
            if !present {
                let this = self.clone();
                self.rt.spawn(async move {
                    let _ = this.get(n).await;
                });
            }
        }
    }

    async fn get(&self, b: u64) -> AppResult<Block> {
        let slot = self.slot(b);
        slot.get_or_try_init(|| self.fetch(b)).await.cloned()
    }

    async fn fetch(&self, b: u64) -> AppResult<Block> {
        let mut last = AppFailure::network("stream read failed");
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(250 * attempt as u64)).await;
            }
            let r = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(AppFailure::Cancelled),
                r = self.fetch_once(b) => r,
            };
            match r {
                Ok(v) => return Ok(v),
                // Integrity failures are final: never retry into a different answer.
                Err(e @ AppFailure::Crypto { .. }) => return Err(e),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    async fn fetch_once(&self, b: u64) -> AppResult<Block> {
        match &self.source {
            Source::Plain => {
                let start = b * self.block;
                let len = self.block.min(self.len - start);
                let data = self.fetch.fetch(start, len).await?;
                if data.len() as u64 != len {
                    return Err(AppFailure::network("short read"));
                }
                self.fetched.fetch_add(len, Ordering::Relaxed);
                Ok(Arc::new(Zeroizing::new(data.to_vec())))
            }
            Source::Encrypted { reader, segs_per_block } => {
                let first = b * segs_per_block;
                let (s, e) = reader.cipher_range(first, *segs_per_block);
                let ct = self.fetch.fetch(s, e - s).await?;
                if ct.len() as u64 != e - s {
                    return Err(AppFailure::network("short read"));
                }
                self.fetched.fetch_add(e - s, Ordering::Relaxed);
                let reader = reader.clone();
                let plain = crate::blocking(move || reader.open_segments(first, &ct)).await?;
                Ok(Arc::new(Zeroizing::new(plain)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bf_core::crypto::file_crypto::{encrypt_file, no_control};
    use bf_core::crypto::secure_bytes::random_bytes;
    use std::sync::atomic::AtomicUsize;

    /// Serves an in-memory object, counting requests.
    struct Mem(Vec<u8>, AtomicUsize);
    impl RangeFetch for Mem {
        fn fetch(&self, start: u64, len: u64) -> BoxFuture<'_, AppResult<Bytes>> {
            self.1.fetch_add(1, Ordering::SeqCst);
            let end = ((start + len) as usize).min(self.0.len());
            let b = Bytes::copy_from_slice(&self.0[start as usize..end]);
            Box::pin(async move { Ok(b) })
        }
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 253) as u8).collect()
    }

    fn check(stream: &Arc<MediaStream>, plain: &[u8]) {
        assert_eq!(stream.len(), plain.len() as u64);
        let mut rng = 12345u64;
        for _ in 0..60 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let off = rng % (plain.len() as u64 + 10);
            let n = (rng >> 33) as usize % (3 * 1024 * 1024) + 1;
            let mut buf = vec![0u8; n];
            let got = stream.read_at(off, &mut buf).unwrap();
            let want = &plain[(off as usize).min(plain.len())..((off as usize).saturating_add(n)).min(plain.len())];
            assert_eq!(got, want.len(), "length at {off}+{n}");
            assert_eq!(&buf[..got], want, "bytes at {off}+{n}");
        }
    }

    #[test]
    fn plain_and_encrypted_reads_match_the_plaintext() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let plain = data(5 * 1024 * 1024 + 4321);

        let mem = Arc::new(Mem(plain.clone(), AtomicUsize::new(0)));
        let s = rt.block_on(MediaStream::new(mem.clone(), plain.len() as u64, None, CancellationToken::new())).unwrap();
        check(&s, &plain);

        let d = tempfile::tempdir().unwrap();
        let (src, enc) = (d.path().join("p"), d.path().join("e"));
        std::fs::write(&src, &plain).unwrap();
        let key = random_bytes(32);
        encrypt_file(&src, &enc, &key, bf_core::crypto::file_crypto::DEFAULT_CHUNK_SIZE, None, &mut no_control()).unwrap();
        let ct = std::fs::read(&enc).unwrap();
        let mem = Arc::new(Mem(ct.clone(), AtomicUsize::new(0)));
        assert!(rt.block_on(MediaStream::new(mem.clone(), ct.len() as u64, Some(&random_bytes(32)), CancellationToken::new())).is_err(), "wrong key");
        let s = rt.block_on(MediaStream::new(mem.clone(), ct.len() as u64, Some(&key), CancellationToken::new())).unwrap();
        check(&s, &plain);

        // A tampered byte fails the read instead of returning bad plaintext.
        let mut bad = ct.clone();
        let at = bad.len() / 2;
        bad[at] ^= 1;
        let mem = Arc::new(Mem(bad, AtomicUsize::new(0)));
        let s = rt.block_on(MediaStream::new(mem, ct.len() as u64, Some(&key), CancellationToken::new())).unwrap();
        let mut buf = vec![0u8; plain.len()];
        assert!(matches!(s.read_at(0, &mut buf), Err(AppFailure::Crypto { .. })));

        // Cancelled streams fail fast.
        let mem = Arc::new(Mem(plain.clone(), AtomicUsize::new(0)));
        let s = rt.block_on(MediaStream::new(mem, plain.len() as u64, None, CancellationToken::new())).unwrap();
        s.cancel();
        assert!(s.read_at(0, &mut [0u8; 16]).is_err());
    }

    #[test]
    fn a_read_fetches_only_nearby_blocks() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let plain = data(64 * 1024 * 1024);
        let mem = Arc::new(Mem(plain.clone(), AtomicUsize::new(0)));
        let s = rt.block_on(MediaStream::new(mem.clone(), plain.len() as u64, None, CancellationToken::new())).unwrap();
        let mut buf = vec![0u8; 4096];
        s.read_at(40 * 1024 * 1024, &mut buf).unwrap();
        assert_eq!(&buf[..], &plain[40 * 1024 * 1024..40 * 1024 * 1024 + 4096]);
        std::thread::sleep(std::time::Duration::from_millis(200));
        // One block plus the read-ahead — not the 64 MiB object.
        let n = mem.1.load(Ordering::SeqCst);
        assert!(n <= 1 + READ_AHEAD as usize, "{n} requests");
        assert!(s.fetched() <= (1 + READ_AHEAD) * BLOCK);
    }
}
