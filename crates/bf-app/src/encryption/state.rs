//! `encryption_state.json` (spec §9.5): the encrypted-key index (the only
//! source the app consults for "is this encrypted", §9.6) plus the queue.
//!
//! Writes are **atomic** (`.tmp` + rename) and **serialised** through a single
//! writer task that coalesces bursts, so concurrent task transitions can't
//! interleave or clobber each other. A corrupt file loads as empty state.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use bf_core::log_warn;

pub const EXTERNAL_EPOCH: i64 = -1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EncOp {
    Encrypt,
    Decrypt,
    ReEncrypt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EncStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptTask {
    pub id: String,
    pub remote_key: String,
    pub op: EncOp,
    pub status: EncStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptionState {
    #[serde(default)]
    pub active_epoch: Option<i64>,
    #[serde(default)]
    pub rekey_to_epoch: Option<i64>,
    #[serde(default)]
    pub removing: bool,
    #[serde(default)]
    pub encrypted: BTreeMap<String, i64>,
    #[serde(default)]
    pub queue: Vec<EncryptTask>,
}

impl EncryptionState {
    pub fn is_active(&self, key: &str) -> bool {
        self.queue
            .iter()
            .any(|t| t.remote_key == key && matches!(t.status, EncStatus::Queued | EncStatus::Running))
    }
    pub fn app_encrypted(&self) -> impl Iterator<Item = (&String, &i64)> {
        self.encrypted.iter().filter(|(_, e)| **e != EXTERNAL_EPOCH)
    }
    pub fn busy(&self) -> bool {
        self.queue.iter().any(|t| matches!(t.status, EncStatus::Queued | EncStatus::Running))
    }
}

pub struct StateStore {
    path: PathBuf,
    pub state: Arc<RwLock<EncryptionState>>,
    notify: Arc<Notify>,
}

impl StateStore {
    pub fn load(path: PathBuf) -> Self {
        let state = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|_| {
                log_warn!("encryption state corrupt; starting empty");
                EncryptionState::default()
            }),
            Err(_) => EncryptionState::default(),
        };
        Self { path, state: Arc::new(RwLock::new(state)), notify: Arc::new(Notify::new()) }
    }

    /// Spawn the single serialised writer. Ends when `cancel` fires, after a
    /// final flush.
    pub fn spawn_writer(&self, cancel: tokio_util::sync::CancellationToken) -> tokio::task::JoinHandle<()> {
        let (path, state, notify) = (self.path.clone(), self.state.clone(), self.notify.clone());
        tokio::spawn(async move {
            loop {
                let stop = tokio::select! {
                    _ = cancel.cancelled() => true,
                    _ = notify.notified() => false,
                };
                let json = serde_json::to_vec(&*state.read());
                if let Ok(json) = json {
                    let p = path.clone();
                    let _ = tokio::task::spawn_blocking(move || bf_data::secure::store::atomic_write(&p, &json)).await;
                }
                if stop {
                    return;
                }
                // Coalesce bursts of transitions into one write.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
    }

    pub fn save(&self) {
        self.notify.notify_one();
    }

    pub fn read<R>(&self, f: impl FnOnce(&EncryptionState) -> R) -> R {
        f(&self.state.read())
    }

    pub fn write<R>(&self, f: impl FnOnce(&mut EncryptionState) -> R) -> R {
        let r = f(&mut self.state.write());
        self.save();
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupt_loads_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        std::fs::write(&p, b"{not json").unwrap();
        let s = StateStore::load(p);
        assert!(s.read(|s| s.encrypted.is_empty()));
    }
}
