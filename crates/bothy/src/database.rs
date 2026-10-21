//! The async public API: [`Database`] and [`Collection`].

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{Notify, broadcast};

use crate::engine::{self, AutoSync, CompactReport, SyncHandle};
use crate::error::{Error, ErrorKind, Result};
use crate::hlc::{Clock, SystemClock};
use crate::merge::DocState;
use crate::model::{CollectionName, DbId, DeviceId, DocId, FieldName, Value};
use crate::remote::RemoteStore;
use crate::store::{Conflict, Document, PackLimits, Store};

/// When to snapshot and how long to keep covered files.
#[derive(Clone, Copy, Debug)]
pub struct SnapshotPolicy {
    /// Snapshot after this many segments were uploaded or applied since the last own snapshot.
    pub every_segments: u64,
    /// Or after this many bytes of segments.
    pub every_bytes: u64,
    /// Grace period written to the manifest of a new database (days, at least 1).
    pub grace_period_days: u32,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            every_segments: 500,
            every_bytes: 50 * 1024 * 1024,
            grace_period_days: 30,
        }
    }
}

/// Settings for [`Database::open`].
#[derive(Clone)]
pub struct Config {
    pub(crate) path: PathBuf,
    pub(crate) app_id: String,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) pack: PackLimits,
    pub(crate) policy: SnapshotPolicy,
    pub(crate) device_name: Option<String>,
    pub(crate) id_seed: Option<u64>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("path", &self.path)
            .field("app_id", &self.app_id)
            .field("pack", &self.pack)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// A local database file at `path` for the application `app_id` (reverse domain name).
    pub fn new(path: impl Into<PathBuf>, app_id: &str) -> Self {
        Self {
            path: path.into(),
            app_id: app_id.to_owned(),
            clock: Arc::new(SystemClock),
            pack: PackLimits::default(),
            policy: SnapshotPolicy::default(),
            device_name: None,
            id_seed: None,
        }
    }

    /// Derives every new id (device, database, fork) from `seed` instead of the system random
    /// generator. Only for deterministic tests and simulations.
    #[doc(hidden)]
    pub fn deterministic_ids(mut self, seed: u64) -> Self {
        self.id_seed = Some(seed);
        self
    }

    /// Snapshot and compaction policy.
    pub fn snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replaces the wall clock (tests and simulations).
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Segment size limits.
    pub fn pack_limits(mut self, limits: PackLimits) -> Self {
        self.pack = limits;
        self
    }

    /// Human-readable device name written to `device.json`.
    pub fn device_name(mut self, name: &str) -> Self {
        self.device_name = Some(name.to_owned());
        self
    }
}

/// Where a change came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A write on this device.
    Local,
    /// A segment from another device.
    Remote,
}

/// A document's visible content changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEvent {
    /// Collection of the document.
    pub collection: CollectionName,
    /// The document.
    pub doc_id: DocId,
    /// Where the change came from.
    pub origin: Origin,
}

/// What one [`Database::sync`] call did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SyncReport {
    /// Local ops packed into new segments.
    pub pushed_ops: usize,
    /// Segments uploaded (including retries of earlier ones).
    pub segments_up: usize,
    /// Remote segments downloaded and applied.
    pub segments_down: usize,
    /// Ops in those segments.
    pub pulled_ops: usize,
    /// Documents whose visible content changed because of remote ops.
    pub changed_docs: usize,
    /// Snapshots downloaded (to join or to repair gaps).
    pub snapshots_down: usize,
    /// Snapshots merged into the local state.
    pub snapshots_applied: usize,
    /// Snapshots uploaded.
    pub snapshots_up: usize,
    /// Own files deleted by compaction.
    pub deleted_files: usize,
    /// Devices whose next segment is missing and not yet covered by a snapshot: `(device, seq)`.
    pub gaps: Vec<(DeviceId, u64)>,
    /// Set if this device took a new id because another install used the old one.
    pub forked_to: Option<DeviceId>,
    /// Remote files skipped because they failed validation.
    pub skipped: Vec<String>,
}

impl SyncReport {
    pub(crate) fn absorb(&mut self, o: SyncReport) {
        self.pushed_ops += o.pushed_ops;
        self.segments_up += o.segments_up;
        self.segments_down += o.segments_down;
        self.pulled_ops += o.pulled_ops;
        self.changed_docs += o.changed_docs;
        self.snapshots_down += o.snapshots_down;
        self.snapshots_applied += o.snapshots_applied;
        self.snapshots_up += o.snapshots_up;
        self.deleted_files += o.deleted_files;
        self.gaps = o.gaps;
        self.forked_to = o.forked_to.or(self.forked_to);
        self.skipped.extend(o.skipped);
    }
}

