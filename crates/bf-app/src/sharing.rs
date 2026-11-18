//! Share links (spec §12): presigned GET signed locally, TTL clamped to 7 d.
//! History records **never** store the signed URL (it carries a signature
//! derived from the secret).

use std::sync::Arc;

use parking_lot::RwLock;

use bf_core::AppResult;
use bf_data::local::AppDb;
use bf_data::local::app_db::SharedLink;
use bf_data::r2::sigv4::PRESIGN_MAX_TTL;

use crate::session::ClientHandle;
use crate::{SharedNotifier, new_id, now_ms};

/// Playback presigns are not share links and are never recorded.
pub const PLAYBACK_TTL: u64 = 6 * 3600;

#[derive(Debug, Clone)]
pub struct IssuedLink {
    pub remote_key: String,
    pub url: String,
    pub expires_at: i64,
}

pub struct SharingService {
    db: AppDb,
    client: ClientHandle,
    notifier: SharedNotifier,
    links: RwLock<Vec<SharedLink>>,
}

impl SharingService {
    pub fn new(db: AppDb, client: ClientHandle, notifier: SharedNotifier) -> Arc<Self> {
        Arc::new(Self { db, client, notifier, links: RwLock::new(vec![]) })
    }

    pub fn links(&self) -> Vec<SharedLink> {
        self.links.read().clone()
    }

    pub async fn reload(&self) {
        if let Ok(l) = self.db.links().await {
            *self.links.write() = l;
        }
        self.notifier.changed();
    }

    /// One link per file.
    pub async fn create(&self, keys: &[String], ttl_secs: u64) -> AppResult<Vec<IssuedLink>> {
        let ttl = ttl_secs.clamp(1, PRESIGN_MAX_TTL);
        let client = self.client.get();
        let now = now_ms();
        let mut out = vec![];
        for k in keys {
            let leaf = bf_core::media::leaf_name(k).replace('"', "");
            let disposition = format!("attachment; filename=\"{leaf}\"");
            let url = client.presign_get(k, ttl, Some(&disposition));
            let expires_at = now + ttl as i64 * 1000;
            self.db.insert_link(SharedLink { id: new_id(), remote_key: k.clone(), created_at: now, expires_at }).await?;
            out.push(IssuedLink { remote_key: k.clone(), url, expires_at });
        }
        self.reload().await;
        Ok(out)
    }

    /// For in-app streaming; not recorded.
    pub fn playback_url(&self, key: &str) -> String {
        self.client.get().presign_get(key, PLAYBACK_TTL, None)
    }

    pub async fn forget(&self, id: &str) -> AppResult<()> {
        self.db.delete_link(id).await?;
        self.reload().await;
        Ok(())
    }

    pub async fn forget_expired(&self) -> AppResult<()> {
        self.db.delete_expired_links(now_ms()).await?;
        self.reload().await;
        Ok(())
    }
}
