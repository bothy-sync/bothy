//! Native module of the `bothy` Python package (`bothy._native`).
//!
//! A thin layer over `bothy::blocking`: every call releases the GIL while the engine works.
//! Documents cross as JSON text; the Python side (`bothy/__init__.py`) encodes and decodes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bothy::auth::AccessTokenProvider;
use bothy::blocking;
use bothy::remote::drive::DriveRemote;
use bothy::remote::{FolderRemote, RemoteStore};
use bothy::{Config, ErrorKind, ListOptions, Origin};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;

create_exception!(bothy, BothyError, PyException, "Any error from the engine.");
create_exception!(
    bothy,
    OfflineError,
    BothyError,
    "The remote is unreachable."
);
create_exception!(bothy, AuthRequiredError, BothyError, "Sign in again.");
create_exception!(
    bothy,
    RateLimitedError,
    BothyError,
    "The remote asked to slow down."
);
create_exception!(
    bothy,
    QuotaExceededError,
    BothyError,
    "The remote storage is full."
);
create_exception!(
    bothy,
    RemoteInconsistentError,
    BothyError,
    "Remote files disagree."
);
create_exception!(
    bothy,
    FormatTooNewError,
    BothyError,
    "Written by a newer version."
);
create_exception!(
    bothy,
    SchemaTooNewError,
    BothyError,
    "Local database is newer."
);
create_exception!(
    bothy,
    WrongPasswordError,
    BothyError,
    "The password or key is wrong."
);
create_exception!(
    bothy,
    KeyRequiredError,
    BothyError,
    "The database is encrypted and locked."
);
create_exception!(bothy, CorruptError, BothyError, "A file failed validation.");
create_exception!(
    bothy,
    DatabaseLockedError,
    BothyError,
    "Another process has it open."
);
create_exception!(
    bothy,
    InvalidInputError,
    BothyError,
    "A name, id or value is invalid."
);
create_exception!(
    bothy,
    InternalError,
    BothyError,
    "A bug or an unexpected failure."
);

/// Maps an engine error to its Python exception. Messages never contain tokens or keys.
fn err(e: bothy::Error) -> PyErr {
    let m = e.to_string();
    match e.kind() {
        ErrorKind::Offline => OfflineError::new_err(m),
        ErrorKind::AuthRequired => AuthRequiredError::new_err(m),
        ErrorKind::RateLimited => RateLimitedError::new_err(m),
        ErrorKind::QuotaExceeded => QuotaExceededError::new_err(m),
        ErrorKind::RemoteInconsistent => RemoteInconsistentError::new_err(m),
        ErrorKind::FormatTooNew => FormatTooNewError::new_err(m),
        ErrorKind::SchemaTooNew => SchemaTooNewError::new_err(m),
        ErrorKind::WrongPassword => WrongPasswordError::new_err(m),
        ErrorKind::KeyRequired => KeyRequiredError::new_err(m),
        ErrorKind::Corrupt => CorruptError::new_err(m),
        ErrorKind::DatabaseLocked => DatabaseLockedError::new_err(m),
        ErrorKind::InvalidInput => InvalidInputError::new_err(m),
        _ => InternalError::new_err(m),
    }
}

fn invalid(m: impl Into<String>) -> PyErr {
    InvalidInputError::new_err(m.into())
}

/// A runtime for the OAuth flows (the database has its own, inside `bothy::blocking`).
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("bothy-auth")
            .enable_all()
            .build()
            .expect("cannot start the bothy runtime")
    })
}

/// Tokens from a Python callable `(force_refresh: bool) -> str`.
struct PyTokens {
    get: Py<PyAny>,
    stale: AtomicBool,
}

#[async_trait::async_trait]
impl AccessTokenProvider for PyTokens {
    async fn access_token(&self) -> bothy::Result<String> {
        let refresh = self.stale.swap(false, Ordering::SeqCst);
        let get = Python::attach(|py| self.get.clone_ref(py));
        let token = tokio::task::spawn_blocking(move || {
            Python::attach(|py| get.call1(py, (refresh,))?.extract::<String>(py))
        })
        .await
        .map_err(|_| bothy::Error::new(ErrorKind::Internal, "token callback task failed"))?;
        match token {
            Ok(t) if !t.is_empty() => Ok(t),
            // The exception may describe the user's setup; don't log it, only report it.
            _ => Err(bothy::Error::new(
                ErrorKind::AuthRequired,
                "the token callback returned no token",
            )),
        }
    }

