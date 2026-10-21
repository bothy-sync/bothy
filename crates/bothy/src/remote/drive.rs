//! Google Drive backend (Drive API v3), feature `drive`.
//!
//! Files live flat in one parent: the hidden `appDataFolder` (scope `drive.appdata`) or one
//! folder `Bothy/<name>` (scope `drive.file`). The file name is the logical key; `appProperties`
//! tag each file with its database, kind, device and seq.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};

use crate::auth::AccessTokenProvider;
use crate::codec::keys::{KeyKind, LogicalKey};
use crate::error::{Error, ErrorKind, Result};
use crate::remote::{ChangeBatch, PutOutcome, RemoteEntry, RemoteStore};

const DEFAULT_BASE: &str = "https://www.googleapis.com";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const TOP_FOLDER: &str = "Bothy";

/// Where the files live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveRoot {
    /// The hidden per-app folder (`appDataFolder`, scope `drive.appdata`). Not reachable from
    /// the device (TV) OAuth flow.
    AppData,
    /// A visible folder `Bothy/<name>` created by the app (scope `drive.file`). Required when a
    /// headless device signs in with the device flow.
    Folder(String),
}

/// Retry timing for rate limits, server errors and network failures.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// First backoff delay.
    pub base: Duration,
    /// Longest single delay.
    pub max: Duration,
    /// Attempts in total, including the first.
    pub attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(64),
            attempts: 8,
        }
    }
}

#[derive(Default)]
struct Cache {
    parent: Option<String>,
    ids: HashMap<String, String>,
    keys: HashMap<String, String>,
}

impl Cache {
    fn remember(&mut self, key: &str, id: &str) {
        self.ids.insert(key.to_owned(), id.to_owned());
        self.keys.insert(id.to_owned(), key.to_owned());
    }

    fn forget(&mut self, key: &str) {
        if let Some(id) = self.ids.remove(key) {
            self.keys.remove(&id);
        }
    }
}

/// A [`RemoteStore`] on Google Drive.
#[derive(Clone)]
pub struct DriveRemote {
    http: reqwest::Client,
    tokens: Arc<dyn AccessTokenProvider>,
    root: DriveRoot,
    base: String,
    retry: RetryPolicy,
    cache: Arc<Mutex<Cache>>,
}

impl std::fmt::Debug for DriveRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveRemote")
            .field("root", &self.root)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// Escapes a string for a Drive query literal.
