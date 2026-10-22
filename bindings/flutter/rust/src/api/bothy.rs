//! The Dart-facing API: a thin layer over the `bothy` crate.
//!
//! Documents cross the boundary as JSON text; the Dart side encodes and decodes them. Every
//! engine call runs on this library's own Tokio runtime, whatever executor drives the bridge.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use bothy::auth::AccessTokenProvider;
use bothy::remote::drive::DriveRemote;
use bothy::remote::{FolderRemote, RemoteStore};
use bothy::{Config, Database, ErrorKind, ListOptions, Origin};
use flutter_rust_bridge::{DartFnFuture, frb};

use crate::frb_generated::StreamSink;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("bothy")
            .enable_all()
            .build()
            .expect("cannot start the bothy runtime")
    })
}

/// Runs `fut` on the bothy runtime and maps its error.
async fn run<T: Send + 'static>(
    fut: impl Future<Output = bothy::Result<T>> + Send + 'static,
) -> Result<T, BothyException> {
    match runtime().spawn(fut).await {
        Ok(r) => r.map_err(BothyException::from),
        Err(e) => Err(BothyException {
            kind: BothyErrorKind::Internal,
            message: format!("engine task failed: {e}"),
        }),
    }
}

/// Error categories, as in the Rust `ErrorKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BothyErrorKind {
    Offline,
    AuthRequired,
    RateLimited,
    QuotaExceeded,
    RemoteInconsistent,
    FormatTooNew,
    SchemaTooNew,
    WrongPassword,
    KeyRequired,
    Corrupt,
    DatabaseLocked,
    InvalidInput,
    Internal,
}

/// An error from the engine. Never contains tokens, passwords or keys.
#[derive(Clone, Debug)]
pub struct BothyException {
    pub kind: BothyErrorKind,
    pub message: String,
}

impl From<bothy::Error> for BothyException {
    fn from(e: bothy::Error) -> Self {
        use BothyErrorKind as K;
        let kind = match e.kind() {
            ErrorKind::Offline => K::Offline,
            ErrorKind::AuthRequired => K::AuthRequired,
            ErrorKind::RateLimited => K::RateLimited,
            ErrorKind::QuotaExceeded => K::QuotaExceeded,
            ErrorKind::RemoteInconsistent => K::RemoteInconsistent,
            ErrorKind::FormatTooNew => K::FormatTooNew,
            ErrorKind::SchemaTooNew => K::SchemaTooNew,
            ErrorKind::WrongPassword => K::WrongPassword,
            ErrorKind::KeyRequired => K::KeyRequired,
            ErrorKind::Corrupt => K::Corrupt,
            ErrorKind::DatabaseLocked => K::DatabaseLocked,
            ErrorKind::InvalidInput => K::InvalidInput,
            _ => K::Internal,
        };
        Self {
            kind,
            message: e.to_string(),
        }
    }
}

fn invalid(message: impl Into<String>) -> BothyException {
    BothyException {
        kind: BothyErrorKind::InvalidInput,
        message: message.into(),
    }
}

/// How to open a local database.
pub struct BothyOptions {
    /// Path of the local SQLite file.
    pub path: String,
    /// Application id (reverse domain name), stored in the manifest.
    pub app_id: String,
    /// Human-readable device name shown to other devices.
    pub device_name: Option<String>,
    /// Encrypt a database this device creates, with `password`.
    pub encrypt: bool,
    /// Password to create, join or unlock an encrypted database.
    pub password: Option<String>,
    /// A data key the app kept from `export_key`, instead of the password.
    pub key: Option<Vec<u8>>,
    /// Conflict records to keep; `None` turns the conflict log off.
    pub conflict_log_limit: Option<u32>,
}

/// Kinds of remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BothyRemoteKind {
    /// A local directory (tests, shared folders).
    Path,
    /// The app's hidden folder in Google Drive (`drive.appdata` scope).
    GoogleDriveAppData,
    /// A visible folder `Bothy/<name>` in Google Drive (`drive.file` scope).
    GoogleDriveFolder,
}

/// Where the shared files live.
pub struct BothyRemote {
    pub kind: BothyRemoteKind,
    /// The directory for `Path`, the folder name for `GoogleDriveFolder`.
    pub location: Option<String>,
}

/// Access tokens from the app. The callback gets `true` when the last token was rejected and
/// must be refreshed; it returns an empty string when the user is not signed in.
struct DartTokens {
    get: Box<dyn Fn(bool) -> DartFnFuture<String> + Send + Sync>,
    stale: AtomicBool,
}