    async fn invalidate(&self, _rejected: &str) {
        self.stale.store(true, Ordering::SeqCst);
    }
}

/// Access tokens for Google Drive: a stored grant (from `device_login` or `loopback_login`)
/// refreshed through its OAuth client.
#[pyclass(frozen, module = "bothy._native")]
struct TokenProvider {
    inner: Arc<dyn AccessTokenProvider>,
}

#[pymethods]
impl TokenProvider {
    /// Fresh tokens from `grant`, issued by the client in `client_file` (`None`: Bothy's
    /// official client of the grant's kind).
    #[new]
    fn new(client_file: Option<&str>, grant: &Grant) -> PyResult<Self> {
        let client = client_or_official(client_file, &grant.inner.client)?;
        let tokens = bothy_oauth::RefreshingTokens::new(client, &grant.inner).map_err(err)?;
        Ok(Self {
            inner: Arc::new(tokens),
        })
    }

    /// A current access token (for calling other Google APIs).
    fn access_token(&self, py: Python<'_>) -> PyResult<String> {
        let inner = self.inner.clone();
        py.detach(|| runtime().block_on(inner.access_token()))
            .map_err(err)
    }
}

fn token_provider(py: Python<'_>, tokens: Py<PyAny>) -> PyResult<Arc<dyn AccessTokenProvider>> {
    if let Ok(p) = tokens.bind(py).cast::<TokenProvider>() {
        return Ok(p.get().inner.clone());
    }
    if !tokens.bind(py).is_callable() {
        return Err(invalid(
            "tokens must be a TokenProvider or a callable (force_refresh) -> str",
        ));
    }
    Ok(Arc::new(PyTokens {
        get: tokens,
        stale: AtomicBool::new(false),
    }))
}

/// Where a database's shared files live.
#[pyclass(frozen, module = "bothy._native")]
struct Remote {
    inner: Arc<dyn RemoteStore>,
    description: String,
}

#[pymethods]
impl Remote {
    /// A local or shared directory.
    #[staticmethod]
    fn folder(path: &str) -> PyResult<Self> {
        Ok(Self {
            inner: Arc::new(FolderRemote::new(path).map_err(err)?),
            description: format!("folder {path}"),
        })
    }

    /// The app's hidden folder in Google Drive (`drive.appdata` scope).
    #[staticmethod]
    fn drive_appdata(py: Python<'_>, tokens: Py<PyAny>) -> PyResult<Self> {
        let tokens = token_provider(py, tokens)?;
        Ok(Self {
            inner: Arc::new(DriveRemote::appdata(tokens).map_err(err)?),
            description: "Google Drive app folder".into(),
        })
    }

    /// A visible folder `Bothy/<name>` in Google Drive (`drive.file` scope; works with the
    /// device flow).
    #[staticmethod]
    fn drive_folder(py: Python<'_>, name: &str, tokens: Py<PyAny>) -> PyResult<Self> {
        let tokens = token_provider(py, tokens)?;
        Ok(Self {
            inner: Arc::new(DriveRemote::folder(tokens, name).map_err(err)?),
            description: format!("Google Drive folder Bothy/{name}"),
        })
    }

    fn __repr__(&self) -> String {
        format!("<Remote {}>", self.description)
    }
}

/// The client in `client_file`, or Bothy's official client of `kind` (`desktop` or `device`).
fn client_or_official(client_file: Option<&str>, kind: &str) -> PyResult<bothy_oauth::Client> {
    match client_file {
        Some(file) => bothy_oauth::Client::from_file(file).map_err(err),
        None => bothy_oauth::official::client(kind).ok_or_else(|| {
            invalid(
                "this build has no official Bothy OAuth client; pass the client JSON file of your own Google Cloud project",
            )
        }),
    }
}

/// A Google sign-in: the long-lived grant. Its repr never shows the token.
#[pyclass(frozen, module = "bothy._native")]
struct Grant {
    inner: bothy_oauth::Grant,
}

#[pymethods]
impl Grant {
    /// Scopes the user granted.
    #[getter]
    fn scopes(&self) -> Vec<String> {
        self.inner.scopes.clone()
    }

    /// Saves the grant to `path`, readable only by the user.
    fn save(&self, path: &str) -> PyResult<()> {
        bothy_oauth::TokenStore::file_only(path)
            .save(&self.inner)
            .map(|_| ())
            .map_err(err)
    }