fn q(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn bad_response(what: &str) -> Error {
    Error::new(
        ErrorKind::RemoteInconsistent,
        format!("drive: unexpected response to {what}"),
    )
}

fn parse_json(body: &[u8], what: &str) -> Result<J> {
    serde_json::from_slice(body).map_err(|_| bad_response(what))
}

/// `appProperties` for a key, so files can be filtered by database, kind and device.
fn app_properties(key: &str) -> J {
    let Some(k) = LogicalKey::parse(key) else {
        return json!({});
    };
    let mut m = serde_json::Map::new();
    m.insert("bothy_db".into(), J::String(k.db_id.to_hex()));
    let (kind, device, seq) = match k.kind {
        KeyKind::Manifest => ("manifest", None, None),
        KeyKind::Device(d) => ("device", Some(d), None),
        KeyKind::Segment(d, s) => ("seg", Some(d), Some(s)),
        KeyKind::Snapshot(d, _) => ("snap", Some(d), None),
    };
    m.insert("bothy_kind".into(), J::String(kind.into()));
    if let Some(d) = device {
        m.insert("bothy_device".into(), J::String(d.to_hex()));
    }
    if let Some(s) = seq {
        m.insert("bothy_seq".into(), J::String(s.to_string()));
    }
    J::Object(m)
}

fn reason(body: &[u8]) -> String {
    serde_json::from_slice::<J>(body)
        .ok()
        .and_then(|j| {
            j["error"]["errors"][0]["reason"]
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn jitter(d: Duration) -> Duration {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    let frac = u32::from_le_bytes(b) as f64 / u32::MAX as f64;
    d.mul_f64(0.5 + frac * 0.5)
}

struct Response {
    status: StatusCode,
    body: Vec<u8>,
}

impl DriveRemote {
    /// A backend on `root`, authenticating with `tokens`.
    pub fn new(tokens: Arc<dyn AccessTokenProvider>, root: DriveRoot) -> Result<Self> {
        // reqwest uses the process-wide rustls provider; ring keeps cross-compiling simple.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| {
                Error::new(ErrorKind::Internal, "cannot build HTTP client").with_source(e)
            })?;
        Ok(Self {
            http,
            tokens,
            root,
            base: DEFAULT_BASE.to_owned(),
            retry: RetryPolicy::default(),
            cache: Arc::new(Mutex::new(Cache::default())),
        })
    }

    /// The `appDataFolder` backend.
    pub fn appdata(tokens: Arc<dyn AccessTokenProvider>) -> Result<Self> {
        Self::new(tokens, DriveRoot::AppData)
    }

    /// The `Bothy/<name>` folder backend.
    pub fn folder(tokens: Arc<dyn AccessTokenProvider>, name: &str) -> Result<Self> {
        if name.is_empty() || name.contains('/') {
            return Err(Error::invalid_input(
                "folder name must be non-empty and contain no '/'",
            ));
        }
        Self::new(tokens, DriveRoot::Folder(name.to_owned()))
    }

    /// Sends requests to another server (tests).
    pub fn with_base_url(mut self, base: &str) -> Self {
        self.base = base.trim_end_matches('/').to_owned();
        self
    }

    /// Replaces the retry timing.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The root mode.
    pub fn root(&self) -> &DriveRoot {
        &self.root
    }

    fn space(&self) -> &'static str {
        match self.root {
            DriveRoot::AppData => "appDataFolder",
            DriveRoot::Folder(_) => "drive",
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sends a request, retrying rate limits, server errors and network failures, and
    /// refreshing the token once after a 401. Returns any other status to the caller.
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<(&str, &[u8])>,
    ) -> Result<Response> {
        let mut url = reqwest::Url::parse(&format!("{}{path}", self.base))
            .map_err(|_| Error::new(ErrorKind::Internal, "bad Drive URL"))?;
        url.query_pairs_mut().extend_pairs(query);
        let mut refreshed = false;
        let mut delay = self.retry.base;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let token = self.tokens.access_token().await?;
            let mut req = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(&token);
            if let Some((ctype, bytes)) = body {
                req = req.header("Content-Type", ctype).body(bytes.to_vec());
            }
            let outcome = match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    match resp.bytes().await {
                        Ok(b) => Ok(Response {
                            status,
                            body: b.to_vec(),
                        }),
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
            let retry_kind = match outcome {
                Err(e) => {
                    if attempt >= self.retry.attempts {
                        return Err(Error::new(ErrorKind::Offline, "cannot reach Google Drive")
                            .with_source(e));
                    }
                    ErrorKind::Offline
                }
                Ok(r) => match r.status {
                    StatusCode::UNAUTHORIZED if !refreshed => {
                        refreshed = true;
                        self.tokens.invalidate(&token).await;
                        continue;
                    }
                    StatusCode::UNAUTHORIZED => {
                        return Err(Error::new(
                            ErrorKind::AuthRequired,
                            "Google Drive rejected the access token",
                        ));
                    }
                    StatusCode::FORBIDDEN => match reason(&r.body).as_str() {
                        "userRateLimitExceeded" | "rateLimitExceeded" => ErrorKind::RateLimited,
                        "storageQuotaExceeded" | "quotaExceeded" => {
                            return Err(Error::new(
                                ErrorKind::QuotaExceeded,
                                "Google Drive storage is full",
                            ));
                        }
                        "insufficientPermissions"
                        | "insufficientScopes"
                        | "appNotAuthorizedToFile" => {
                            return Err(Error::new(
                                ErrorKind::AuthRequired,
                                "the access token lacks the Drive scope for this root",
                            ));
                        }
                        other => {
                            return Err(Error::new(
                                ErrorKind::Internal,
                                format!("Google Drive refused the request ({other})"),
                            ));
                        }
                    },
                    StatusCode::TOO_MANY_REQUESTS => ErrorKind::RateLimited,
                    s if s.is_server_error() => ErrorKind::Offline,
                    _ => return Ok(r),
                },
            };
            if attempt >= self.retry.attempts {
                let msg = match retry_kind {
                    ErrorKind::RateLimited => {
                        "Google Drive rate limit still exceeded after retries"
                    }
                    _ => "Google Drive kept failing after retries",
                };
                return Err(Error::new(retry_kind, msg));
            }
            tokio::time::sleep(jitter(delay)).await;
            delay = (delay * 2).min(self.retry.max);
        }
    }

    async fn find_folder(&self, name: &str, parent: &str) -> Result<Option<String>> {
        let query = format!(
            "name = '{}' and mimeType = '{FOLDER_MIME}' and '{}' in parents and trashed = false",
            q(name),
            q(parent)
        );
        let r = self
            .send(
                Method::GET,
                "/drive/v3/files",
                &[("q", &query), ("spaces", "drive"), ("fields", "files(id)")],
                None,
            )
            .await?;
        if !r.status.is_success() {
            return Err(bad_response("folder lookup"));
        }
        Ok(parse_json(&r.body, "folder lookup")?["files"][0]["id"]
            .as_str()
            .map(str::to_owned))
    }

    async fn create_folder(&self, name: &str, parent: &str) -> Result<String> {
        let meta = json!({"name": name, "mimeType": FOLDER_MIME, "parents": [parent]});
        let body = serde_json::to_vec(&meta).expect("JSON values always serialize");
        let r = self
            .send(
                Method::POST,
                "/drive/v3/files",
                &[("fields", "id")],
                Some(("application/json", &body)),
            )
            .await?;
        if !r.status.is_success() {
            return Err(bad_response("folder creation"));
        }
        parse_json(&r.body, "folder creation")?["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| bad_response("folder creation"))
    }

    /// Id of the parent all files live in, creating `Bothy/<name>` on first use.
    async fn parent(&self) -> Result<String> {
        if let Some(p) = self.cache().parent.clone() {
            return Ok(p);
        }
        let id = match &self.root {
            DriveRoot::AppData => "appDataFolder".to_owned(),
            DriveRoot::Folder(name) => {
                let top = match self.find_folder(TOP_FOLDER, "root").await? {
                    Some(id) => id,
                    None => self.create_folder(TOP_FOLDER, "root").await?,
                };
                match self.find_folder(name, &top).await? {
                    Some(id) => id,
                    None => self.create_folder(name, &top).await?,
                }
            }
        };
        self.cache().parent = Some(id.clone());
        Ok(id)
    }

    /// Lists files in the parent matching an extra query clause, following pages.
    async fn files(&self, extra: Option<String>) -> Result<Vec<(String, String, u64)>> {
        let parent = self.parent().await?;
        let mut query = format!("'{}' in parents and trashed = false", q(&parent));
        if let Some(x) = extra {
            query.push_str(" and ");
            query.push_str(&x);
        }
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut params = vec![
                ("q", query.as_str()),
                ("spaces", self.space()),
                ("pageSize", "1000"),
                ("fields", "nextPageToken,files(id,name,size)"),
            ];
            if let Some(p) = &page {
                params.push(("pageToken", p.as_str()));
            }
            let r = self
                .send(Method::GET, "/drive/v3/files", &params, None)
                .await?;
            if !r.status.is_success() {
                return Err(bad_response("files.list"));
            }
            let j = parse_json(&r.body, "files.list")?;
            for f in j["files"].as_array().into_iter().flatten() {
                let (Some(id), Some(name)) = (f["id"].as_str(), f["name"].as_str()) else {
                    continue;
                };
                let size = f["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                out.push((id.to_owned(), name.to_owned(), size));
            }
            match j["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_owned()),
                None => break,
            }
        }
        let mut c = self.cache();
        for (id, name, _) in &out {
            c.remember(name, id);
        }
        Ok(out)
    }

    async fn lookup(&self, key: &str) -> Result<Option<String>> {
        if let Some(id) = self.cache().ids.get(key).cloned() {
            return Ok(Some(id));
        }
        let found = self.files(Some(format!("name = '{}'", q(key)))).await?;
        Ok(found.into_iter().next().map(|(id, _, _)| id))
    }

    async fn generate_id(&self) -> Result<String> {
        let r = self
            .send(
                Method::GET,
                "/drive/v3/files/generateIds",
                &[("count", "1"), ("space", self.space()), ("type", "files")],
                None,
            )
            .await?;
        if !r.status.is_success() {
            return Err(bad_response("files.generateIds"));
        }
        parse_json(&r.body, "files.generateIds")?["ids"][0]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| bad_response("files.generateIds"))
    }

    /// Whether the existing file `id` has exactly `data`, by SHA-256.
    async fn same_content(&self, id: &str, data: &[u8]) -> Result<bool> {
        let r = self
            .send(
                Method::GET,
                &format!("/drive/v3/files/{id}"),
                &[("fields", "sha256Checksum")],
                None,
            )
            .await?;
        if !r.status.is_success() {
            return Err(bad_response("checksum lookup"));
        }
        let want = hex_digest(data);
        match parse_json(&r.body, "checksum lookup")?["sha256Checksum"].as_str() {
            Some(have) => Ok(have.eq_ignore_ascii_case(&want)),
            // No checksum reported: compare the bytes.
            None => Ok(self.download(id).await?.as_deref() == Some(data)),
        }
    }

    async fn download(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let r = self
            .send(
                Method::GET,
                &format!("/drive/v3/files/{id}"),
                &[("alt", "media")],
                None,
            )
            .await?;
        match r.status {
            s if s.is_success() => Ok(Some(r.body)),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(bad_response("file download")),
        }
    }
}

fn hex_digest(data: &[u8]) -> String {
    crate::model::to_hex(&Sha256::digest(data))
}

/// A `multipart/related` upload body: JSON metadata, then the bytes.
fn multipart(meta: &J, data: &[u8]) -> (String, Vec<u8>) {
    let mut boundary_bytes = [0u8; 12];
    let _ = getrandom::fill(&mut boundary_bytes);
    let boundary = format!("bothy-{}", crate::model::to_hex(&boundary_bytes));
    let mut body = Vec::with_capacity(data.len() + 512);
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(&serde_json::to_vec(meta).expect("JSON values always serialize"));
    body.extend_from_slice(
        format!("\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/related; boundary={boundary}"), body)
}

#[async_trait]
impl RemoteStore for DriveRemote {
    async fn list(&self, prefix: &str) -> Result<Vec<RemoteEntry>> {
        // Narrow by database on the server when the prefix names one.
        let db = prefix
            .get(..32)
            .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()));
        let extra = db.map(|d| {
            format!(
                "appProperties has {{ key='bothy_db' and value='{}' }}",
                q(d)
            )
        });
        let mut out: Vec<RemoteEntry> = self
            .files(extra)
            .await?
            .into_iter()
            .filter(|(_, name, _)| name.starts_with(prefix))
            .map(|(_, key, size)| RemoteEntry { key, size })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let Some(id) = self.lookup(key).await? else {
            return Ok(None);
        };
        let got = self.download(&id).await?;
        if got.is_none() {
            self.cache().forget(key);
        }
        Ok(got)
    }

    async fn reserve(&self, _key: &str) -> Result<Option<String>> {
        Ok(Some(self.generate_id().await?))
    }

    async fn put_new(
        &self,
        key: &str,
        data: &[u8],
        reservation: Option<&str>,
    ) -> Result<PutOutcome> {
        let id = match reservation {
            Some(id) => id.to_owned(),
            None => {
                // Without a stored reservation, a file of this name may be from an earlier try.
                if let Some(existing) = self.lookup(key).await? {
                    return Ok(PutOutcome::AlreadyExists {
                        same_content: self.same_content(&existing, data).await?,
                    });
                }
                self.generate_id().await?
            }
        };
        let parent = self.parent().await?;
        let meta = json!({"id": id, "name": key, "parents": [parent], "appProperties": app_properties(key)});
        let (ctype, body) = multipart(&meta, data);
        let r = self
            .send(
                Method::POST,
                "/upload/drive/v3/files",
                &[("uploadType", "multipart"), ("fields", "id")],
                Some((&ctype, &body)),
            )
            .await?;
        match r.status {
            s if s.is_success() => {
                self.cache().remember(key, &id);
                Ok(PutOutcome::Created)
            }
            // fileIdInUse: an earlier attempt with this id got through.
            StatusCode::CONFLICT => {
                self.cache().remember(key, &id);
                Ok(PutOutcome::AlreadyExists {
                    same_content: self.same_content(&id, data).await?,
                })
            }
            _ => Err(bad_response("files.create")),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let Some(id) = self.lookup(key).await? else {
            return Ok(());
        };
        let r = self
            .send(Method::DELETE, &format!("/drive/v3/files/{id}"), &[], None)
            .await?;
        self.cache().forget(key);
        match r.status {
            s if s.is_success() || s == StatusCode::NOT_FOUND => Ok(()),
            _ => Err(bad_response("files.delete")),
        }
    }

    async fn changes_since(&self, cursor: Option<&str>) -> Result<Option<ChangeBatch>> {
        let Some(mut token) = cursor.map(str::to_owned) else {
            // Start: remember the current position, then list everything once.
            let r = self
                .send(
                    Method::GET,
                    "/drive/v3/changes/startPageToken",
                    &[("spaces", self.space())],
                    None,
                )
                .await?;
            if !r.status.is_success() {
                return Err(bad_response("changes.getStartPageToken"));
            }
            let start = parse_json(&r.body, "changes.getStartPageToken")?["startPageToken"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad_response("changes.getStartPageToken"))?;
            let changed = self.list("").await?;
            return Ok(Some(ChangeBatch {
                changed,
                removed: Vec::new(),
                cursor: start,
            }));
        };
        let parent = self.parent().await?;
        let mut batch = ChangeBatch::default();
        loop {
            let r = self
                .send(
                    Method::GET,
                    "/drive/v3/changes",
                    &[
                        ("pageToken", token.as_str()),
                        ("spaces", self.space()),
                        ("includeRemoved", "true"),
                        ("pageSize", "1000"),
                        ("fields", "nextPageToken,newStartPageToken,changes(fileId,removed,file(id,name,size,trashed,parents))"),
                    ],
                    None,
                )
                .await?;
            if !r.status.is_success() {
                return Err(bad_response("changes.list"));
            }
            let j = parse_json(&r.body, "changes.list")?;
            for c in j["changes"].as_array().into_iter().flatten() {
                let file_id = c["fileId"].as_str().unwrap_or_default();
                let f = &c["file"];
                let gone =
                    c["removed"].as_bool() == Some(true) || f["trashed"].as_bool() == Some(true);
                if gone {
                    let key = self.cache().keys.get(file_id).cloned();
                    if let Some(key) = key {
                        self.cache().forget(&key);
                        batch.removed.push(key);
                    }
                    continue;
                }
                let in_parent = f["parents"].as_array().is_some_and(|ps| ps.iter().any(|p| p.as_str() == Some(&parent)))
                    // appDataFolder files report the folder's real id, not the alias.
                    || matches!(self.root, DriveRoot::AppData);
                if let (true, Some(name)) = (in_parent, f["name"].as_str()) {
                    self.cache().remember(name, file_id);
                    let size = f["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                    batch.changed.push(RemoteEntry {
                        key: name.to_owned(),
                        size,
                    });
                }
            }
            if let Some(next) = j["nextPageToken"].as_str() {
                token = next.to_owned();
                continue;
            }
            batch.cursor = j["newStartPageToken"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad_response("changes.list"))?;
            return Ok(Some(batch));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_literals_are_escaped() {
        assert_eq!(q("it's a\\b"), "it\\'s a\\\\b");
    }

    #[test]
    fn app_properties_follow_the_key() {
        let db = "000102030405060708090a0b0c0d0e0f";
        let dev = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
        let seg = app_properties(&format!("{db}/devices/{dev}/seg-000000000000002a.bseg"));
        assert_eq!(
            seg,
            json!({"bothy_db": db, "bothy_kind": "seg", "bothy_device": dev, "bothy_seq": "42"})
        );
        let m = app_properties(&format!("{db}/manifest.json"));
        assert_eq!(m, json!({"bothy_db": db, "bothy_kind": "manifest"}));
    }

    #[test]
    fn multipart_body_has_metadata_then_bytes() {
        let (ctype, body) = multipart(&json!({"name": "k"}), b"DATA");
        let boundary = ctype.strip_prefix("multipart/related; boundary=").unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(&format!("--{boundary}\r\nContent-Type: application/json")));
        assert!(text.contains("{\"name\":\"k\"}"));
        assert!(text.ends_with(&format!("DATA\r\n--{boundary}--\r\n")));
    }
}
