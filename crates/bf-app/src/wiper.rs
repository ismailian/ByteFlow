//! The wipe sequence (spec §3.6): ordered, idempotent, resumable.
//!
//! 0. tombstone `wipe_pending = 1` before anything else
//! 1. delete the hardware key alias
//! 2. overwrite every secure-storage key (except the tombstone) with 32
//!    random bytes, then delete it
//! 3. delete `r2vault.sqlite` + `-wal` + `-shm` (the cache DB is exempt)
//! 4. empty temp, app-support, app cache — **never** the Documents folder,
//!    never a custom download folder, never a custom-located cache DB
//! 5. delete the tombstone last (then the secure-store file itself)
//!
//! Each step is wrapped so one failure never halts the wipe.

use std::fs;
use std::path::{Path, PathBuf};

use bf_core::{AppResult, log_warn};
use bf_data::secure::SecureStore;
use bf_data::secure::vault_state_store::keys as vkeys;

use crate::core::Core;

fn step(name: &str, r: AppResult<()>) {
    if let Err(e) = r {
        log_warn!("wipe step {name}: {}", e.developer_message());
    }
}

fn sqlite_files(main: &Path) -> Vec<PathBuf> {
    ["", "-wal", "-shm", "-journal"]
        .iter()
        .map(|s| {
            let mut p = main.as_os_str().to_os_string();
            p.push(s);
            PathBuf::from(p)
        })
        .collect()
}

/// Empty a directory, skipping `keep` paths (the secure store, which holds
/// the tombstone until step 5, and the single-instance lock).
fn empty_dir(dir: &Path, keep: &[PathBuf]) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if keep.iter().any(|k| k == &p) {
            continue;
        }
        let r = if p.is_dir() && !e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
        if let Err(err) = r {
            log_warn!("wipe: could not remove an entry: {err}");
        }
    }
}

/// Blocking: run on the blocking pool. The session must already be torn down.
pub fn wipe(core: &Core) {
    // 0. Tombstone first.
    step("tombstone", core.vault.set_wipe_pending());
    // 1. Hardware key alias.
    step("hw key", core.credentials.delete_bio_key());
    // 2. Overwrite then delete every secure-storage key except the tombstone.
    for e in core.secure.overwrite_and_delete_all(vkeys::WIPE_PENDING) {
        log_warn!("wipe secure key: {}", e.developer_message());
    }
    // 3. The app DB (close the handle so the file can go on Windows).
    step("app db close", core.app_db.close());
    for f in sqlite_files(&core.paths.app_db()) {
        if f.exists() {
            step("app db", fs::remove_file(&f).map_err(|e| bf_core::AppFailure::io("remove app db", e)));
        }
    }
    // 4. Scratch directories. A default-located cache DB lives in support and
    //    goes with it; a custom-located one is untouched (deliberate, §5.6).
    let cache_in_support = core.cache_db.path().starts_with(&core.paths.support);
    if cache_in_support {
        step("cache close", core.cache_db.close());
    }
    let keep = vec![core.paths.secure_store(), core.paths.instance_lock()];
    for d in core.paths.scratch_directories() {
        empty_dir(d, &keep);
    }
    step("thumbs", core.thumbs.clear());
    // 5. Tombstone last, then the (now empty) store file and OS key material.
    step("tombstone clear", core.vault.clear_wipe_pending());
    step("store file", core.secure.destroy_backing());

    // Reopen fresh databases so the app looks exactly like a fresh install.
    step("app db reopen", core.app_db.reopen(&core.paths.app_db()));
    if cache_in_support {
        step("cache reopen", core.cache_db.reopen(&core.paths.default_cache_db_dir().join(bf_data::paths::CACHE_DB_FILE)));
    }
    core.prefs.reset();
}

/// On every launch: a set tombstone re-runs the whole wipe first.
pub fn resume_if_pending(core: &Core) -> bool {
    if core.vault.wipe_pending().unwrap_or(false) {
        wipe(core);
        return true;
    }
    false
}

pub fn store_is_empty(store: &dyn SecureStore) -> bool {
    store.keys().map(|k| k.is_empty()).unwrap_or(false)
}
