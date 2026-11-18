//! Folder navigation and listing over the cache (spec §6.1). Navigation lives
//! in controller state (not a route). Everything here is a local query —
//! zero R2 calls — except an explicit `refresh()`, which runs the full scan.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use bf_core::AppResult;
use bf_data::local::CacheDb;
use bf_data::local::cache_db::{CacheEntry, Usage};

use crate::SharedNotifier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserSort {
    NameAsc,
    NameDesc,
    DateNewest,
    DateOldest,
    SizeLargest,
    SizeSmallest,
}

impl BrowserSort {
    pub const ALL: [BrowserSort; 6] =
        [Self::NameAsc, Self::NameDesc, Self::DateNewest, Self::DateOldest, Self::SizeLargest, Self::SizeSmallest];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NameAsc => "nameAsc",
            Self::NameDesc => "nameDesc",
            Self::DateNewest => "dateNewest",
            Self::DateOldest => "dateOldest",
            Self::SizeLargest => "sizeLargest",
            Self::SizeSmallest => "sizeSmallest",
        }
    }
    /// Unknown values fall back to the default.
    pub fn parse(s: &str) -> Self {
        Self::ALL.into_iter().find(|v| v.as_str() == s).unwrap_or(Self::NameAsc)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::NameAsc => "Name A–Z",
            Self::NameDesc => "Name Z–A",
            Self::DateNewest => "Newest first",
            Self::DateOldest => "Oldest first",
            Self::SizeLargest => "Largest first",
            Self::SizeSmallest => "Smallest first",
        }
    }
}

fn name_key(s: &str) -> String {
    bf_core::media::leaf_name(s).to_lowercase()
}

/// Folders always first, ordered by name (a name sort flips them too).
pub fn sort_entries(folders: &mut [CacheEntry], files: &mut [CacheEntry], sort: BrowserSort) {
    folders.sort_by_cached_key(|e| name_key(&e.key));
    if sort == BrowserSort::NameDesc {
        folders.reverse();
    }
    match sort {
        BrowserSort::NameAsc => files.sort_by_cached_key(|e| name_key(&e.key)),
        BrowserSort::NameDesc => {
            files.sort_by_cached_key(|e| name_key(&e.key));
            files.reverse();
        }
        BrowserSort::DateNewest => files.sort_by_key(|e| std::cmp::Reverse(e.last_modified_ms.unwrap_or(i64::MIN))),
        BrowserSort::DateOldest => files.sort_by_key(|e| e.last_modified_ms.unwrap_or(i64::MAX)),
        BrowserSort::SizeLargest => files.sort_by_key(|e| std::cmp::Reverse(e.size)),
        BrowserSort::SizeSmallest => files.sort_by_key(|e| e.size),
    }
}

#[derive(Debug, Clone, Default)]
pub struct BrowserData {
    pub prefix: String,
    pub folders: Vec<CacheEntry>,
    pub files: Vec<CacheEntry>,
    pub counts: HashMap<String, Usage>,
    pub has_scanned: bool,
    pub loading: bool,
}

impl BrowserData {
    pub fn breadcrumbs(&self) -> Vec<(String, String)> {
        let mut out = vec![("Bucket".to_string(), String::new())];
        let mut acc = String::new();
        for seg in self.prefix.split('/').filter(|s| !s.is_empty()) {
            acc.push_str(seg);
            acc.push('/');
            out.push((seg.to_string(), acc.clone()));
        }
        out
    }

    /// Case-insensitive filter within the current folder.
    pub fn filtered(&self, query: &str) -> (Vec<&CacheEntry>, Vec<&CacheEntry>) {
        let q = query.trim().to_lowercase();
        let m = |e: &&CacheEntry| q.is_empty() || name_key(&e.key).contains(&q);
        (self.folders.iter().filter(m).collect(), self.files.iter().filter(m).collect())
    }
}

pub struct BrowserController {
    cache: Arc<CacheDb>,
    prefix: RwLock<String>,
    data: RwLock<Arc<BrowserData>>,
    sort: RwLock<BrowserSort>,
    notifier: SharedNotifier,
    generation: std::sync::atomic::AtomicU64,
}