#[async_trait::async_trait]
impl AccessTokenProvider for DartTokens {
    async fn access_token(&self) -> bothy::Result<String> {
        let refresh = self.stale.swap(false, Ordering::SeqCst);
        let token = (self.get)(refresh).await;
        if token.is_empty() {
            return Err(bothy::Error::new(
                ErrorKind::AuthRequired,
                "the app has no Google sign-in",
            ));
        }
        Ok(token)
    }

    async fn invalidate(&self, _rejected: &str) {
        self.stale.store(true, Ordering::SeqCst);
    }
}

/// A document id and its JSON.
pub struct BothyDoc {
    pub id: String,
    pub json: String,
}

/// A document changed, here or on another device.
pub struct BothyChange {
    pub collection: String,
    pub id: String,
    pub remote: bool,
}

/// What one sync did.
pub struct BothySyncReport {
    pub pushed_ops: u32,
    pub segments_up: u32,
    pub segments_down: u32,
    pub pulled_ops: u32,
    pub changed_docs: u32,
    pub password_changed: bool,
    pub skipped: Vec<String>,
}

impl From<bothy::SyncReport> for BothySyncReport {
    fn from(r: bothy::SyncReport) -> Self {
        let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Self {
            pushed_ops: n(r.pushed_ops),
            segments_up: n(r.segments_up),
            segments_down: n(r.segments_down),
            pulled_ops: n(r.pulled_ops),
            changed_docs: n(r.changed_docs),
            password_changed: r.password_changed,
            skipped: r.skipped,
        }
    }
}

/// The outcome of a background sync: a report, or an error.
pub struct BothySyncEvent {
    pub report: Option<BothySyncReport>,
    pub error: Option<BothyException>,
}

/// Local counters.
pub struct BothyStatus {
    pub documents: i64,
    pub tombstones: i64,
    pub unsynced_ops: i64,
    pub pending_uploads: i64,
    /// Milliseconds since the epoch.
    pub last_sync_ms: Option<i64>,
    pub encrypted: bool,
    pub locked: bool,
}

/// A write from another device that lost to a newer write.
pub struct BothyConflict {
    pub collection: String,
    pub id: String,
    pub field: String,
    /// When this device recorded it, in milliseconds since the epoch.
    pub at_ms: i64,
    pub winner_device: String,
    pub loser_device: String,
    /// The losing value as JSON; `None` if the losing write removed the field.
    pub loser_value_json: Option<String>,
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn parse_json(json: &str) -> Result<serde_json::Value, BothyException> {
    serde_json::from_str(json).map_err(|e| invalid(format!("not valid JSON: {e}")))
}

/// An open database.
#[frb(opaque)]
pub struct BothyDatabase {
    db: Database,
    auto_sync: Mutex<Option<bothy::SyncHandle>>,
    /// Set by `close`; ends the `watch` streams.
    closed: tokio::sync::watch::Sender<bool>,
}

impl BothyDatabase {
    /// Opens (or creates) the local database. `get_token` is only called for Google Drive.
    pub async fn open(
        options: BothyOptions,
        remote: BothyRemote,
        get_token: impl Fn(bool) -> DartFnFuture<String> + Send + Sync + 'static,
    ) -> Result<BothyDatabase, BothyException> {
        let location = || {
            remote
                .location
                .clone()
                .ok_or_else(|| invalid("this remote needs a location"))
        };
        let tokens = || {
            Arc::new(DartTokens {
                get: Box::new(get_token),
                stale: AtomicBool::new(false),
            })
        };
        let remote: Arc<dyn RemoteStore> = match remote.kind {
            BothyRemoteKind::Path => Arc::new(FolderRemote::new(location()?)?),
            BothyRemoteKind::GoogleDriveAppData => Arc::new(DriveRemote::appdata(tokens())?),
            BothyRemoteKind::GoogleDriveFolder => {
                Arc::new(DriveRemote::folder(tokens(), &location()?)?)
            }
        };
        let mut config = Config::new(&options.path, &options.app_id)
            .conflict_log(options.conflict_log_limit);
        if let Some(name) = &options.device_name {
            config = config.device_name(name);
        }
        match (&options.password, &options.key) {
            (Some(p), _) if options.encrypt => config = config.encrypt(p),
            (Some(p), _) => config = config.password(p),
            (None, Some(k)) => config = config.key(data_key(k)?),
            (None, None) if options.encrypt => {
                return Err(invalid("an encrypted database needs a password"));
            }
            (None, None) => {}
        }
        let db = run(async move { Database::open(config, remote).await }).await?;
        Ok(BothyDatabase {
            db,
            auto_sync: Mutex::new(None),
            closed: tokio::sync::watch::channel(false).0,
        })
    }

