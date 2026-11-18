//! Cloudflare R2 (S3-compatible) access: hand-rolled SigV4, the client, XML
//! parsing, and error mapping (spec §4, §14.2).

pub mod client;
pub mod config;
pub mod failure;
pub mod sigv4;
pub mod xml;

pub use client::{
    BodyProgress, Clock, CompletedPart, DeleteError, HeadInfo, ListPage, ObjectInfo, R2Client,
    SystemClock,
};
pub use config::R2Config;