pub(crate) struct Inner {
    pub(crate) store: Mutex<Store>,
    pub(crate) remote: Arc<dyn RemoteStore>,
    pub(crate) device: RwLock<DeviceId>,
    pub(crate) app_id: String,
    pub(crate) device_name: Option<String>,
    pub(crate) pack: PackLimits,
    pub(crate) policy: SnapshotPolicy,
    pub(crate) sync_lock: tokio::sync::Mutex<()>,
    pub(crate) events: broadcast::Sender<ChangeEvent>,
    pub(crate) poke: Notify,
}

impl Inner {
    pub(crate) fn device(&self) -> DeviceId {
        *self.device.read().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set_device(&self, d: DeviceId) {
        *self.device.write().unwrap_or_else(|e| e.into_inner()) = d;
    }

    /// Runs `f` on the store on the blocking thread pool.
    pub(crate) async fn store<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut s = me
                .store
                .lock()
                .map_err(|_| Error::new(ErrorKind::Internal, "store lock poisoned"))?;
            f(&mut s)
        })
        .await
        .map_err(|e| Error::new(ErrorKind::Internal, "store task failed").with_source(e))?
    }

    pub(crate) fn emit(&self, collection: CollectionName, doc_id: DocId, origin: Origin) {
        // No receivers is fine.
        let _ = self.events.send(ChangeEvent {
            collection,
            doc_id,
            origin,
        });
    }
}

/// A local-first database that syncs through a [`RemoteStore`].
///
/// Cloning is cheap; clones share the same database.
#[derive(Clone)]
pub struct Database {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("device", &self.inner.device())
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Opens (or creates) the local database. A second open of the same file fails with
    /// `DatabaseLocked`.
    pub async fn open(config: Config, remote: Arc<dyn RemoteStore>) -> Result<Database> {
        let Config {
            path,
            app_id,
            clock,
            pack,
            policy,
            device_name,
            id_seed,
        } = config;
        let store = tokio::task::spawn_blocking(move || Store::open_with(path, clock, id_seed))
            .await
            .map_err(|e| Error::new(ErrorKind::Internal, "open task failed").with_source(e))??;
        let (events, _) = broadcast::channel(1024);
        Ok(Database {
            inner: Arc::new(Inner {
                device: RwLock::new(store.device_id()),
                store: Mutex::new(store),
                remote,
                app_id,
                device_name,
                pack,
                policy,
                sync_lock: tokio::sync::Mutex::new(()),
                events,
                poke: Notify::new(),
            }),
        })
    }

    /// This device's id.
    pub fn device_id(&self) -> DeviceId {
        self.inner.device()
    }

    /// The remote database this device is attached to, if any.
    pub async fn db_id(&self) -> Result<Option<DbId>> {
        self.inner.store(|s| s.db_id()).await
    }

    /// Creates a new remote database and attaches this device to it.
    pub async fn init(&self) -> Result<DbId> {
        engine::init(&self.inner).await
    }

    /// Attaches this device to an existing remote database and pulls its data, starting from
    /// the most complete snapshot if there is one.
    pub async fn join(&self, db_id: DbId) -> Result<SyncReport> {
        engine::join(&self.inner, db_id).await
    }

    /// Attaches to the oldest remote database of this app, or creates one if there is none.
    /// Does nothing if already attached.
    pub async fn init_or_join(&self) -> Result<DbId> {
        engine::init_or_join(&self.inner).await
    }

    /// A handle to a collection. Names must match `[a-z0-9_]{1,64}`.
    pub fn collection(&self, name: &str) -> Result<Collection> {
        Ok(Collection {
            db: self.clone(),
            name: CollectionName::new(name)?,
        })
    }

    /// Pushes local changes and pulls remote ones. Only one sync runs at a time.
    pub async fn sync(&self) -> Result<SyncReport> {
        engine::sync(&self.inner).await
    }

    /// A stream of document changes, local and remote.
    pub fn subscribe(&self) -> broadcast::Receiver<ChangeEvent> {
        self.inner.events.subscribe()
    }

    /// Starts background sync: soon after local writes, and periodically to pull.
    /// Stops when the returned handle is stopped or dropped. Needs a Tokio runtime.
    pub fn start_auto_sync(&self, options: AutoSync) -> SyncHandle {
        engine::start_auto_sync(self.inner.clone(), options)
    }

    /// Uploads a snapshot of the local state now. Returns its key.
    pub async fn snapshot(&self) -> Result<String> {
        let db_id = self.attached().await?;
        let _sync = self.inner.sync_lock.lock().await;
        engine::create_snapshot(&self.inner, db_id).await
    }

    /// Deletes this device's files that a snapshot older than the grace period covers.
    /// With `dry_run`, only reports what would be deleted.
    pub async fn compact(&self, dry_run: bool) -> Result<CompactReport> {
        let db_id = self.attached().await?;
        let _sync = self.inner.sync_lock.lock().await;
        engine::compact(&self.inner, db_id, dry_run).await
    }