    /// Attaches to the oldest database of this app on the remote, or creates one. Returns its id.
    pub async fn init_or_join(&self) -> Result<String, BothyException> {
        let db = self.db.clone();
        Ok(run(async move { db.init_or_join().await }).await?.to_hex())
    }

    /// The remote database id, if attached.
    pub async fn db_id(&self) -> Result<Option<String>, BothyException> {
        let db = self.db.clone();
        Ok(run(async move { db.db_id().await }).await?.map(|d| d.to_hex()))
    }

    /// This device's id.
    #[frb(sync)]
    pub fn device_id(&self) -> String {
        self.db.device_id().to_hex()
    }

    pub async fn is_encrypted(&self) -> Result<bool, BothyException> {
        let db = self.db.clone();
        run(async move { db.is_encrypted().await }).await
    }

    pub async fn is_locked(&self) -> Result<bool, BothyException> {
        let db = self.db.clone();
        run(async move { db.is_locked().await }).await
    }

    /// Unlocks with the password. Fails with `WrongPassword`.
    pub async fn unlock(&self, password: String) -> Result<(), BothyException> {
        let db = self.db.clone();
        let password = zeroize::Zeroizing::new(password);
        run(async move { db.unlock(&password).await }).await
    }

    /// Unlocks with a data key from `export_key`.
    pub async fn unlock_with_key(&self, key: Vec<u8>) -> Result<(), BothyException> {
        let db = self.db.clone();
        let key = data_key(&key)?;
        run(async move { db.unlock_with_key(key).await }).await
    }

    /// Forgets the data key in memory. Local reads and writes keep working; sync needs a new
    /// unlock.
    #[frb(sync)]
    pub fn lock(&self) {
        self.db.lock();
    }

    /// The 32-byte data key, for the app to keep in secure storage.
    #[frb(sync)]
    pub fn export_key(&self) -> Result<Vec<u8>, BothyException> {
        Ok(self.db.export_key()?.to_bytes().to_vec())
    }

    /// Changes the password on every device. Kept data keys stay valid.
    pub async fn change_password(&self, old: String, new: String) -> Result<(), BothyException> {
        let db = self.db.clone();
        let (old, new) = (zeroize::Zeroizing::new(old), zeroize::Zeroizing::new(new));
        run(async move { db.change_password(&old, &new).await }).await
    }

    /// Replaces a document. Returns whether anything changed.
    pub async fn put(
        &self,
        collection: String,
        id: String,
        json: String,
    ) -> Result<bool, BothyException> {
        let doc = parse_json(&json)?;
        let c = self.db.collection(&collection)?;
        run(async move { c.put(&id, doc).await }).await
    }

    /// Sets the given fields and removes `unset`.
    pub async fn patch(
        &self,
        collection: String,
        id: String,
        json: String,
        unset: Vec<String>,
    ) -> Result<bool, BothyException> {
        let doc = parse_json(&json)?;
        let c = self.db.collection(&collection)?;
        run(async move {
            let unset: Vec<&str> = unset.iter().map(String::as_str).collect();
            c.patch(&id, doc, &unset).await
        })
        .await
    }

    /// The document as JSON, or `None`.
    pub async fn get(&self, collection: String, id: String) -> Result<Option<String>, BothyException> {
        let c = self.db.collection(&collection)?;
        let doc = run(async move { c.get(&id).await }).await?;
        Ok(doc.map(|v| v.to_json().to_string()))
    }

