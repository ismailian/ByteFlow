//! Thumbnails (spec §10.2–§10.4).
//!
//! **Hard security boundary**: [`thumb_ineligible_reason`] is the single
//! predicate both generation and display route through, fed from the
//! encryption engine's index (never the cache column). It is re-checked on
//! every read and again immediately before every write, so an object
//! encrypted mid-generation never gets a thumbnail on disk.
//!
//! Generation is never speculative: only from bytes already in hand (an
//! upload's source, a finished download, a decoded preview). JPEGs decode
//! with DCT downscaling, so a 48 MP photo never becomes a full-size bitmap.

use std::path::Path;
use std::sync::Arc;

use image::{DynamicImage, ImageReader, RgbImage};
use tokio_util::sync::CancellationToken;

use bf_core::media::{MediaKind, media_kind_for_key};
use bf_core::{AppFailure, AppResult};
use bf_data::local::ThumbnailStore;

use crate::SharedNotifier;
use crate::encryption::EncryptionEngine;

pub const THUMB_MAX_EDGE: u32 = 144;
pub const THUMB_QUALITY: u8 = 80;
/// Refuse absurd inputs outright (decompression bombs).
const MAX_DECODE_PIXELS: u64 = 120_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligible {
    Folder,
    Encrypted,
    EncryptionInFlight,
    NotMedia,
}

/// `folder` → `encrypted` → `encryptionInFlight` → `notMedia`.
pub fn thumb_ineligible_reason(key: &str, is_encrypted: bool, is_locked: bool) -> Option<Ineligible> {
    if key.ends_with('/') {
        Some(Ineligible::Folder)
    } else if is_encrypted {
        Some(Ineligible::Encrypted)
    } else if is_locked {
        Some(Ineligible::EncryptionInFlight)
    } else if !matches!(media_kind_for_key(key), MediaKind::Image) {
        // Video frames need a platform extractor; icon fallback is a
        // supported, safe outcome (§10.5, §17.5).
        Some(Ineligible::NotMedia)
    } else {
        None
    }
}

pub struct ThumbnailService {
    store: Arc<ThumbnailStore>,
    encryption: Arc<EncryptionEngine>,
    notifier: SharedNotifier,
    cancel: CancellationToken,
}

impl ThumbnailService {
    pub fn new(store: Arc<ThumbnailStore>, encryption: Arc<EncryptionEngine>, notifier: SharedNotifier, cancel: CancellationToken) -> Arc<Self> {
        Arc::new(Self { store, encryption, notifier, cancel })
    }

    fn eligible(&self, key: &str) -> bool {
        thumb_ineligible_reason(key, self.encryption.is_encrypted(key), self.encryption.is_locked(key)).is_none()
    }

    /// Display path: an in-memory set lookup; never a network request.
    pub fn has(&self, key: &str) -> bool {
        self.eligible(key) && self.store.has(key)
    }

    pub fn read(&self, key: &str) -> Option<Vec<u8>> {
        if !self.eligible(key) {
            return None;
        }
        self.store.read(key)
    }

    /// From a local plaintext file (upload source / finished download).
    pub fn generate_from_file(self: &Arc<Self>, key: &str, path: &Path) {
        // Checked before *any* decode work.
        if !self.eligible(key) || self.store.has(key) {
            return;
        }
        let (me, key, path) = (self.clone(), key.to_string(), path.to_path_buf());
        tokio::spawn(async move {
            let p = path.clone();
            let jpeg = crate::blocking(move || make_thumbnail_from_path(&p)).await;
            me.store_checked(&key, jpeg);
        });
    }

    /// From an image already decoded for the preview (zero extra bytes).
    pub fn generate_from_image(self: &Arc<Self>, key: &str, img: Arc<DynamicImage>) {
        if !self.eligible(key) || self.store.has(key) {
            return;
        }
        let (me, key) = (self.clone(), key.to_string());
        tokio::spawn(async move {
            let jpeg = crate::blocking(move || encode_thumb(&img)).await;
            me.store_checked(&key, jpeg);
        });
    }

    fn store_checked(&self, key: &str, jpeg: AppResult<Vec<u8>>) {
        // Re-check immediately before the write (encrypted-mid-generation race).
        if let Ok(j) = jpeg
            && self.eligible(key)
            && self.store.put(key, &j).is_ok()
        {
            self.notifier.changed();
        }
    }

    pub fn on_renamed_or_moved(&self, from: &str) {
        self.store.remove(from);
    }
    pub fn remove(&self, key: &str) {
        self.store.remove(key);
    }
    pub fn retire(&self, keys: &[String]) {
        for k in keys {
            self.store.remove(k);
        }
    }

    /// Anything that becomes encrypted loses its cover. Sweeps the whole
    /// index (idempotent, so a missed event can't strand a thumbnail).
    pub fn subscribe_to_encryption(self: &Arc<Self>) {
        let mut rx = self.encryption.subscribe();
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = me.cancel.cancelled() => return,
                    r = rx.changed() => if r.is_err() { return },
                }
                for k in me.encryption.encrypted_keys().into_iter().chain(me.encryption.locked_keys()) {
                    if me.store.has(&k) {
                        me.store.remove(&k);
                    }
                }
            }
        });
    }
}

fn encode_thumb(img: &DynamicImage) -> AppResult<Vec<u8>> {
    let t = img.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE).to_rgb8();
    encode_jpeg(&t)
}

