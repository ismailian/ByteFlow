//! ByteFlow data layer: the R2 client and SigV4 signer, S3 XML parsing,
//! secure-storage stores, the two SQLite databases, and the thumbnail store.

pub mod local;
pub mod paths;
pub mod r2;
pub mod secure;
