//! dev-env builds only: `BYTEFLOW_DEV_DEMO=1` fills a throwaway vault
//! (use it together with `BYTEFLOW_DEV_DATA_DIR`) with a made-up bucket
//! listing, thumbnails, transfers, sync pairs and share links, then unlocks
//! it on start. Nothing here talks to the network; it exists so the website
//! screenshots show a populated app. Never compiled into shipped builds.
//!
//! Optional: `BYTEFLOW_DEV_PAGE` (files|transfers|encryption|sync|links|settings),
//! `BYTEFLOW_DEV_PREFIX` (folder to open), `BYTEFLOW_DEV_THEME` (light|dark),
//! `BYTEFLOW_DEV_GRID` (1 = grid view).

use std::sync::Arc;

use bf_app::core::Core;
use bf_app::encryption::state::EncryptionState;
use bf_data::local::app_db::{SharedLink, SyncPair, TransferKind, TransferRow, TransferStatus};
use bf_data::local::cache_db::CacheEntry;
use bf_data::r2::R2Config;
use image::{ImageEncoder, Rgb, RgbImage};

pub const PASSWORD: &str = "Demo-Vault-Password-2026!";
const DESTRUCTION: &str = "Demo-Destroy-Everything-2026?";

pub fn enabled() -> bool {
    std::env::var_os("BYTEFLOW_DEV_DEMO").is_some()
}

pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

const DAY: i64 = 86_400_000;
const MB: u64 = 1024 * 1024;

/// (key, size, days ago, encrypted)
const FILES: &[(&str, u64, i64, bool)] = &[
    ("Photos/2026 Lisbon/alfama-sunrise.jpg", 6 * MB + 412_000, 12, false),
    ("Photos/2026 Lisbon/tram-28.jpg", 5 * MB + 90_000, 12, false),
    ("Photos/2026 Lisbon/belem-tower.jpg", 7 * MB + 230_000, 11, false),
    ("Photos/2026 Lisbon/pasteis.jpg", 3 * MB + 800_000, 11, false),
    ("Photos/2026 Lisbon/miradouro-dusk.jpg", 6 * MB + 10_000, 10, false),
    ("Photos/2026 Lisbon/coast-cascais.jpg", 8 * MB + 515_000, 10, false),
    ("Photos/2026 Lisbon/rooftops.jpg", 4 * MB + 330_000, 9, false),
    ("Photos/2026 Lisbon/ocean-blue.jpg", 5 * MB + 720_000, 9, false),
    ("Photos/2026 Lisbon/sintra-forest.jpg", 9 * MB + 120_000, 8, false),
    ("Photos/2026 Lisbon/night-market.jpg", 4 * MB + 940_000, 8, false),
    ("Photos/2026 Lisbon/lisbon-trip.mp4", 412 * MB, 8, false),
    ("Photos/2026 Lisbon/passport-scan.jpg", 1 * MB + 220_000, 13, true),
    ("Photos/Family/birthday-cake.jpg", 3 * MB + 100_000, 40, false),
    ("Photos/Family/garden.jpg", 4 * MB + 600_000, 52, false),
    ("Photos/Family/first-snow.jpg", 5 * MB + 300_000, 260, false),
    ("Photos/cover-autumn.jpg", 2 * MB + 900_000, 30, false),
    ("Documents/Taxes 2025/return-2025.pdf", 1 * MB + 310_000, 90, true),
    ("Documents/Taxes 2025/receipts.zip", 48 * MB, 92, true),
    ("Documents/Taxes 2025/notes.txt", 4_200, 91, false),
    ("Documents/Contracts/apartment-lease.pdf", 2 * MB + 40_000, 150, true),
    ("Documents/Contracts/employment-agreement.pdf", 880_000, 400, true),
    ("Documents/budget-2026.csv", 38_000, 3, false),
    ("Documents/project-roadmap.md", 12_400, 1, false),
    ("Documents/meeting-notes.txt", 6_100, 0, false),
    ("Backups/laptop/2026-11-01.tar.zst", 18 * 1024 * MB, 17, true),
    ("Backups/laptop/2026-11-15.tar.zst", 18 * 1024 * MB + 600 * MB, 3, true),
    ("Backups/phone/contacts.vcf", 220_000, 6, true),
    ("Backups/phone/photos-oct.zip", 2 * 1024 * MB + 300 * MB, 25, false),
    ("Music/field-recordings/rain-on-roof.flac", 62 * MB, 70, false),
    ("Music/field-recordings/harbour.wav", 88 * MB, 71, false),
    ("Music/demo-mix.mp3", 9 * MB + 400_000, 20, false),
    ("Videos/drone-coastline.mp4", 1_350 * MB, 33, false),
    ("Videos/talk-recording.mkv", 2_800 * MB, 61, false),
    ("Design/logo-final.svg", 18_000, 14, false),
    ("Design/mockup-home.png", 2 * MB + 210_000, 14, false),
    ("Design/palette.png", 340_000, 15, false),
    ("README.md", 2_400, 2, false),
    ("vault-keys.kdbx", 96_000, 5, true),
];

