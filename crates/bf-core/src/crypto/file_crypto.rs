//! Streaming object-body encryption (spec §9.1), byte-identical with
//! `lib/core/crypto/file_crypto.dart`.
//!
//! ```text
//! v2 "BFENC2": magic(6) saltLen(u8) salt memKib(u32BE) iters(u16BE) par(u8)
//!              wrapLen(u16BE) wrappedKey noncePrefix(7) chunkSize(u32BE) segments…
//! v1 "BFENC1": magic(6) wrapLen wrappedKey noncePrefix chunkSize segments…
//! segment_i  = AES-GCM(contentKey, prefix‖u32BE(i)‖last, chunk_i, AAD = header) ‖ tag
//! ```
//!
//! Performance: segments are independent, so each batch of up to
//! [`BATCH_SEGMENTS`] chunks is sealed/opened in parallel on rayon, then
//! written in order. Memory stays bounded at ~2 × batch (≈ 8 MiB).
//!
//! Output is written to `<out>.bfpart` and atomically renamed only after every
//! segment authenticated; on any failure the partial file is deleted, so a
//! tampered object never leaves unauthenticated plaintext on disk.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{AeadInOut, Tag};
use base64::Engine;
use rayon::prelude::*;

use super::envelope::{self, AAD_FILE_KEY, Envelope, KEY_LEN, NONCE_LEN, TAG_LEN};
use super::kdf::{Argon2Params, PasswordHasher};
use super::secure_bytes::{SecretBytes, random_bytes, random_secret};
use crate::failures::{AppFailure, AppResult};

pub const MAGIC_V1: &[u8; 6] = b"BFENC1";
pub const MAGIC_V2: &[u8; 6] = b"BFENC2";
pub const NONCE_PREFIX_LEN: usize = 7;
pub const DEFAULT_CHUNK_SIZE: u32 = 64 * 1024;
pub const KDF_SALT_LEN: usize = 16;
/// Ranged-GET size for header-first key resolution (§9.9).
pub const KEY_HEADER_PROBE_BYTES: u64 = 512;
/// Ranged-GET size for the scan's magic probe (§5.4).
pub const MAGIC_PROBE_BYTES: u64 = 128;

const MAX_KDF_MEM_KIB: u32 = 512 * 1024;
const MAX_KDF_ITERATIONS: u32 = 16;
const MAX_KDF_PARALLELISM: u32 = 32;
const MAX_KDF_SALT_LEN: usize = 64;
const MIN_WRAP_LEN: usize = NONCE_LEN + TAG_LEN;
const MAX_WRAP_LEN: usize = 256;
/// Defensive cap on an untrusted chunk size (the format writes 64 KiB).
const MAX_CHUNK_SIZE: u32 = 64 * 1024 * 1024;
const BATCH_SEGMENTS: usize = 64;

pub const LEGACY_FORMAT_MESSAGE: &str = "This object was encrypted before passphrase recovery was \
    added (legacy format) - it can only be opened on the device that encrypted it.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KdfInfo {
    pub salt: Vec<u8>,
    pub params: Argon2Params,
}

impl KdfInfo {
    /// base64 of the salt: the external-key table id (§9.9).
    pub fn salt_id(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.salt)
    }
}

/// The key-bearing part of a header, parsed from ≤ 512 bytes.
#[derive(Debug, Clone)]
pub struct ObjectKeyInfo {
    pub is_v2: bool,
    pub wrapped_key: Vec<u8>,
    pub kdf: Option<KdfInfo>,
}

impl ObjectKeyInfo {
    pub fn kdf_salt_id(&self) -> Option<String> {
        self.kdf.as_ref().map(KdfInfo::salt_id)
    }
}

/// Progress/cancel control: called with `(done, total)` plaintext bytes;
/// returning `false` cancels.
pub type Control<'a> = &'a mut dyn FnMut(u64, u64) -> bool;

pub fn starts_with_magic(head: &[u8]) -> bool {
    head.len() >= 6 && (&head[..6] == MAGIC_V1 || &head[..6] == MAGIC_V2)
}

pub fn is_encrypted_file(path: &Path) -> bool {
    let mut head = [0u8; 6];
    File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && starts_with_magic(&head)
}

// ---------------------------------------------------------------- header --

struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> AppResult<&'a [u8]> {
        if self.at + n > self.b.len() {
            return Err(AppFailure::crypto("short header"));
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> AppResult<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> AppResult<u16> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> AppResult<u32> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
}

fn take_magic(c: &mut Cursor<'_>) -> AppResult<bool> {
    let m = c.take(6)?;
    if m == MAGIC_V2 {
        Ok(true)
    } else if m == MAGIC_V1 {
        Ok(false)
    } else {
        Err(AppFailure::crypto("not an encrypted object"))
    }
}

/// Untrusted-header bounds are checked before any allocation or KDF.
fn take_kdf(c: &mut Cursor<'_>) -> AppResult<KdfInfo> {
    let salt_len = c.u8()? as usize;
    if salt_len == 0 || salt_len > MAX_KDF_SALT_LEN {
        return Err(AppFailure::crypto("corrupt KDF header"));
    }
    let salt = c.take(salt_len)?.to_vec();
    let mem = c.u32()?;
    let iters = c.u16()? as u32;
    let par = c.u8()? as u32;
    if mem == 0
        || mem > MAX_KDF_MEM_KIB
        || iters == 0
        || iters > MAX_KDF_ITERATIONS
        || par == 0
        || par > MAX_KDF_PARALLELISM
    {
        return Err(AppFailure::crypto("corrupt KDF header"));
    }
    Ok(KdfInfo {
        salt,
        params: Argon2Params { memory_kib: mem, iterations: iters, parallelism: par, hash_length: 32 },
    })
}

fn take_wrap(c: &mut Cursor<'_>) -> AppResult<Vec<u8>> {
    let len = c.u16()? as usize;
    if !(MIN_WRAP_LEN..=MAX_WRAP_LEN).contains(&len) {
        return Err(AppFailure::crypto("corrupt key header"));
    }
    Ok(c.take(len)?.to_vec())
}

/// Parse only the key-bearing header from the first ≤ 512 bytes.
pub fn read_key_info_from_bytes(head: &[u8]) -> AppResult<ObjectKeyInfo> {
    let mut c = Cursor { b: head, at: 0 };
    let is_v2 = take_magic(&mut c)?;
    let kdf = if is_v2 { Some(take_kdf(&mut c)?) } else { None };
    let wrapped_key = take_wrap(&mut c)?;
    Ok(ObjectKeyInfo { is_v2, wrapped_key, kdf })
}

pub fn read_key_info(path: &Path) -> AppResult<ObjectKeyInfo> {
    let f = File::open(path).map_err(|e| AppFailure::io("open", e))?;
    let mut head = Vec::with_capacity(KEY_HEADER_PROBE_BYTES as usize);
    f.take(KEY_HEADER_PROBE_BYTES).read_to_end(&mut head).map_err(|e| AppFailure::io("read", e))?;
    read_key_info_from_bytes(&head)
}

/// One GCM open of ~60 B: a definitive "is this my key".
pub fn opens_with_info(info: &ObjectKeyInfo, master_key: &[u8]) -> bool {
    master_key.len() == KEY_LEN && Envelope::open(&info.wrapped_key, master_key, AAD_FILE_KEY).is_ok()
}

/// Derive the master key from a passphrase and the header's salt/params, and
/// confirm it opens this object. The one deliberate non-uniform failure:
/// "that passphrase does not open this object" (computable offline anyway).
pub fn derive_key_for_info(info: &ObjectKeyInfo, passphrase: &[u8]) -> AppResult<SecretBytes> {
    let Some(kdf) = &info.kdf else {
        return Err(AppFailure::crypto(LEGACY_FORMAT_MESSAGE));
    };
    let candidate = PasswordHasher::derive_key(passphrase, &kdf.salt, kdf.params)?;
    if !opens_with_info(info, &candidate) {
        return Err(AppFailure::crypto("that passphrase does not open this object"));
    }
    Ok(candidate)
}

fn build_header(wrapped: &[u8], nonce_prefix: &[u8], chunk_size: u32, kdf: Option<&KdfInfo>) -> Vec<u8> {
    let mut b = Vec::with_capacity(64 + wrapped.len());
    match kdf {
        None => b.extend_from_slice(MAGIC_V1),
        Some(k) => {
            b.extend_from_slice(MAGIC_V2);
            b.push(k.salt.len() as u8);
            b.extend_from_slice(&k.salt);
            b.extend_from_slice(&k.params.memory_kib.to_be_bytes());
            b.extend_from_slice(&(k.params.iterations as u16).to_be_bytes());
            b.push(k.params.parallelism as u8);
        }
    }
    b.extend_from_slice(&(wrapped.len() as u16).to_be_bytes());
    b.extend_from_slice(wrapped);
    b.extend_from_slice(nonce_prefix);
    b.extend_from_slice(&chunk_size.to_be_bytes());
    b
}