    /// Loads a grant saved with `save`, or `None`.
    #[staticmethod]
    fn load(path: &str) -> Option<Self> {
        bothy_oauth::TokenStore::file_only(path)
            .load()
            .map(|inner| Self { inner })
    }

    fn __repr__(&self) -> String {
        format!("<Grant scopes={:?}>", self.inner.scopes)
    }
}

/// Device sign-in: calls `show(verification_url, user_code)`, then waits until the user
/// approves on another device. Grants only `drive.file` (Google rejects `drive.appdata` here).
#[pyfunction]
#[pyo3(signature = (client_file, show))]
fn device_login(py: Python<'_>, client_file: Option<&str>, show: Py<PyAny>) -> PyResult<Grant> {
    let client = client_or_official(client_file, "device")?;
    let shown: Arc<Mutex<Option<PyErr>>> = Arc::default();
    let s = shown.clone();
    let grant = py.detach(|| {
        runtime().block_on(bothy_oauth::device_login(&client, move |code| {
            Python::attach(|py| {
                if let Err(e) = show.call1(py, (&code.verification_url, &code.user_code)) {
                    *s.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                }
            });
        }))
    });
    if let Some(e) = shown.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(e);
    }
    Ok(Grant {
        inner: grant.map_err(err)?,
    })
}

/// Browser sign-in: calls `show(url)` (open it in a browser), then waits for the loopback
/// redirect. `scopes` defaults to `drive.appdata` and `drive.file`.
#[pyfunction]
#[pyo3(signature = (client_file, show, scopes = None))]
fn loopback_login(
    py: Python<'_>,
    client_file: Option<&str>,
    show: Py<PyAny>,
    scopes: Option<Vec<String>>,
) -> PyResult<Grant> {
    let client = client_or_official(client_file, "desktop")?;
    let scopes = scopes.unwrap_or_else(|| {
        vec![
            bothy_oauth::SCOPE_APPDATA.into(),
            bothy_oauth::SCOPE_FILE.into(),
        ]
    });
    let grant = py.detach(|| {
        let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
        runtime().block_on(bothy_oauth::loopback_login(&client, &scopes, move |url| {
            Python::attach(|py| {
                let _ = show.call1(py, (url,));
            });
        }))
    });
    Ok(Grant {
        inner: grant.map_err(err)?,
    })
}

/// A blocking stream of change events.
#[pyclass(module = "bothy._native")]
struct Changes {
    rx: Mutex<tokio::sync::broadcast::Receiver<bothy::ChangeEvent>>,
    rt: tokio::runtime::Runtime,
}

#[pymethods]
impl Changes {
    /// The next change `(collection, id, remote)`, or `None` after `timeout` seconds
    /// (`None` waits forever). Raises `BothyError` once the database is closed.
    #[pyo3(signature = (timeout = None))]
    fn next(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<Option<(String, String, bool)>> {
        use tokio::sync::broadcast::error::RecvError;
        py.detach(|| {
            let mut rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                // Timers must be created inside the runtime.
                let got = match timeout {
                    Some(t) => match self.rt.block_on(async {
                        tokio::time::timeout(Duration::from_secs_f64(t.max(0.0)), rx.recv()).await
                    }) {
                        Ok(r) => r,
                        Err(_) => return Ok(None),
                    },
                    None => self.rt.block_on(rx.recv()),
                };
                match got {
                    Ok(e) => {
                        return Ok(Some((
                            e.collection.as_str().to_owned(),
                            e.doc_id.as_str().to_owned(),
                            e.origin == Origin::Remote,
                        )));
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => {
                        return Err(BothyError::new_err("the database was closed"));
                    }
                }
            }
        })
    }
}

/// An open database. Use `bothy.Database`, which wraps this with JSON conversion.
#[pyclass(module = "bothy._native")]
struct Database {
    db: Option<blocking::Database>,
    auto_sync: Option<blocking::SyncHandle>,
}

impl Database {
    fn db(&self) -> PyResult<&blocking::Database> {
        self.db
            .as_ref()
            .ok_or_else(|| BothyError::new_err("the database is closed"))
    }

    fn collection(&self, name: &str) -> PyResult<blocking::Collection> {
        self.db()?.collection(name).map_err(err)
    }
}

fn parse(json: &str) -> PyResult<serde_json::Value> {
    serde_json::from_str(json).map_err(|e| invalid(format!("not valid JSON: {e}")))
}

fn data_key(bytes: &[u8]) -> PyResult<bothy::crypto::DataKey> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| invalid("a data key has 32 bytes"))?;
    Ok(bothy::crypto::DataKey::from_bytes(arr))
}