/// Thumbnail palettes: (sky top, sky bottom, ground, sun).
fn palette(i: usize) -> [[u8; 3]; 4] {
    const P: &[[[u8; 3]; 4]] = &[
        [[255, 179, 120], [255, 222, 170], [120, 92, 140], [255, 245, 210]],
        [[70, 130, 200], [170, 210, 240], [210, 170, 110], [255, 250, 220]],
        [[40, 60, 120], [240, 140, 110], [50, 40, 70], [255, 200, 120]],
        [[110, 190, 230], [220, 240, 250], [60, 130, 90], [255, 255, 235]],
        [[25, 30, 60], [90, 80, 150], [20, 20, 35], [250, 230, 170]],
        [[0, 110, 170], [90, 200, 220], [230, 210, 160], [255, 250, 230]],
        [[200, 120, 90], [250, 200, 150], [150, 70, 60], [255, 240, 200]],
        [[30, 90, 70], [140, 200, 150], [40, 70, 45], [240, 250, 220]],
    ];
    P[i % P.len()]
}

fn lerp(a: [u8; 3], b: [u8; 3], t: f32) -> Rgb<u8> {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgb([f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])])
}

/// A small landscape-style picture so the grid looks like a photo folder.
fn thumbnail(i: usize) -> Vec<u8> {
    let (w, h) = if i % 5 == 3 { (108u32, 144u32) } else { (144u32, 108u32) };
    let [top, bottom, ground, sun] = palette(i);
    let horizon = h as f32 * (0.58 + (i % 3) as f32 * 0.06);
    let (sx, sy, sr) = (w as f32 * (0.25 + (i % 4) as f32 * 0.15), horizon - h as f32 * 0.18, 9.0 + (i % 3) as f32 * 4.0);
    let phase = i as f32 * 1.7;
    let img = RgbImage::from_fn(w, h, |x, y| {
        let (xf, yf) = (x as f32, y as f32);
        let hill = horizon + (xf / w as f32 * 6.0 + phase).sin() * h as f32 * 0.06 + (xf / w as f32 * 13.0 + phase).sin() * h as f32 * 0.025;
        if yf > hill {
            let t = ((yf - hill) / (h as f32 - hill + 1.0)).clamp(0.0, 1.0);
            lerp(ground, [ground[0] / 2, ground[1] / 2, ground[2] / 2], t)
        } else if ((xf - sx).powi(2) + (yf - sy).powi(2)).sqrt() < sr {
            Rgb(sun)
        } else {
            lerp(top, bottom, (yf / hill).clamp(0.0, 1.0))
        }
    });
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .expect("jpeg encode");
    out
}

