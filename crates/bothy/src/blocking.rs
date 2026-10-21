//! Synchronous API for callers without an async runtime (CLI, Python, C).
//!
//! Each [`Database`] owns a small Tokio runtime. Calling these methods from inside an async
//! runtime returns `InvalidInput`; use [`crate::Database`] there instead.

use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio::sync::broadcast;

use crate::Config;
use crate::database::{ChangeEvent, IntoDocument, ListOptions, SyncReport};
use crate::engine::{AutoSync, CompactReport, SyncStatus};
use crate::error::{Error, ErrorKind, Result};
use crate::merge::DocState;
use crate::model::{CollectionName, DbId, DeviceId, DocId, Value};
use crate::remote::RemoteStore;
use crate::store::{Conflict, DeviceStatus, LocalStatus};

fn outside_runtime() -> Result<()> {
    if tokio::runtime::Handle::try_current().is_ok() {
        Err(Error::invalid_input(
            "the blocking API was called inside an async runtime; use bothy::Database instead",
        ))
    } else {
        Ok(())
    }
}

/// Synchronous counterpart of [`crate::Database`].
#[derive(Clone, Debug)]
pub struct Database {
    rt: Arc<Runtime>,
    db: crate::Database,
}

impl Database {
    /// Opens (or creates) the local database. See [`crate::Database::open`].
    pub fn open(config: Config, remote: Arc<dyn RemoteStore>) -> Result<Self> {
        outside_runtime()?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("bothy")
            .enable_time()
            .build()
            .map_err(|e| Error::new(ErrorKind::Internal, "cannot start runtime").with_source(e))?;
        let db = rt.block_on(crate::Database::open(config, remote))?;
        Ok(Self {
            rt: Arc::new(rt),
            db,
        })
    }

    fn run<T>(&self, f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        outside_runtime()?;
        self.rt.block_on(f)
    }

    /// This device's id.
    pub fn device_id(&self) -> DeviceId {
        self.db.device_id()
    }

    /// See [`crate::Database::db_id`].
    pub fn db_id(&self) -> Result<Option<DbId>> {
        self.run(self.db.db_id())
    }

    /// See [`crate::Database::init`].
    pub fn init(&self) -> Result<DbId> {
        self.run(self.db.init())
    }

    /// See [`crate::Database::join`].
    pub fn join(&self, db_id: DbId) -> Result<SyncReport> {
        self.run(self.db.join(db_id))
    }

    /// See [`crate::Database::init_or_join`].
    pub fn init_or_join(&self) -> Result<DbId> {
        self.run(self.db.init_or_join())
    }

    /// See [`crate::Database::sync`].
    pub fn sync(&self) -> Result<SyncReport> {
        self.run(self.db.sync())
    }

    /// See [`crate::Database::is_encrypted`].
    pub fn is_encrypted(&self) -> Result<bool> {
        self.run(self.db.is_encrypted())
    }

    /// See [`crate::Database::is_locked`].
    pub fn is_locked(&self) -> Result<bool> {
        self.run(self.db.is_locked())
    }

    /// See [`crate::Database::unlock`].
    #[cfg(feature = "crypto")]
    pub fn unlock(&self, password: &str) -> Result<()> {
        self.run(self.db.unlock(password))
    }

    /// See [`crate::Database::unlock_with_key`].
    #[cfg(feature = "crypto")]
    pub fn unlock_with_key(&self, key: crate::crypto::DataKey) -> Result<()> {
        self.run(self.db.unlock_with_key(key))
    }

    /// See [`crate::Database::lock`].
    pub fn lock(&self) {
        self.db.lock();
    }

    /// See [`crate::Database::export_key`].
    #[cfg(feature = "crypto")]
    pub fn export_key(&self) -> Result<crate::crypto::DataKey> {
        self.db.export_key()
    }

    /// See [`crate::Database::change_password`].
    #[cfg(feature = "crypto")]
    pub fn change_password(&self, old: &str, new: &str) -> Result<()> {
        self.run(self.db.change_password(old, new))
    }

    /// See [`crate::Database::snapshot`].
    pub fn snapshot(&self) -> Result<String> {
        self.run(self.db.snapshot())
    }

    /// See [`crate::Database::compact`].
    pub fn compact(&self, dry_run: bool) -> Result<CompactReport> {
        self.run(self.db.compact(dry_run))
    }

