//! Preferences (spec §13), stored in secure storage so a wipe erases them.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::browser::BrowserSort;
use bf_core::AppResult;
use bf_data::secure::VaultStateStore;

pub const AUTO_LOCK_OPTIONS: [u64; 5] = [30, 60, 120, 300, 600];
pub const LINK_TTL_PRESETS: [u64; 4] = [900, 3600, 86_400, 604_800];
pub const TRANSFER_CONCURRENCY_OPTIONS: [usize; 5] = [1, 2, 3, 4, 5];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
    System,
}

impl ThemeMode {
    fn parse(s: &str) -> Self {
        match s {
            "dark" => Self::Dark,
            "system" => Self::System,
            _ => Self::Light,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    pub auto_lock_seconds: u64,
    pub transfer_concurrency: usize,
    pub encryption_concurrency: usize,
    pub default_link_ttl_seconds: u64,
    pub wipe_on_bio_change: bool,
    pub encrypt_new_uploads: bool,
    pub download_dir: Option<PathBuf>,
    pub cache_db_dir: Option<PathBuf>,
    pub files_sort: BrowserSort,
    pub files_grid_view: bool,
    /// [DESKTOP] Right-hand details panel on the Files page.
    pub files_details_open: bool,
    pub theme_mode: ThemeMode,
    /// [DESKTOP] Blur on every focus loss, not only minimise/lock (§17.1).
    pub privacy_blur_on_focus_loss: bool,
    /// [DESKTOP] Seconds after which a copied share link is cleared from the
    /// clipboard (0 = never). §17.1.
    pub clipboard_clear_seconds: u64,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            auto_lock_seconds: 120,
            transfer_concurrency: 3,
            encryption_concurrency: 2,
            default_link_ttl_seconds: 3600,
            wipe_on_bio_change: false,
            encrypt_new_uploads: false,
            download_dir: None,
            cache_db_dir: None,
            files_sort: BrowserSort::NameAsc,
            files_grid_view: false,
            files_details_open: true,
            theme_mode: ThemeMode::Light,
            privacy_blur_on_focus_loss: false,
            clipboard_clear_seconds: 60,
        }
    }
}

mod k {
    pub const AUTO_LOCK: &str = "auto_lock_seconds";
    pub const TRANSFER_CONC: &str = "transfer_concurrency";
    pub const ENC_CONC: &str = "encryption_concurrency";
    pub const LINK_TTL: &str = "default_link_ttl_seconds";
    pub const WIPE_BIO: &str = "wipe_on_bio_change";
    pub const ENC_UPLOADS: &str = "encrypt_new_uploads";
    pub const DOWNLOAD_DIR: &str = "download_dir";
    pub const CACHE_DIR: &str = "cache_db_dir";
    pub const SORT: &str = "files_sort";
    pub const GRID: &str = "files_grid_view";
    pub const DETAILS: &str = "files_details_open";
    pub const THEME: &str = "theme_mode";
    pub const BLUR_FOCUS: &str = "privacy_blur_on_focus_loss";
    pub const CLIP_CLEAR: &str = "clipboard_clear_seconds";
}

pub struct PrefStore {
    vault: Arc<VaultStateStore>,
    cur: RwLock<Prefs>,
}

fn b(s: Option<String>, d: bool) -> bool {
    s.map(|v| v == "1" || v == "true").unwrap_or(d)
}

impl PrefStore {
    pub fn load(vault: Arc<VaultStateStore>) -> Self {
        let d = Prefs::default();
        let num = |key: &str, def: u64| vault.pref(key).and_then(|v| v.parse().ok()).unwrap_or(def);
        let p = Prefs {
            auto_lock_seconds: num(k::AUTO_LOCK, d.auto_lock_seconds),
            transfer_concurrency: num(k::TRANSFER_CONC, d.transfer_concurrency as u64).clamp(1, 8) as usize,
            encryption_concurrency: num(k::ENC_CONC, d.encryption_concurrency as u64).clamp(1, 8) as usize,
            default_link_ttl_seconds: num(k::LINK_TTL, d.default_link_ttl_seconds).clamp(1, 604_800),
            wipe_on_bio_change: b(vault.pref(k::WIPE_BIO), d.wipe_on_bio_change),
            encrypt_new_uploads: b(vault.pref(k::ENC_UPLOADS), d.encrypt_new_uploads),
            download_dir: vault.pref(k::DOWNLOAD_DIR).map(PathBuf::from),
            cache_db_dir: vault.pref(k::CACHE_DIR).map(PathBuf::from),
            files_sort: vault.pref(k::SORT).map(|s| BrowserSort::parse(&s)).unwrap_or(d.files_sort),
            files_grid_view: b(vault.pref(k::GRID), d.files_grid_view),
            files_details_open: b(vault.pref(k::DETAILS), d.files_details_open),
            theme_mode: vault.pref(k::THEME).map(|s| ThemeMode::parse(&s)).unwrap_or(d.theme_mode),
            privacy_blur_on_focus_loss: b(vault.pref(k::BLUR_FOCUS), d.privacy_blur_on_focus_loss),
            clipboard_clear_seconds: num(k::CLIP_CLEAR, d.clipboard_clear_seconds),
        };
        Self { vault, cur: RwLock::new(p) }
    }