/// Seed once: an already configured vault is left as it is.
pub async fn seed(core: &Arc<Core>) {
    if core.credentials.is_configured().unwrap_or(false) {
        apply_prefs(core);
        return;
    }
    let cfg = R2Config {
        account_id: "3f9c2a7b5d1e4f60a8b9c0d1e2f3a4b5".into(),
        access_key_id: "demo0access0key0id0000000000000".into(),
        secret_access_key: "demo0secret0access0key00000000000000000000000000000000000000000".into(),
        bucket: "personal-vault".into(),
        public_url: None,
        endpoint: None,
    };
    core.vault.set_destruction_password(DESTRUCTION.as_bytes()).expect("demo: destruction password");
    core.credentials.setup(&cfg, PASSWORD.as_bytes(), false).expect("demo: credentials");
    core.vault.mark_passwords_set().expect("demo: passwords set");
    seed_more_buckets(core, &cfg).await;

    let now = bf_app::now_ms();
    let mut folders = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    let mut enc = EncryptionState { active_epoch: Some(1), ..Default::default() };
    for (i, &(key, size, days, encrypted)) in FILES.iter().enumerate() {
        let mut p = bf_core::media::parent_prefix(key).to_string();
        while !p.is_empty() {
            folders.insert(p.clone());
            p = bf_core::media::parent_prefix(p.trim_end_matches('/')).to_string();
        }
        let mut e = CacheEntry::file(key, size, Some(format!("\"{:016x}{:016x}\"", (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15), (i as u64 + 7).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))), Some(now - days * DAY - (i as i64 * 3_517_000)));
        if encrypted {
            e.encrypted = true;
            e.key_epoch = Some(1);
            enc.encrypted.insert(key.to_string(), 1);
        } else if key.ends_with(".jpg") || key.ends_with(".png") {
            let _ = core.thumbs.put(key, &thumbnail(i));
        }
        entries.push(e);
    }
    entries.extend(folders.iter().map(|f| CacheEntry::folder(f)));
    core.cache_db.replace_all(entries, now - 4 * 60_000).await.expect("demo: cache");
    std::fs::write(core.paths.encryption_state(), serde_json::to_vec(&enc).expect("json")).expect("demo: encryption state");

    // A made-up profile: these paths end up in public screenshots.
    let home = std::path::PathBuf::from(r"C:\Users\alex");
    let t = |id: &str, kind, local: &str, remote: &str, total: u64, done: u64, status, error: Option<&str>, ago_min: i64| TransferRow {
        id: id.into(),
        kind,
        group_id: None,
        local_path: home.join(local).to_string_lossy().into_owned(),
        remote_key: remote.into(),
        total_bytes: total,
        transferred_bytes: done,
        status,
        error: error.map(str::to_string),
        content_type: None,
        multipart_upload_id: None,
        completed_parts: Vec::new(),
        retry_count: 0,
        delete_source_ref: None,
        created_at: now - ago_min * 60_000,
        updated_at: now - ago_min * 60_000 + 30_000,
    };
    use TransferKind::*;
    use TransferStatus::*;
    let rows = [
        t("demo-1", Upload, r"Videos\drone-coastline.mp4", "Videos/drone-coastline.mp4", 1_350 * MB, 912 * MB, Paused, None, 2),
        t("demo-2", Upload, r"Backups\2026-11-15.tar.zst", "Backups/laptop/2026-11-15.tar.zst", 18_000 * MB, 6_400 * MB, Paused, None, 3),
        t("demo-3", Download, r"Downloads\return-2025.pdf", "Documents/Taxes 2025/return-2025.pdf", 1_310_000, 1_310_000, Completed, None, 6),
        t("demo-4", Upload, r"Pictures\Lisbon\sintra-forest.jpg", "Photos/2026 Lisbon/sintra-forest.jpg", 9 * MB, 9 * MB, Completed, None, 14),
        t("demo-5", Upload, r"Pictures\Lisbon\night-market.jpg", "Photos/2026 Lisbon/night-market.jpg", 4 * MB, 4 * MB, Completed, None, 15),
        t("demo-6", Download, r"Downloads\demo-mix.mp3", "Music/demo-mix.mp3", 9 * MB, 9 * MB, Completed, None, 40),
        t("demo-7", Upload, r"Music\harbour.wav", "Music/field-recordings/harbour.wav", 88 * MB, 31 * MB, Failed, Some("The connection dropped. Retry to resume."), 55),
    ];
    for r in rows {
        let _ = core.app_db.insert_transfer(r).await;
    }
    for (id, local, remote, ago) in [
        ("demo-sync-1", r"Pictures\Camera Roll", "Photos/Camera Roll/", 25i64),
        ("demo-sync-2", r"Documents\Work", "Documents/Work/", 180),
    ] {
        let _ = core
            .app_db
            .insert_sync_pair(SyncPair { id: id.into(), local_dir_path: home.join(local).to_string_lossy().into_owned(), remote_prefix: remote.into(), last_scan_at: Some(now - ago * 60_000) })
            .await;
    }
    for (id, key, ago_h, ttl_h) in [
        ("demo-link-1", "Photos/2026 Lisbon/alfama-sunrise.jpg", 2i64, 24i64),
        ("demo-link-2", "Documents/project-roadmap.md", 20, 168),
        ("demo-link-3", "Music/demo-mix.mp3", 30, 72),
    ] {
        let _ = core.app_db.insert_link(SharedLink { id: id.into(), remote_key: key.into(), created_at: now - ago_h * 3_600_000, expires_at: now + (ttl_h - ago_h) * 3_600_000 }).await;
    }
    apply_prefs(core);
}

