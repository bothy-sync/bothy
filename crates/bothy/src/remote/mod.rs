//! Remote storage backends. Every file is immutable once written; there is no overwrite.

mod folder;

use async_trait::async_trait;

use crate::error::Result;

pub use folder::FolderRemote;

/// A file in the remote store, named by its logical key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    /// Logical key, e.g. `<db>/devices/<device>/seg-<seq>.bseg`.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
}

/// Result of [`RemoteStore::put_new`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    /// The file was created.
    Created,
    /// A file with this key already exists.
    AlreadyExists {
        /// Whether its content equals the bytes we tried to write. `true` means an earlier
        /// attempt succeeded; `false` means another writer used the key.
        same_content: bool,
    },
}

/// Files that appeared or disappeared since a cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeBatch {
    /// New files.
    pub changed: Vec<RemoteEntry>,
    /// Keys of deleted files.
    pub removed: Vec<String>,
    /// Cursor for the next call.
    pub cursor: String,
}

/// A remote store: a flat namespace of immutable files addressed by logical keys.
///
/// Errors use [`crate::ErrorKind`]: `Offline` for network failures, `RateLimited`,
/// `QuotaExceeded` and `AuthRequired` where they apply.
#[async_trait]
pub trait RemoteStore: Send + Sync + 'static {
    /// All files whose key starts with `prefix`.
    async fn list(&self, prefix: &str) -> Result<Vec<RemoteEntry>>;

    /// A file's bytes, or `None` if it does not exist.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Reserves an upload slot for `key` before the bytes are sent, if the backend needs one
    /// (Drive pre-generates file ids). The caller stores the token with the bytes so a retry
    /// after a crash reuses it.
    async fn reserve(&self, key: &str) -> Result<Option<String>> {
        let _ = key;
        Ok(None)
    }

    /// Creates `key` with `data` if it does not exist yet. Never overwrites.
    async fn put_new(
        &self,
        key: &str,
        data: &[u8],
        reservation: Option<&str>,
    ) -> Result<PutOutcome>;

    /// Deletes `key`. Deleting a missing file succeeds.
    async fn delete(&self, key: &str) -> Result<()>;

    /// Changes since `cursor` (`None`: start, returning every current file). Backends without a
    /// change feed return `Ok(None)` and the engine lists instead.
    async fn changes_since(&self, cursor: Option<&str>) -> Result<Option<ChangeBatch>> {
        let _ = cursor;
        Ok(None)
    }
}