fn encode_jpeg(img: &RgbImage) -> AppResult<Vec<u8>> {
    let mut out = Vec::with_capacity(8 * 1024);
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, THUMB_QUALITY);
    img.write_with_encoder(enc).map_err(|_| AppFailure::unknown("thumbnail encode"))?;
    Ok(out)
}

/// Decode with downsampling where the codec allows (JPEG: DCT scaling to
/// ≥ the target, so memory is ~1/64 of full size for large photos).
pub fn decode_downscaled(path: &Path, max_edge: u32) -> AppResult<DynamicImage> {
    let ext = bf_core::media::extension(&path.to_string_lossy());
    if ext == "jpg" || ext == "jpeg" {
        let f = std::fs::File::open(path).map_err(|e| AppFailure::io("open image", e))?;
        let mut d = jpeg_decoder::Decoder::new(std::io::BufReader::new(f));
        d.read_info().map_err(|_| AppFailure::unknown("jpeg header"))?;
        let edge = max_edge.min(u16::MAX as u32) as u16;
        let (w, h) = d.scale(edge, edge).map_err(|_| AppFailure::unknown("jpeg scale"))?;
        let px = d.decode().map_err(|_| AppFailure::unknown("jpeg decode"))?;
        let info = d.info().ok_or_else(|| AppFailure::unknown("jpeg info"))?;
        let (w, h) = (w as u32, h as u32);
        let img = match info.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => image::RgbImage::from_raw(w, h, px).map(DynamicImage::ImageRgb8),
            jpeg_decoder::PixelFormat::L8 => image::GrayImage::from_raw(w, h, px).map(DynamicImage::ImageLuma8),
            _ => None,
        };
        if let Some(i) = img {
            return Ok(apply_exif_orientation(path, i));
        }
    }
    let mut r = ImageReader::open(path).map_err(|e| AppFailure::io("open image", e))?.with_guessed_format().map_err(|e| AppFailure::io("read image", e))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_PIXELS * 4);
    r.limits(limits);
    let (w, h) = r.into_dimensions().map_err(|_| AppFailure::unknown("image header"))?;
    if (w as u64) * (h as u64) > MAX_DECODE_PIXELS {
        return Err(AppFailure::unknown("image too large"));
    }
    let img = ImageReader::open(path)
        .map_err(|e| AppFailure::io("open image", e))?
        .with_guessed_format()
        .map_err(|e| AppFailure::io("read image", e))?
        .decode()
        .map_err(|_| AppFailure::unknown("image decode"))?;
    let img = if img.width() > max_edge * 2 || img.height() > max_edge * 2 { img.thumbnail(max_edge * 2, max_edge * 2) } else { img };
    Ok(img)
}

/// Minimal EXIF orientation handling for JPEGs (tags 3/6/8).
fn apply_exif_orientation(path: &Path, img: DynamicImage) -> DynamicImage {
    match exif_orientation(path) {
        Some(3) => img.rotate180(),
        Some(6) => img.rotate90(),
        Some(8) => img.rotate270(),
        _ => img,
    }
}

fn exif_orientation(path: &Path) -> Option<u16> {
    use std::io::Read;
    let mut buf = vec![0u8; 64 * 1024];
    let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
    let b = &buf[..n];
    let pos = b.windows(6).position(|w| w == b"Exif\0\0")? + 6;
    let t = b.get(pos..)?;
    let le = t.get(0..2)? == b"II";
    let u16_at = |o: usize| -> Option<u16> {
        let s = t.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let s = t.get(o..o + 4)?;
        Some(if le { u32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { u32::from_be_bytes([s[0], s[1], s[2], s[3]]) })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    for i in 0..count.min(64) {
        let e = ifd + 2 + i * 12;
        if u16_at(e)? == 0x0112 {
            return u16_at(e + 8);
        }
    }
    None
}

pub fn make_thumbnail_from_path(path: &Path) -> AppResult<Vec<u8>> {
    let img = decode_downscaled(path, THUMB_MAX_EDGE)?;
    encode_thumb(&img)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn predicate_order() {
        assert_eq!(thumb_ineligible_reason("a/", true, true), Some(Ineligible::Folder));
        assert_eq!(thumb_ineligible_reason("a.jpg", true, true), Some(Ineligible::Encrypted));
        assert_eq!(thumb_ineligible_reason("a.jpg", false, true), Some(Ineligible::EncryptionInFlight));
        assert_eq!(thumb_ineligible_reason("a.txt", false, false), Some(Ineligible::NotMedia));
        assert_eq!(thumb_ineligible_reason("a.jpg", false, false), None);
    }

    #[test]
    fn thumbnail_from_png() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big.png");
        image::RgbImage::from_pixel(1000, 500, image::Rgb([10, 200, 30])).save(&p).unwrap();
        let jpeg = make_thumbnail_from_path(&p).unwrap();
        let back = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (144, 72));
    }

    #[test]
    fn thumbnail_from_jpeg_downscaled() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big.jpg");
        image::RgbImage::from_pixel(2000, 1000, image::Rgb([200, 10, 30])).save(&p).unwrap();
        let jpeg = make_thumbnail_from_path(&p).unwrap();
        let back = image::load_from_memory(&jpeg).unwrap();
        assert!(back.width() <= 144 && back.height() <= 144);
    }
}