impl BrowserController {
    pub fn new(cache: Arc<CacheDb>, sort: BrowserSort, notifier: SharedNotifier) -> Arc<Self> {
        Arc::new(Self {
            cache,
            prefix: RwLock::new(String::new()),
            data: RwLock::new(Arc::new(BrowserData { loading: true, ..Default::default() })),
            sort: RwLock::new(sort),
            notifier,
            generation: Default::default(),
        })
    }

    pub fn data(&self) -> Arc<BrowserData> {
        self.data.read().clone()
    }
    pub fn prefix(&self) -> String {
        self.prefix.read().clone()
    }
    pub fn sort(&self) -> BrowserSort {
        *self.sort.read()
    }

    pub async fn set_sort(&self, sort: BrowserSort) {
        *self.sort.write() = sort;
        self.reload().await;
    }

    pub async fn open_folder(&self, prefix: &str) {
        *self.prefix.write() = prefix.to_string();
        self.reload().await;
    }

    /// Returns false at root, so back/escape can leave a folder first.
    pub async fn go_parent(&self) -> bool {
        let p = self.prefix();
        if p.is_empty() {
            return false;
        }
        let parent = bf_core::media::parent_prefix(&p).to_string();
        self.open_folder(&parent).await;
        true
    }

    /// Re-read from the cache; **no network I/O**.
    pub async fn reload(&self) {
        let generation = self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let prefix = self.prefix();
        let sort = self.sort();
        let result = self.load(&prefix, sort).await;
        // Drop stale results if the user navigated meanwhile.
        if self.generation.load(std::sync::atomic::Ordering::SeqCst) != generation {
            return;
        }
        if let Ok(d) = result {
            *self.data.write() = Arc::new(d);
        }
        self.notifier.changed();
    }

    async fn load(&self, prefix: &str, sort: BrowserSort) -> AppResult<BrowserData> {
        let children = self.cache.children_of(prefix).await?;
        let (mut folders, mut files): (Vec<_>, Vec<_>) = children.into_iter().partition(|e| e.is_folder);
        // Markers are folders in the cache; nothing to hide among files.
        files.retain(|e| !e.key.ends_with('/'));
        sort_entries(&mut folders, &mut files, sort);
        let usages = self.cache.folder_usages(folders.iter().map(|f| f.key.clone()).collect()).await?;
        let counts = folders.iter().map(|f| f.key.clone()).zip(usages).collect();
        Ok(BrowserData {
            prefix: prefix.to_string(),
            folders,
            files,
            counts,
            has_scanned: self.cache.has_scanned().await?,
            loading: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(key: &str, size: u64, t: Option<i64>) -> CacheEntry {
        CacheEntry::file(key, size, None, t)
    }

    #[test]
    fn folders_first_and_flip_on_name_sort() {
        let mut folders = vec![CacheEntry::folder("b/"), CacheEntry::folder("a/")];
        let mut files = vec![f("z.txt", 1, Some(1)), f("y.txt", 5, None), f("x.txt", 3, Some(9))];
        sort_entries(&mut folders, &mut files, BrowserSort::NameDesc);
        assert_eq!(folders[0].key, "b/");
        assert_eq!(files[0].key, "z.txt");
        sort_entries(&mut folders, &mut files, BrowserSort::SizeLargest);
        assert_eq!(folders[0].key, "a/", "folders stay by name");
        assert_eq!(files[0].key, "y.txt");
        sort_entries(&mut folders, &mut files, BrowserSort::DateNewest);
        assert_eq!(files[0].key, "x.txt");
        assert_eq!(files[2].key, "y.txt", "undated last");
        assert_eq!(BrowserSort::parse("garbage"), BrowserSort::NameAsc);
    }

    #[test]
    fn crumbs() {
        let d = BrowserData { prefix: "a/b/".into(), ..Default::default() };
        let c = d.breadcrumbs();
        assert_eq!(c.len(), 3);
        assert_eq!(c[2], ("b".to_string(), "a/b/".to_string()));
    }
}