fn segment_nonce(prefix: &[u8], index: u64, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..7].copy_from_slice(prefix);
    n[7..11].copy_from_slice(&(index as u32).to_be_bytes());
    n[11] = u8::from(last);
    n
}

// ------------------------------------------------------------- file I/O --

fn part_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_os_string();
    s.push(".bfpart");
    PathBuf::from(s)
}

/// Writes to a sibling `.bfpart` and renames on success; deletes on failure.
fn with_atomic_output(out: &Path, body: impl FnOnce(&mut BufWriter<File>) -> AppResult<()>) -> AppResult<()> {
    let tmp = part_path(out);
    let result = (|| {
        let f = File::create(&tmp).map_err(|e| AppFailure::io("create output", e))?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        body(&mut w)?;
        let f = w.into_inner().map_err(|e| AppFailure::io("flush", e.into_error()))?;
        f.sync_all().map_err(|e| AppFailure::io("sync", e))?;
        drop(f);
        fs::rename(&tmp, out).map_err(|e| AppFailure::io("rename output", e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> AppResult<()> {
    r.read_exact(buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            AppFailure::crypto("unexpected end of object")
        } else {
            AppFailure::io("read", e)
        }
    })
}

// --------------------------------------------------------------- encrypt --

/// Encrypt `input` → `output` under `master_key`. With `kdf` the output is v2
/// (self-recoverable by passphrase); without, legacy v1 (only for epochs that
/// predate v2, §9.7).
pub fn encrypt_file(
    input: &Path,
    output: &Path,
    master_key: &[u8],
    chunk_size: u32,
    kdf: Option<&KdfInfo>,
    ctl: Control<'_>,
) -> AppResult<()> {
    if master_key.len() != KEY_LEN {
        return Err(AppFailure::crypto("master key must be 32 bytes"));
    }
    if chunk_size == 0 || chunk_size > MAX_CHUNK_SIZE {
        return Err(AppFailure::crypto("bad chunk size"));
    }
    let content_key = random_secret(KEY_LEN);
    let wrapped = Envelope::seal(&content_key, master_key, AAD_FILE_KEY)?;
    let prefix = random_bytes(NONCE_PREFIX_LEN);
    let header = build_header(&wrapped, &prefix, chunk_size, kdf);
    let cipher = envelope::cipher(&content_key)?;

    let f = File::open(input).map_err(|e| AppFailure::io("open input", e))?;
    let total = f.metadata().map_err(|e| AppFailure::io("stat input", e))?.len();
    let mut reader = BufReader::with_capacity(1 << 20, f);
    let chunk = chunk_size as u64;
    let segs = if total == 0 { 1 } else { total.div_ceil(chunk) };

    with_atomic_output(output, |w| {
        w.write_all(&header).map_err(|e| AppFailure::io("write", e))?;
        let mut buf = vec![0u8; (chunk as usize) * BATCH_SEGMENTS];
        let mut seg = 0u64;
        let mut done = 0u64;
        while seg < segs {
            if !ctl(done, total) {
                return Err(AppFailure::Cancelled);
            }
            let n_segs = (segs - seg).min(BATCH_SEGMENTS as u64);
            let bytes = ((total - done).min(n_segs * chunk)) as usize;
            read_full(&mut reader, &mut buf[..bytes])?;
            let base = seg;
            let sealed: Vec<AppResult<Vec<u8>>> = (0..n_segs as usize)
                .into_par_iter()
                .map(|j| {
                    let start = j * chunk as usize;
                    let end = (start + chunk as usize).min(bytes);
                    let idx = base + j as u64;
                    let nonce = segment_nonce(&prefix, idx, idx == segs - 1);
                    let mut out = Vec::with_capacity(end - start + TAG_LEN);
                    out.extend_from_slice(&buf[start..end]);
                    let tag = cipher
                        .encrypt_inout_detached(
                            &envelope::nonce_from(&nonce)?,
                            &header,
                            out.as_mut_slice().into(),
                        )
                        .map_err(|_| AppFailure::crypto("encrypt failed"))?;
                    out.extend_from_slice(&tag);
                    Ok(out)
                })
                .collect();
            for s in sealed {
                w.write_all(&s?).map_err(|e| AppFailure::io("write", e))?;
            }
            done += bytes as u64;
            seg += n_segs;
        }
        // Zero the plaintext staging buffer before it is freed.
        zeroize::Zeroize::zeroize(&mut buf);
        ctl(total, total);
        Ok(())
    })
}

// --------------------------------------------------------------- decrypt --

pub fn decrypt_file(input: &Path, output: &Path, master_key: &[u8], ctl: Control<'_>) -> AppResult<()> {
    if master_key.len() != KEY_LEN {
        return Err(AppFailure::crypto("master key must be 32 bytes"));
    }
    let f = File::open(input).map_err(|e| AppFailure::io("open input", e))?;
    let file_len = f.metadata().map_err(|e| AppFailure::io("stat input", e))?.len();
    let mut reader = BufReader::with_capacity(1 << 20, f);

    // Read the header field by field (bounded), keeping the exact bytes: the
    // whole header is the AAD of every segment.
    let mut header = Vec::with_capacity(128);
    let grab = |r: &mut BufReader<File>, n: usize, h: &mut Vec<u8>| -> AppResult<()> {
        let start = h.len();
        h.resize(start + n, 0);
        read_full(r, &mut h[start..])
    };
    grab(&mut reader, 6, &mut header)?;
    let is_v2 = take_magic(&mut Cursor { b: &header, at: 0 })?;
    if is_v2 {
        grab(&mut reader, 1, &mut header)?;
        let salt_len = header[6] as usize;
        if salt_len == 0 || salt_len > MAX_KDF_SALT_LEN {
            return Err(AppFailure::crypto("corrupt KDF header"));
        }
        grab(&mut reader, salt_len + 7, &mut header)?;
        take_kdf(&mut Cursor { b: &header[6..], at: 0 })?;
    }
    let wrap_at = header.len();
    grab(&mut reader, 2, &mut header)?;
    let wrap_len = u16::from_be_bytes([header[wrap_at], header[wrap_at + 1]]) as usize;
    if !(MIN_WRAP_LEN..=MAX_WRAP_LEN).contains(&wrap_len) {
        return Err(AppFailure::crypto("corrupt key header"));
    }
    grab(&mut reader, wrap_len + NONCE_PREFIX_LEN + 4, &mut header)?;
    let wrapped = header[wrap_at + 2..wrap_at + 2 + wrap_len].to_vec();
    let p_at = wrap_at + 2 + wrap_len;
    let prefix = header[p_at..p_at + NONCE_PREFIX_LEN].to_vec();
    let cs = &header[p_at + NONCE_PREFIX_LEN..];
    let chunk_size = u32::from_be_bytes([cs[0], cs[1], cs[2], cs[3]]);
    if chunk_size == 0 || chunk_size > MAX_CHUNK_SIZE {
        return Err(AppFailure::crypto("bad chunk size"));
    }

    let content_key = Envelope::open(&wrapped, master_key, AAD_FILE_KEY)
        .map_err(|_| AppFailure::crypto("wrong key"))?;
    let cipher = envelope::cipher(&content_key)?;

    let header_len = header.len() as u64;
    let body_len = file_len.saturating_sub(header_len);
    if file_len < header_len || body_len < TAG_LEN as u64 {
        return Err(AppFailure::crypto("truncated object"));
    }
    let seg_full = chunk_size as u64 + TAG_LEN as u64;
    let full = body_len / seg_full;
    let rem = body_len % seg_full;
    let total_segs = if rem > 0 { full + 1 } else { full };
    let last_len = if rem > 0 { rem } else { seg_full };
    if last_len < TAG_LEN as u64 {
        return Err(AppFailure::crypto("corrupt segment"));
    }
    let plain_total = body_len - total_segs * TAG_LEN as u64;

    with_atomic_output(output, |w| {
        let mut buf = vec![0u8; seg_full as usize * BATCH_SEGMENTS];
        let mut seg = 0u64;
        let mut done = 0u64;
        while seg < total_segs {
            if !ctl(done, plain_total) {
                return Err(AppFailure::Cancelled);
            }
            let n_segs = (total_segs - seg).min(BATCH_SEGMENTS as u64);
            let includes_last = seg + n_segs == total_segs;
            let bytes = if includes_last {
                (n_segs - 1) * seg_full + last_len
            } else {
                n_segs * seg_full
            } as usize;
            read_full(&mut reader, &mut buf[..bytes])?;
            let base = seg;
            let opened: Vec<AppResult<Vec<u8>>> = (0..n_segs as usize)
                .into_par_iter()
                .map(|j| {
                    let start = j * seg_full as usize;
                    let end = (start + seg_full as usize).min(bytes);
                    let idx = base + j as u64;
                    let nonce = segment_nonce(&prefix, idx, idx == total_segs - 1);
                    let ct_end = end - TAG_LEN;
                    let mut out = buf[start..ct_end].to_vec();
                    let tag = Tag::<aes_gcm::Aes256Gcm>::try_from(&buf[ct_end..end])
                        .map_err(|_| AppFailure::crypto("authentication failed"))?;
                    cipher
                        .decrypt_inout_detached(
                            &envelope::nonce_from(&nonce)?,
                            &header,
                            out.as_mut_slice().into(),
                            &tag,
                        )
                        .map_err(|_| AppFailure::crypto("authentication failed"))?;
                    Ok(out)
                })
                .collect();
            for o in opened {
                let mut o = o?;
                w.write_all(&o).map_err(|e| AppFailure::io("write", e))?;
                done += o.len() as u64;
                zeroize::Zeroize::zeroize(&mut o);
            }
            seg += n_segs;
        }
        ctl(plain_total, plain_total);
        Ok(())
    })
}

// ------------------------------------------------------------ passphrase --

/// One-shot v2 encrypt with a fresh salt (the CLI path).
pub fn encrypt_file_with_passphrase(
    input: &Path,
    output: &Path,
    passphrase: &[u8],
    params: Argon2Params,
    ctl: Control<'_>,
) -> AppResult<()> {
    let salt = random_bytes(KDF_SALT_LEN);
    let key = PasswordHasher::derive_key(passphrase, &salt, params)?;
    let kdf = KdfInfo { salt, params };
    encrypt_file(input, output, &key, DEFAULT_CHUNK_SIZE, Some(&kdf), ctl)
}

pub fn decrypt_file_with_passphrase(
    input: &Path,
    output: &Path,
    passphrase: &[u8],
    ctl: Control<'_>,
) -> AppResult<()> {
    let info = read_key_info(input)?;
    let Some(kdf) = info.kdf else {
        return Err(AppFailure::crypto(LEGACY_FORMAT_MESSAGE));
    };
    let key = PasswordHasher::derive_key(passphrase, &kdf.salt, kdf.params)?;
    decrypt_file(input, output, &key, ctl)
}

pub fn no_control() -> impl FnMut(u64, u64) -> bool {
    |_, _| true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};

    fn write(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, data).unwrap();
        p
    }

    fn kdf() -> KdfInfo {
        KdfInfo { salt: vec![7u8; 16], params: Argon2Params::test_weak() }
    }

    fn round_trip(len: usize, chunk: u32, v2: bool) {
        let d = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let src = write(d.path(), "src", &data);
        let enc = d.path().join("enc");
        let dec = d.path().join("dec");
        let key = random_bytes(32);
        let k = kdf();
        encrypt_file(&src, &enc, &key, chunk, v2.then_some(&k), &mut no_control()).unwrap();
        assert!(is_encrypted_file(&enc));
        decrypt_file(&enc, &dec, &key, &mut no_control()).unwrap();
        assert_eq!(fs::read(&dec).unwrap(), data);
        let info = read_key_info(&enc).unwrap();
        assert_eq!(info.is_v2, v2);
        assert!(opens_with_info(&info, &key));
        assert!(!opens_with_info(&info, &random_bytes(32)));
        // Size: header + plaintext + one tag per segment.
        let segs = if len == 0 { 1 } else { len.div_ceil(chunk as usize) };
        let enc_len = fs::metadata(&enc).unwrap().len() as usize;
        let header_len = 6 + if v2 { 1 + 16 + 7 } else { 0 } + 2 + 60 + 7 + 4;
        assert_eq!(enc_len, header_len + len + segs * 16);
    }

    #[test]
    fn round_trips() {
        for &(len, chunk) in &[(0, 64), (1, 64), (63, 64), (64, 64), (65, 64), (64 * 200 + 3, 64), (300_000, 65_536)] {
            round_trip(len, chunk, true);
            round_trip(len, chunk, false);
        }
    }

    #[test]
    fn tamper_truncate_reorder_fail_identically() {
        let d = tempfile::tempdir().unwrap();
        let data = vec![9u8; 64 * 5];
        let src = write(d.path(), "src", &data);
        let enc = d.path().join("enc");
        let key = random_bytes(32);
        encrypt_file(&src, &enc, &key, 64, Some(&kdf()), &mut no_control()).unwrap();
        let bytes = fs::read(&enc).unwrap();
        let header_len = bytes.len() - (64 + 16) * 5;
        let out = d.path().join("out");

        // Body bit flip.
        let mut t = bytes.clone();
        t[header_len + 3] ^= 1;
        let p = write(d.path(), "t1", &t);
        let e1 = decrypt_file(&p, &out, &key, &mut no_control()).unwrap_err();
        assert!(!out.exists(), "no plaintext left behind");
        // Truncation: drop the true last segment.
        let p = write(d.path(), "t2", &bytes[..bytes.len() - 80]);
        let e2 = decrypt_file(&p, &out, &key, &mut no_control()).unwrap_err();
        // Reorder: swap segments 0 and 1.
        let mut t = bytes.clone();
        let (a, b) = (header_len, header_len + 80);
        let seg0 = t[a..a + 80].to_vec();
        let seg1 = t[b..b + 80].to_vec();
        t[a..a + 80].copy_from_slice(&seg1);
        t[b..b + 80].copy_from_slice(&seg0);
        let p = write(d.path(), "t3", &t);
        let e3 = decrypt_file(&p, &out, &key, &mut no_control()).unwrap_err();
        // Header tamper (salt byte) breaks every segment's AAD.
        let mut t = bytes.clone();
        t[8] ^= 1;
        let p = write(d.path(), "t4", &t);
        let e4 = decrypt_file(&p, &out, &key, &mut no_control()).unwrap_err();
        assert_eq!(e1, e2);
        assert_eq!(e2, e3);
        assert_eq!(e3, e4);
        // Wrong key.
        let p = write(d.path(), "ok", &bytes);
        let e5 = decrypt_file(&p, &out, &random_bytes(32), &mut no_control()).unwrap_err();
        assert!(matches!(e5, AppFailure::Crypto { .. }));
    }

    #[test]
    fn hostile_header_bounds() {
        let mut h = MAGIC_V2.to_vec();
        h.push(16);
        h.extend_from_slice(&[0u8; 16]);
        h.extend_from_slice(&(u32::MAX).to_be_bytes()); // absurd memory
        h.extend_from_slice(&3u16.to_be_bytes());
        h.push(4);
        assert!(read_key_info_from_bytes(&h).is_err());
        assert!(read_key_info_from_bytes(b"NOTENC").is_err());
        assert!(read_key_info_from_bytes(b"BF").is_err());
    }

    #[test]
    fn passphrase_round_trip_and_cancel() {
        let d = tempfile::tempdir().unwrap();
        let src = write(d.path(), "src", b"secret contents");
        let enc = d.path().join("enc");
        let dec = d.path().join("dec");
        encrypt_file_with_passphrase(&src, &enc, b"correct horse", Argon2Params::test_weak(), &mut no_control()).unwrap();
        decrypt_file_with_passphrase(&enc, &dec, b"correct horse", &mut no_control()).unwrap();
        assert_eq!(fs::read(&dec).unwrap(), b"secret contents");
        let info = read_key_info(&enc).unwrap();
        assert!(derive_key_for_info(&info, b"correct horse").is_ok());
        assert!(derive_key_for_info(&info, b"wrong").is_err());

        let out = d.path().join("cancelled");
        let r = encrypt_file(&src, &out, &random_bytes(32), 64, None, &mut |_, _| false);
        assert_eq!(r.unwrap_err(), AppFailure::Cancelled);
        assert!(!out.exists() && !part_path(&out).exists());
        let _ = File::open(&enc).unwrap().seek(SeekFrom::Start(0));
    }
}
