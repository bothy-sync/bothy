//! `FolderRemote` and `Database`.

use std::sync::Arc;

use napi::tokio::sync::{OwnedRwLockReadGuard, RwLock};
use napi_derive::napi;

use crate::collection::Collection;
use crate::error::{Result, from_engine, invalid_input};
use crate::value::Str;

/// A folder shared by the devices (a local or synced directory).
#[napi]
pub struct FolderRemote {
    path: String,
}

#[napi]
impl FolderRemote {
    #[napi(constructor)]
    pub fn new(path: Str) -> Result<Self> {
        let path = path.0;
        // The engine would take "" as the working directory and sync into it silently.
        if path.is_empty() {
            return Err(invalid_input("remote folder path is empty"));
        }
        // Validates the path and creates the folder, like the engine does on open.
        bothy::remote::FolderRemote::new(&path).map_err(from_engine)?;
        Ok(Self { path })
    }

    #[napi(getter)]
    pub fn path(&self) -> String {
        self.path.clone()
    }
}

/// The engine handle shared by a `Database` and its collections.
///
/// Every operation holds a read guard while it runs. `close()` takes the write lock, so it
/// waits for operations in flight, then drops the engine and with it the local database lock.
#[derive(Clone)]
pub(crate) struct DbSlot(Arc<RwLock<Option<bothy::Database>>>);

/// An engine handle held for the duration of one operation.
pub(crate) struct InUse(OwnedRwLockReadGuard<Option<bothy::Database>>);

impl InUse {
    pub(crate) fn db(&self) -> &bothy::Database {
        self.0.as_ref().expect("checked when acquired")
    }
}

impl DbSlot {
    fn new(db: bothy::Database) -> Self {
        Self(Arc::new(RwLock::new(Some(db))))
    }

    /// The engine for one operation; fails once the database is closed.
    pub(crate) async fn acquire(&self) -> Result<InUse> {
        let guard = self.0.clone().read_owned().await;
        if guard.is_none() {
            return Err(invalid_input("database is closed"));
        }
        Ok(InUse(guard))
    }

    /// Waits for operations in flight, then releases the engine.
    async fn close(&self) {
        drop(self.0.write().await.take());
    }
}

/// What one `sync()` did (the same fields as the Python package's report).
#[napi(object)]
pub struct SyncReport {
    pub pushed_ops: u32,
    pub segments_up: u32,
    pub segments_down: u32,
    pub pulled_ops: u32,
    pub changed_docs: u32,
    pub snapshots_down: u32,
    pub snapshots_up: u32,
    pub password_changed: bool,
    pub skipped: Vec<String>,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl From<bothy::SyncReport> for SyncReport {
    fn from(r: bothy::SyncReport) -> Self {
        Self {
            pushed_ops: count(r.pushed_ops),
            segments_up: count(r.segments_up),
            segments_down: count(r.segments_down),
            pulled_ops: count(r.pulled_ops),
            changed_docs: count(r.changed_docs),
            snapshots_down: count(r.snapshots_down),
            snapshots_up: count(r.snapshots_up),
            password_changed: r.password_changed,
            skipped: r.skipped,
        }
    }
}

#[napi]
pub struct Database {
    slot: DbSlot,
}

/// Opens (or creates) the local database. The facade's `Database.open` calls this.
#[napi]
pub async fn open_database(
    path: Str,
    app_id: Str,
    remote_path: Str,
    device_name: Option<Str>,
) -> Result<Database> {
    let remote = Arc::new(bothy::remote::FolderRemote::new(&remote_path.0).map_err(from_engine)?);
    let mut config = bothy::Config::new(path.0, &app_id.0);
    if let Some(name) = &device_name {
        config = config.device_name(&name.0);
    }
    let db = bothy::Database::open(config, remote)
        .await
        .map_err(from_engine)?;
    Ok(Database {
        slot: DbSlot::new(db),
    })
}

#[napi]
impl Database {
    /// Creates the database on the remote or joins the one that is there; returns its id (hex).
    #[napi]
    pub async fn init_or_join(&self) -> Result<String> {
        let open = self.slot.acquire().await?;
        Ok(open
            .db()
            .init_or_join()
            .await
            .map_err(from_engine)?
            .to_hex())
    }

    /// Pushes local changes and pulls remote ones.
    #[napi]
    pub async fn sync(&self) -> Result<SyncReport> {
        let open = self.slot.acquire().await?;
        Ok(open.db().sync().await.map_err(from_engine)?.into())
    }

    /// A collection of documents. The name is checked now; the database may be closed later.
    #[napi]
    pub fn collection(&self, name: Str) -> Result<Collection> {
        bothy::CollectionName::new(&name.0).map_err(from_engine)?;
        Ok(Collection {
            slot: self.slot.clone(),
            name: name.0,
        })
    }

    /// Names of the collections that hold at least one visible document.
    #[napi]
    pub async fn collections(&self) -> Result<Vec<String>> {
        let open = self.slot.acquire().await?;
        let names = open.db().collections().await.map_err(from_engine)?;
        Ok(names.iter().map(|n| n.as_str().to_owned()).collect())
    }

    /// Waits for operations in flight, then releases the local database. Calling it again is
    /// fine.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.slot.close().await;
        Ok(())
    }
}