/// Two more (made-up) buckets for the bucket grid, each with a small listing.
async fn seed_more_buckets(core: &Arc<Core>, primary: &R2Config) {
    use bf_app::buckets::{BucketEntry, PRIMARY_BUCKET_ID};
    let dek = core.credentials.unwrap_dek_with_password(PASSWORD.as_bytes()).expect("demo: dek");
    let mut list = vec![BucketEntry { id: PRIMARY_BUCKET_ID.into(), config: primary.clone() }];
    let extra = [
        ("demowork0000000000000000000000001", "work-archive", None, 140u64, 2 * 1024 * MB),
        ("demofamily00000000000000000000002", "family-photos", Some("https://3f9c2a7b5d1e4f60a8b9c0d1e2f3a4b5.eu.r2.cloudflarestorage.com"), 860, 31 * 1024 * MB),
    ];
    let now = bf_app::now_ms();
    for (id, bucket, endpoint, files, bytes) in extra {
        let mut cfg = primary.clone();
        cfg.bucket = bucket.into();
        cfg.endpoint = endpoint.map(str::to_string);
        list.push(BucketEntry { id: id.into(), config: cfg });
        let stores = core.bucket_stores(id).expect("demo: stores");
        let entries: Vec<CacheEntry> = (0..files).map(|i| CacheEntry::file(&format!("item-{i:04}.bin"), bytes / files, None, Some(now - (i as i64) * DAY / 3))).collect();
        stores.cache_db.replace_all(entries, now - 26 * 60 * 60_000).await.expect("demo: cache");
    }
    core.credentials.seal_buckets(&dek, &list).expect("demo: buckets");
}

fn apply_prefs(core: &Arc<Core>) {
    let dark = env("BYTEFLOW_DEV_THEME").is_some_and(|t| t == "dark");
    let grid = env("BYTEFLOW_DEV_GRID").is_some_and(|g| g == "1");
    let _ = core.prefs.update(|p| {
        p.theme_mode = if dark { bf_app::prefs::ThemeMode::Dark } else { bf_app::prefs::ThemeMode::Light };
        p.files_grid_view = grid;
        p.files_details_open = true;
        p.encrypt_new_uploads = true;
    });
}