fn report(py: Python<'_>, r: &bothy::SyncReport) -> PyResult<Py<PyAny>> {
    let d = pyo3::types::PyDict::new(py);
    d.set_item("pushed_ops", r.pushed_ops)?;
    d.set_item("segments_up", r.segments_up)?;
    d.set_item("segments_down", r.segments_down)?;
    d.set_item("pulled_ops", r.pulled_ops)?;
    d.set_item("changed_docs", r.changed_docs)?;
    d.set_item("snapshots_down", r.snapshots_down)?;
    d.set_item("snapshots_up", r.snapshots_up)?;
    d.set_item("password_changed", r.password_changed)?;
    d.set_item("skipped", r.skipped.clone())?;
    Ok(d.into_any().unbind())
}

#[pymethods]
impl Database {
    #[staticmethod]
    #[pyo3(signature = (path, app_id, remote, *, encrypt = false, password = None, key = None,
                        device_name = None, conflict_log_limit = Some(1000), p2p = false,
                        stun = None, p2p_host = None))]
    #[allow(clippy::too_many_arguments)]
    fn open(
        py: Python<'_>,
        path: &str,
        app_id: &str,
        remote: &Remote,
        encrypt: bool,
        password: Option<String>,
        key: Option<Vec<u8>>,
        device_name: Option<String>,
        conflict_log_limit: Option<u32>,
        p2p: bool,
        stun: Option<Vec<String>>,
        p2p_host: Option<String>,
    ) -> PyResult<Self> {
        let mut config = Config::new(path, app_id).conflict_log(conflict_log_limit);
        if p2p {
            let mut p = bothy::p2p::P2pConfig::new();
            if let Some(servers) = stun {
                p = p.stun_servers(servers);
            }
            if let Some(host) = p2p_host {
                let ip = host
                    .parse()
                    .map_err(|_| invalid(format!("p2p_host is not an IP address: {host}")))?;
                p = p.host_address(ip);
            }
            config = config.p2p(p);
        } else if stun.is_some() || p2p_host.is_some() {
            return Err(invalid("stun and p2p_host need p2p=True"));
        }
        if let Some(n) = &device_name {
            config = config.device_name(n);
        }
        config = match (&password, key) {
            (Some(p), _) if encrypt => config.encrypt(p),
            (Some(p), _) => config.password(p),
            (None, Some(k)) => config.key(data_key(&k)?),
            (None, None) if encrypt => return Err(invalid("encrypt needs a password")),
            (None, None) => config,
        };
        let remote = remote.inner.clone();
        let db = py
            .detach(|| blocking::Database::open(config, remote))
            .map_err(err)?;
        Ok(Self {
            db: Some(db),
            auto_sync: None,
        })
    }

    #[getter]
    fn device_id(&self) -> PyResult<String> {
        Ok(self.db()?.device_id().to_hex())
    }

    fn db_id(&self, py: Python<'_>) -> PyResult<Option<String>> {
        let db = self.db()?;
        Ok(py.detach(|| db.db_id()).map_err(err)?.map(|d| d.to_hex()))
    }

    fn init(&self, py: Python<'_>) -> PyResult<String> {
        let db = self.db()?;
        Ok(py.detach(|| db.init()).map_err(err)?.to_hex())
    }

    fn join(&self, py: Python<'_>, db_id: &str) -> PyResult<Py<PyAny>> {
        let db = self.db()?;
        let id = db_id.parse().map_err(err)?;
        let r = py.detach(|| db.join(id)).map_err(err)?;
        report(py, &r)
    }

    fn init_or_join(&self, py: Python<'_>) -> PyResult<String> {
        let db = self.db()?;
        Ok(py.detach(|| db.init_or_join()).map_err(err)?.to_hex())
    }

    fn sync(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let db = self.db()?;
        let r = py.detach(|| db.sync()).map_err(err)?;
        report(py, &r)
    }

    fn put(&self, py: Python<'_>, collection: &str, id: &str, json: &str) -> PyResult<bool> {
        let (c, doc) = (self.collection(collection)?, parse(json)?);
        py.detach(|| c.put(id, doc)).map_err(err)
    }

    fn patch(
        &self,
        py: Python<'_>,
        collection: &str,
        id: &str,
        json: &str,
        unset: Vec<String>,
    ) -> PyResult<bool> {
        let (c, doc) = (self.collection(collection)?, parse(json)?);
        py.detach(|| {
            let unset: Vec<&str> = unset.iter().map(String::as_str).collect();
            c.patch(id, doc, &unset)
        })
        .map_err(err)
    }

    fn get(&self, py: Python<'_>, collection: &str, id: &str) -> PyResult<Option<String>> {
        let c = self.collection(collection)?;
        let doc = py.detach(|| c.get(id)).map_err(err)?;
        Ok(doc.map(|v| v.to_json().to_string()))
    }

    #[pyo3(signature = (collection, limit = 100, after = None))]
    fn list(
        &self,
        py: Python<'_>,
        collection: &str,
        limit: usize,
        after: Option<String>,
    ) -> PyResult<Vec<(String, String)>> {
        let c = self.collection(collection)?;
        let rows = py
            .detach(|| {
                let mut opts = ListOptions::default().limit(limit);
                if let Some(a) = &after {
                    opts = opts.after(a);
                }
                c.list(opts)
            })
            .map_err(err)?;
        Ok(rows
            .into_iter()
            .map(|(id, v)| (id.as_str().to_owned(), v.to_json().to_string()))
            .collect())
    }

    fn delete(&self, py: Python<'_>, collection: &str, id: &str) -> PyResult<bool> {
        let c = self.collection(collection)?;
        py.detach(|| c.delete(id)).map_err(err)
    }

    fn collections(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        let db = self.db()?;
        let names = py.detach(|| db.collections()).map_err(err)?;
        Ok(names.iter().map(|n| n.as_str().to_owned()).collect())
    }

    fn changes(&self) -> PyResult<Changes> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|e| BothyError::new_err(format!("cannot start a timer: {e}")))?;
        Ok(Changes {
            rx: Mutex::new(self.db()?.subscribe()),
            rt,
        })
    }

    /// Starts background sync. `None` to disable; values in seconds.
    #[pyo3(signature = (debounce = 2.0, poll_interval = 300.0))]
    fn start_auto_sync(
        &mut self,
        py: Python<'_>,
        debounce: f64,
        poll_interval: f64,
    ) -> PyResult<()> {
        let options = bothy::AutoSync {
            debounce: Duration::from_secs_f64(debounce.max(0.0)),
            poll_interval: Duration::from_secs_f64(poll_interval.max(1.0)),
            ..bothy::AutoSync::default()
        };
        let handle = self.db()?.start_auto_sync(options);
        if let Some(old) = self.auto_sync.replace(handle) {
            py.detach(|| old.stop());
        }
        Ok(())
    }

    /// The latest background sync outcome: `None`, a report dict, or an exception instance.
    fn auto_sync_status(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        Ok(match self.auto_sync.as_ref().map(|h| h.status()) {
            None | Some(bothy::SyncStatus::Idle) => None,
            Some(bothy::SyncStatus::Ok(r)) => Some(report(py, &r)?),
            Some(bothy::SyncStatus::Failed { kind, message }) => Some(
                err(bothy::Error::new(kind, message))
                    .into_value(py)
                    .into_any(),
            ),
        })
    }

    fn stop_auto_sync(&mut self, py: Python<'_>) {
        if let Some(h) = self.auto_sync.take() {
            py.detach(|| h.stop());
        }
    }

    fn p2p_status(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let st = self.db()?.p2p_status();
        let d = pyo3::types::PyDict::new(py);
        d.set_item("running", st.running)?;
        d.set_item("discovering", st.discovering)?;
        let peers = pyo3::types::PyList::empty(py);
        for p in &st.peers {
            let e = pyo3::types::PyDict::new(py);
            e.set_item("device_id", p.device_id.to_hex())?;
            let state = match p.state {
                bothy::p2p::PeerState::Signaling => "signaling",
                bothy::p2p::PeerState::Connected => "connected",
                bothy::p2p::PeerState::Failed => "failed",
                bothy::p2p::PeerState::Disconnected => "disconnected",
                _ => "unknown",
            };
            e.set_item("state", state)?;
            peers.append(e)?;
        }
        d.set_item("peers", peers)?;
        Ok(d.into_any().unbind())
    }

    fn status(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let db = self.db()?;
        let (st, encrypted, locked) = py
            .detach(|| Ok::<_, bothy::Error>((db.status()?, db.is_encrypted()?, db.is_locked()?)))
            .map_err(err)?;
        let d = pyo3::types::PyDict::new(py);
        d.set_item("documents", st.documents)?;
        d.set_item("tombstones", st.tombstones)?;
        d.set_item("unsynced_ops", st.unpacked_ops)?;
        d.set_item("pending_uploads", st.pending_uploads)?;
        d.set_item("last_sync_ms", st.last_sync_ms)?;
        d.set_item("encrypted", encrypted)?;
        d.set_item("locked", locked)?;
        Ok(d.into_any().unbind())
    }

    #[pyo3(signature = (since_ms = None))]
    fn conflicts(&self, py: Python<'_>, since_ms: Option<u64>) -> PyResult<Vec<Py<PyAny>>> {
        let db = self.db()?;
        let rows = py.detach(|| db.conflicts(since_ms)).map_err(err)?;
        rows.iter()
            .map(|c| {
                let d = pyo3::types::PyDict::new(py);
                d.set_item("collection", c.collection.as_str())?;
                d.set_item("id", c.doc_id.as_str())?;
                d.set_item("field", c.field.to_string())?;
                d.set_item("at_ms", c.at_ms)?;
                d.set_item("winner_device", c.winner.device.to_hex())?;
                d.set_item("loser_device", c.loser.device.to_hex())?;
                d.set_item(
                    "loser_value_json",
                    c.loser_value.as_ref().map(|v| v.to_json().to_string()),
                )?;
                Ok(d.into_any().unbind())
            })
            .collect()
    }

    fn clear_conflicts(&self, py: Python<'_>) -> PyResult<usize> {
        let db = self.db()?;
        py.detach(|| db.clear_conflicts()).map_err(err)
    }

    fn is_encrypted(&self, py: Python<'_>) -> PyResult<bool> {
        let db = self.db()?;
        py.detach(|| db.is_encrypted()).map_err(err)
    }

    fn is_locked(&self, py: Python<'_>) -> PyResult<bool> {
        let db = self.db()?;
        py.detach(|| db.is_locked()).map_err(err)
    }

    fn unlock(&self, py: Python<'_>, password: String) -> PyResult<()> {
        let db = self.db()?;
        py.detach(|| db.unlock(&password)).map_err(err)
    }

    fn unlock_with_key(&self, py: Python<'_>, key: Vec<u8>) -> PyResult<()> {
        let db = self.db()?;
        let key = data_key(&key)?;
        py.detach(|| db.unlock_with_key(key)).map_err(err)
    }

    fn lock(&self) -> PyResult<()> {
        self.db()?.lock();
        Ok(())
    }

    fn export_key<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        let key = self.db()?.export_key().map_err(err)?;
        Ok(pyo3::types::PyBytes::new(py, key.to_bytes().as_slice()))
    }

    fn change_password(&self, py: Python<'_>, old: String, new: String) -> PyResult<()> {
        let db = self.db()?;
        py.detach(|| db.change_password(&old, &new)).map_err(err)
    }

    /// Stops background sync and closes the database. Further calls raise `BothyError`.
    fn close(&mut self, py: Python<'_>) {
        self.stop_auto_sync(py);
        if let Some(db) = self.db.take() {
            py.detach(|| drop(db));
        }
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_class::<Database>()?;
    m.add_class::<Remote>()?;
    m.add_class::<Changes>()?;
    m.add_class::<Grant>()?;
    m.add_class::<TokenProvider>()?;
    m.add_function(wrap_pyfunction!(device_login, m)?)?;
    m.add_function(wrap_pyfunction!(loopback_login, m)?)?;
    for (name, ty) in [
        ("BothyError", py.get_type::<BothyError>()),
        ("OfflineError", py.get_type::<OfflineError>()),
        ("AuthRequiredError", py.get_type::<AuthRequiredError>()),
        ("RateLimitedError", py.get_type::<RateLimitedError>()),
        ("QuotaExceededError", py.get_type::<QuotaExceededError>()),
        (
            "RemoteInconsistentError",
            py.get_type::<RemoteInconsistentError>(),
        ),
        ("FormatTooNewError", py.get_type::<FormatTooNewError>()),
        ("SchemaTooNewError", py.get_type::<SchemaTooNewError>()),
        ("WrongPasswordError", py.get_type::<WrongPasswordError>()),
        ("KeyRequiredError", py.get_type::<KeyRequiredError>()),
        ("CorruptError", py.get_type::<CorruptError>()),
        ("DatabaseLockedError", py.get_type::<DatabaseLockedError>()),
        ("InvalidInputError", py.get_type::<InvalidInputError>()),
        ("InternalError", py.get_type::<InternalError>()),
    ] {
        m.add(name, ty)?;
    }
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
