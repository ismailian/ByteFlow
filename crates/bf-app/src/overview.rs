//! Overview statistics, all from the cache (spec §6.2). R2 has no quota, so
//! composition is shown — never a fill percentage.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Duration, Local, TimeZone};
use parking_lot::RwLock;

use bf_core::AppResult;
use bf_core::media::{StorageCategory, storage_category};
use bf_data::local::CacheDb;

use crate::SharedNotifier;

#[derive(Debug, Clone, Default)]
pub struct Overview {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub composition: Vec<(StorageCategory, u64, u64)>,
    /// Oldest → newest, local-time days: (label, uploads).
    pub histogram: Vec<(String, u64)>,
    pub has_scanned: bool,
    pub last_scan_ms: Option<i64>,
}

pub struct OverviewController {
    cache: Arc<CacheDb>,
    data: RwLock<Arc<Overview>>,
    notifier: SharedNotifier,
}

/// Bucket timestamps into the last 7 local-time days.
pub fn histogram(timestamps: &[i64], now: chrono::DateTime<Local>) -> Vec<(String, u64)> {
    let today = now.date_naive();
    let days: Vec<_> = (0..7).rev().map(|i| today - Duration::days(i)).collect();
    let mut counts: HashMap<chrono::NaiveDate, u64> = HashMap::new();
    for &t in timestamps {
        if let Some(dt) = Local.timestamp_millis_opt(t).single() {
            *counts.entry(dt.date_naive()).or_default() += 1;
        }
    }
    days.into_iter().map(|d| (d.format("%a").to_string(), counts.get(&d).copied().unwrap_or(0))).collect()
}

impl OverviewController {
    pub fn new(cache: Arc<CacheDb>, notifier: SharedNotifier) -> Arc<Self> {
        Arc::new(Self { cache, data: RwLock::new(Arc::new(Overview::default())), notifier })
    }

    pub fn data(&self) -> Arc<Overview> {
        self.data.read().clone()
    }

    pub async fn reload(&self) {
        if let Ok(o) = self.load().await {
            *self.data.write() = Arc::new(o);
        }
        self.notifier.changed();
    }

    async fn load(&self) -> AppResult<Overview> {
        let (files, folders, bytes) = self.cache.totals().await?;
        let mut comp: HashMap<StorageCategory, (u64, u64)> = HashMap::new();
        for (k, s) in self.cache.file_key_sizes().await? {
            let e = comp.entry(storage_category(&k)).or_default();
            e.0 += 1;
            e.1 += s;
        }
        let composition = StorageCategory::ALL.iter().map(|c| {
            let (n, b) = comp.get(c).copied().unwrap_or_default();
            (*c, n, b)
        }).collect();
        let now = Local::now();
        let since = (now - Duration::days(7)).timestamp_millis();
        let meta = self.cache.meta().await?;
        Ok(Overview {
            files,
            folders,
            bytes,
            composition,
            histogram: histogram(&self.cache.object_timestamps_since(since).await?, now),
            has_scanned: meta.last_scan_completed_at_ms.is_some(),
            last_scan_ms: meta.last_scan_completed_at_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seven_days_oldest_first() {
        let now = Local::now();
        let h = histogram(&[now.timestamp_millis(), now.timestamp_millis(), (now - Duration::days(6)).timestamp_millis()], now);
        assert_eq!(h.len(), 7);
        assert_eq!(h[6].1, 2);
        assert_eq!(h[0].1, 1);
    }
}