    /// See [`crate::Database::status`].
    pub fn status(&self) -> Result<LocalStatus> {
        self.run(self.db.status())
    }

    /// See [`crate::Database::conflicts`].
    pub fn conflicts(&self, since_ms: Option<u64>) -> Result<Vec<Conflict>> {
        self.run(self.db.conflicts(since_ms))
    }

    /// See [`crate::Database::clear_conflicts`].
    pub fn clear_conflicts(&self) -> Result<usize> {
        self.run(self.db.clear_conflicts())
    }

    /// See [`crate::Database::devices`].
    pub fn devices(&self) -> Result<Vec<DeviceStatus>> {
        self.run(self.db.devices())
    }

    /// See [`crate::Database::rename_device`].
    pub fn rename_device(&self, device: DeviceId, name: &str) -> Result<()> {
        self.run(self.db.rename_device(device, name))
    }

    /// See [`crate::Database::prune_device`].
    pub fn prune_device(&self, device: DeviceId) -> Result<Vec<String>> {
        self.run(self.db.prune_device(device))
    }

    /// See [`crate::Database::collections`].
    pub fn collections(&self) -> Result<Vec<CollectionName>> {
        self.run(self.db.collections())
    }

    /// See [`crate::Database::canonical_dump`].
    pub fn canonical_dump(&self) -> Result<Vec<u8>> {
        self.run(self.db.canonical_dump())
    }

    /// Change events; use `blocking_recv` on the receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<ChangeEvent> {
        self.db.subscribe()
    }

    /// A collection handle.
    pub fn collection(&self, name: &str) -> Result<Collection> {
        Ok(Collection {
            rt: self.rt.clone(),
            inner: self.db.collection(name)?,
        })
    }

    /// Starts background sync on the database's own runtime.
    pub fn start_auto_sync(&self, options: AutoSync) -> SyncHandle {
        let _enter = self.rt.enter();
        SyncHandle {
            rt: self.rt.clone(),
            inner: Some(self.db.start_auto_sync(options)),
        }
    }
}

/// Synchronous counterpart of [`crate::Collection`].
#[derive(Clone, Debug)]
pub struct Collection {
    rt: Arc<Runtime>,
    inner: crate::Collection,
}

impl Collection {
    fn run<T>(&self, f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        outside_runtime()?;
        self.rt.block_on(f)
    }

    /// See [`crate::Collection::put`].
    pub fn put(&self, id: &str, doc: impl IntoDocument) -> Result<bool> {
        self.run(self.inner.put(id, doc))
    }

    /// See [`crate::Collection::patch`].
    pub fn patch(&self, id: &str, set: impl IntoDocument, unset: &[&str]) -> Result<bool> {
        self.run(self.inner.patch(id, set, unset))
    }

    /// See [`crate::Collection::delete`].
    pub fn delete(&self, id: &str) -> Result<bool> {
        self.run(self.inner.delete(id))
    }

    /// See [`crate::Collection::get`].
    pub fn get(&self, id: &str) -> Result<Option<Value>> {
        self.run(self.inner.get(id))
    }

    /// See [`crate::Collection::get_with_meta`].
    pub fn get_with_meta(&self, id: &str) -> Result<DocState> {
        self.run(self.inner.get_with_meta(id))
    }

    /// See [`crate::Collection::conflicts`].
    pub fn conflicts(&self, id: &str) -> Result<Vec<Conflict>> {
        self.run(self.inner.conflicts(id))
    }

    /// See [`crate::Collection::list`].
    pub fn list(&self, options: ListOptions) -> Result<Vec<(DocId, Value)>> {
        self.run(self.inner.list(options))
    }
}

/// Synchronous counterpart of [`crate::SyncHandle`]. Dropping it stops background sync.
#[derive(Debug)]
pub struct SyncHandle {
    rt: Arc<Runtime>,
    inner: Option<crate::SyncHandle>,
}

impl SyncHandle {
    /// Latest background sync outcome.
    pub fn status(&self) -> SyncStatus {
        self.inner
            .as_ref()
            .map_or(SyncStatus::Idle, crate::SyncHandle::status)
    }

    /// Stops background sync and waits for a running sync to finish.
    pub fn stop(mut self) {
        if let Some(h) = self.inner.take() {
            self.rt.block_on(h.stop());
        }
    }
}
