//! # Bothy
//!
//! Bothy is a local-first sync engine for applications that have no server of their own.
//! Each device keeps its data in a local SQLite database, and devices exchange immutable,
//! append-only files through the user's own cloud storage (Google Drive first).
//!
//! **Status: early development.** The local store, the merge rules, the file formats and the
//! sync engine work against a local or shared folder ([`remote::FolderRemote`]). The Google
//! Drive backend and the CLI are in progress at <https://github.com/bothy-sync/bothy>.
//!
//! ```
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> bothy::Result<()> {
//! use std::sync::Arc;
//! use bothy::{Config, Database, remote::FolderRemote};
//! use serde_json::json;
//!
//! # let dir = tempfile::tempdir().unwrap();
//! # let (local_a, local_b, shared) = (dir.path().join("a.db"), dir.path().join("b.db"), dir.path().join("shared"));
//! let remote = Arc::new(FolderRemote::new(shared)?);
//!
//! // Device A creates the database and writes a note.
//! let a = Database::open(Config::new(local_a, "com.example.notes"), remote.clone()).await?;
//! let db_id = a.init().await?;
//! a.collection("notes")?.put("n1", json!({"title": "Hello"})).await?;
//! a.sync().await?;
//!
//! // Device B joins and receives it.
//! let b = Database::open(Config::new(local_b, "com.example.notes"), remote).await?;
//! b.join(db_id).await?;
//! let note = b.collection("notes")?.get("n1").await?.unwrap();
//! assert_eq!(note.to_json(), json!({"title": "Hello"}));
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

#[cfg(feature = "blocking")]
pub mod blocking;
pub mod codec;
mod database;
mod engine;
mod error;
mod hlc;
pub mod merge;
pub mod model;
pub mod remote;
pub mod store;

pub use database::{
    ChangeEvent, Collection, Config, Database, IntoDocument, ListOptions, Origin, SnapshotPolicy,
    SyncReport,
};
pub use engine::{AutoSync, CompactReport, SyncHandle, SyncStatus};
pub use error::{Error, ErrorKind, Result};
pub use hlc::{Clock, DEFAULT_MAX_DRIFT_MS, Hlc, HlcClock, ParseHlcError, SystemClock};
pub use model::{CollectionName, DbId, DeviceId, DocId, FieldName, Op, OpKind, Stamp, Value};

/// Version of the on-remote file format written by this crate (`bothy_format` in
/// `manifest.json`). Draft: the format is not frozen until `SPEC.md` v1.
pub const FORMAT_VERSION: u16 = 1;
