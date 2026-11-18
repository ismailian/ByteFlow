//! Local persistence: the app DB, the (separate) bucket-listing cache DB, and
//! the thumbnail store (spec §2.5, §5, §10.4).

pub mod app_db;
pub mod cache_db;
pub mod db;
pub mod thumbnail_store;

pub use app_db::AppDb;
pub use cache_db::CacheDb;
pub use db::Db;
pub use thumbnail_store::ThumbnailStore;