    pub fn get(&self) -> Prefs {
        self.cur.read().clone()
    }

    /// Mutate, persisting only the keys that changed.
    pub fn update(&self, f: impl FnOnce(&mut Prefs)) -> AppResult<()> {
        let old = self.get();
        let mut new = old.clone();
        f(&mut new);
        let v = &self.vault;
        let s = |x: u64| x.to_string();
        let bo = |x: bool| if x { "1" } else { "0" }.to_string();
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.to_string_lossy().into_owned());
        if old.auto_lock_seconds != new.auto_lock_seconds {
            v.set_pref(k::AUTO_LOCK, Some(&s(new.auto_lock_seconds)))?;
        }
        if old.transfer_concurrency != new.transfer_concurrency {
            v.set_pref(k::TRANSFER_CONC, Some(&s(new.transfer_concurrency as u64)))?;
        }
        if old.encryption_concurrency != new.encryption_concurrency {
            v.set_pref(k::ENC_CONC, Some(&s(new.encryption_concurrency as u64)))?;
        }
        if old.default_link_ttl_seconds != new.default_link_ttl_seconds {
            v.set_pref(k::LINK_TTL, Some(&s(new.default_link_ttl_seconds)))?;
        }
        if old.wipe_on_bio_change != new.wipe_on_bio_change {
            v.set_pref(k::WIPE_BIO, Some(&bo(new.wipe_on_bio_change)))?;
        }
        if old.encrypt_new_uploads != new.encrypt_new_uploads {
            v.set_pref(k::ENC_UPLOADS, Some(&bo(new.encrypt_new_uploads)))?;
        }
        if old.download_dir != new.download_dir {
            v.set_pref(k::DOWNLOAD_DIR, path(&new.download_dir).as_deref())?;
        }
        if old.cache_db_dir != new.cache_db_dir {
            v.set_pref(k::CACHE_DIR, path(&new.cache_db_dir).as_deref())?;
        }
        if old.files_sort != new.files_sort {
            v.set_pref(k::SORT, Some(new.files_sort.as_str()))?;
        }
        if old.files_grid_view != new.files_grid_view {
            v.set_pref(k::GRID, Some(&bo(new.files_grid_view)))?;
        }
        if old.files_details_open != new.files_details_open {
            v.set_pref(k::DETAILS, Some(&bo(new.files_details_open)))?;
        }
        if old.theme_mode != new.theme_mode {
            v.set_pref(k::THEME, Some(new.theme_mode.as_str()))?;
        }
        if old.privacy_blur_on_focus_loss != new.privacy_blur_on_focus_loss {
            v.set_pref(k::BLUR_FOCUS, Some(&bo(new.privacy_blur_on_focus_loss)))?;
        }
        if old.clipboard_clear_seconds != new.clipboard_clear_seconds {
            v.set_pref(k::CLIP_CLEAR, Some(&s(new.clipboard_clear_seconds)))?;
        }
        *self.cur.write() = new;
        Ok(())
    }

    /// After a wipe: back to defaults.
    pub fn reset(&self) {
        *self.cur.write() = Prefs::default();
    }

    /// Effective download folder: custom if reachable, else the OS default.
    pub fn download_dir(&self) -> PathBuf {
        match self.get().download_dir {
            Some(d) if d.is_dir() => d,
            _ => bf_data::paths::default_download_dir(),
        }
    }
}