    async fn attached(&self) -> Result<DbId> {
        self.db_id().await?.ok_or_else(|| {
            Error::invalid_input("database is not initialised; call init or join first")
        })
    }

    /// Canonical dump of the full merge state (identical on devices that applied the same ops).
    pub async fn canonical_dump(&self) -> Result<Vec<u8>> {
        self.inner.store(|s| s.canonical_dump()).await
    }
}

/// Types that can be written as a whole document.
pub trait IntoDocument {
    /// Converts to document fields.
    fn into_document(self) -> Result<Document>;
}

impl IntoDocument for Document {
    fn into_document(self) -> Result<Document> {
        Ok(self)
    }
}

impl IntoDocument for Value {
    fn into_document(self) -> Result<Document> {
        match self {
            Value::Map(m) => m
                .into_iter()
                .map(|(k, v)| Ok((FieldName::new(&k)?, v)))
                .collect(),
            _ => Err(Error::invalid_input("a document must be a map")),
        }
    }
}

impl IntoDocument for serde_json::Value {
    fn into_document(self) -> Result<Document> {
        if !self.is_object() {
            return Err(Error::invalid_input("a document must be a JSON object"));
        }
        Value::from_json(self)?.into_document()
    }
}

/// Options for [`Collection::list`].
#[derive(Clone, Debug)]
pub struct ListOptions {
    limit: usize,
    after: Option<String>,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            limit: 100,
            after: None,
        }
    }
}

impl ListOptions {
    /// Maximum number of documents.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Start after this document id (for paging).
    pub fn after(mut self, id: &str) -> Self {
        self.after = Some(id.to_owned());
        self
    }
}

/// A collection of documents in a [`Database`].
#[derive(Clone, Debug)]
pub struct Collection {
    db: Database,
    name: CollectionName,
}

fn doc_value(doc: Document) -> Value {
    Value::Map(doc.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

impl Collection {
    /// The collection name.
    pub fn name(&self) -> &CollectionName {
        &self.name
    }

    async fn write(
        &self,
        id: &str,
        f: impl FnOnce(&mut Store, &CollectionName, &DocId) -> Result<bool> + Send + 'static,
    ) -> Result<bool> {
        let id = DocId::new(id)?;
        let name = self.name.clone();
        let (n, i) = (name.clone(), id.clone());
        let changed = self.db.inner.store(move |s| f(s, &n, &i)).await?;
        if changed {
            self.db.inner.emit(name, id, Origin::Local);
            self.db.inner.poke.notify_one();
        }
        Ok(changed)
    }

    /// Writes a whole document; fields not in `doc` are removed. Returns whether anything changed.
    pub async fn put(&self, id: &str, doc: impl IntoDocument) -> Result<bool> {
        let doc = doc.into_document()?;
        self.write(id, move |s, c, d| s.put(c, d, doc)).await
    }

    /// Sets the fields in `set` and removes the fields in `unset`, creating the document if needed.
    pub async fn patch(&self, id: &str, set: impl IntoDocument, unset: &[&str]) -> Result<bool> {
        let set = set.into_document()?;
        let unset: BTreeSet<FieldName> = unset
            .iter()
            .map(|f| FieldName::new(f))
            .collect::<Result<_>>()?;
        self.write(id, move |s, c, d| s.patch(c, d, set, unset))
            .await
    }

    /// Deletes a document. Returns `false` if it did not exist.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        self.write(id, |s, c, d| s.delete(c, d)).await
    }

    /// The document as a map value, or `None` if it does not exist.
    pub async fn get(&self, id: &str) -> Result<Option<Value>> {
        let id = DocId::new(id)?;
        let name = self.name.clone();
        Ok(self
            .db
            .inner
            .store(move |s| s.get(&name, &id))
            .await?
            .map(doc_value))
    }

    /// The document's merge state: field stamps, unset fields and the delete stamp.
    pub async fn get_with_meta(&self, id: &str) -> Result<DocState> {
        let id = DocId::new(id)?;
        let name = self.name.clone();
        self.db.inner.store(move |s| s.doc_state(&name, &id)).await
    }

    /// Writes from other devices that lost to another write on this document, newest first.
    pub async fn conflicts(&self, id: &str) -> Result<Vec<Conflict>> {
        let id = DocId::new(id)?;
        let name = self.name.clone();
        self.db.inner.store(move |s| s.conflicts(&name, &id)).await
    }

    /// Documents in id order.
    pub async fn list(&self, options: ListOptions) -> Result<Vec<(DocId, Value)>> {
        let after = options.after.as_deref().map(DocId::new).transpose()?;
        let name = self.name.clone();
        let rows = self
            .db
            .inner
            .store(move |s| s.list(&name, options.limit, after.as_ref()))
            .await?;
        Ok(rows.into_iter().map(|(id, d)| (id, doc_value(d))).collect())
    }
}