    /// Documents in id order, after `after`.
    pub async fn list(
        &self,
        collection: String,
        limit: u32,
        after: Option<String>,
    ) -> Result<Vec<BothyDoc>, BothyException> {
        let c = self.db.collection(&collection)?;
        let rows = run(async move {
            let mut opts = ListOptions::default().limit(limit as usize);
            if let Some(a) = &after {
                opts = opts.after(a);
            }
            c.list(opts).await
        })
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, v)| BothyDoc {
                id: id.as_str().to_owned(),
                json: v.to_json().to_string(),
            })
            .collect())
    }

    /// Deletes a document. Returns whether it existed.
    pub async fn delete(&self, collection: String, id: String) -> Result<bool, BothyException> {
        let c = self.db.collection(&collection)?;
        run(async move { c.delete(&id).await }).await
    }

    /// Every change, local or remote, until `close`.
    pub fn watch(&self, sink: StreamSink<BothyChange>) {
        let mut events = self.db.subscribe();
        let mut closed = self.closed.subscribe();
        runtime().spawn(async move {
            loop {
                let next = tokio::select! {
                    e = events.recv() => e,
                    _ = closed.wait_for(|c| *c) => break,
                };
                match next {
                    Ok(e) => {
                        let change = BothyChange {
                            collection: e.collection.as_str().to_owned(),
                            id: e.doc_id.as_str().to_owned(),
                            remote: e.origin == Origin::Remote,
                        };
                        if sink.add(change).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// Pushes local changes and pulls remote ones.
    pub async fn sync(&self) -> Result<BothySyncReport, BothyException> {
        let db = self.db.clone();
        Ok(run(async move { db.sync().await }).await?.into())
    }

    /// Syncs in the background (after writes, and every few minutes) until
    /// `stop_auto_sync`. Each outcome goes to `sink`.
    pub fn start_auto_sync(&self, sink: StreamSink<BothySyncEvent>) {
        let _guard = runtime().enter();
        let handle = self.db.start_auto_sync(bothy::AutoSync::default());
        let mut status = handle.watch();
        runtime().spawn(async move {
            while status.changed().await.is_ok() {
                let event = match status.borrow().clone() {
                    bothy::SyncStatus::Ok(r) => BothySyncEvent {
                        report: Some(r.into()),
                        error: None,
                    },
                    bothy::SyncStatus::Failed { kind, message } => BothySyncEvent {
                        report: None,
                        error: Some(BothyException {
                            kind: BothyException::from(bothy::Error::new(kind, "")).kind,
                            message,
                        }),
                    },
                    _ => continue,
                };
                if sink.add(event).is_err() {
                    break;
                }
            }
        });
        if let Some(old) = self
            .auto_sync
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(handle)
        {
            runtime().spawn(old.stop());
        }
    }

    /// Stops background sync and ends the `watch` streams. The database stays usable.
    pub async fn close(&self) {
        self.stop_auto_sync().await;
        self.closed.send_replace(true);
    }

    /// Stops background sync.
    pub async fn stop_auto_sync(&self) {
        let handle = self
            .auto_sync
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(h) = handle {
            let _ = runtime().spawn(h.stop()).await;
        }
    }

    /// Local counters and the lock state.
    pub async fn status(&self) -> Result<BothyStatus, BothyException> {
        let db = self.db.clone();
        run(async move {
            let st = db.status().await?;
            Ok(BothyStatus {
                documents: to_i64(st.documents),
                tombstones: to_i64(st.tombstones),
                unsynced_ops: to_i64(st.unpacked_ops),
                pending_uploads: to_i64(st.pending_uploads),
                last_sync_ms: st.last_sync_ms.map(to_i64),
                encrypted: db.is_encrypted().await?,
                locked: db.is_locked().await?,
            })
        })
        .await
    }

    /// Conflicts across the database, newest first; with `since_ms`, only newer ones.
    pub async fn conflicts(&self, since_ms: Option<i64>) -> Result<Vec<BothyConflict>, BothyException> {
        let db = self.db.clone();
        let since = since_ms.map(|s| s.max(0) as u64);
        let rows = run(async move { db.conflicts(since).await }).await?;
        Ok(rows
            .into_iter()
            .map(|c| BothyConflict {
                collection: c.collection.as_str().to_owned(),
                id: c.doc_id.as_str().to_owned(),
                field: c.field.to_string(),
                at_ms: to_i64(c.at_ms),
                winner_device: c.winner.device.to_hex(),
                loser_device: c.loser.device.to_hex(),
                loser_value_json: c.loser_value.map(|v| v.to_json().to_string()),
            })
            .collect())
    }

    /// Deletes every conflict record. Returns how many there were.
    pub async fn clear_conflicts(&self) -> Result<u32, BothyException> {
        let db = self.db.clone();
        let n = run(async move { db.clear_conflicts().await }).await?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }
}

fn data_key(bytes: &[u8]) -> Result<bothy::crypto::DataKey, BothyException> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| invalid("a data key has 32 bytes"))?;
    Ok(bothy::crypto::DataKey::from_bytes(arr))
}
