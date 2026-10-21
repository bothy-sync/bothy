//! Drive backend contract tests against a mock server: request shape, paging, the idempotent
//! upload path, error mapping and retries, plus two devices syncing through a stateful fake Drive.
#![cfg(feature = "drive")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bothy::auth::AccessTokenProvider;
use bothy::remote::drive::{DriveRemote, DriveRoot, RetryPolicy};
use bothy::remote::{PutOutcome, RemoteStore};
use bothy::{Config, Database, ErrorKind, Result};
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};
use wiremock::matchers::{
    body_string_contains, header, method, path, query_param, query_param_contains,
};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const DB: &str = "000102030405060708090a0b0c0d0e0f";
const DEV: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

/// Hands out `t1`, `t2`, ... and counts invalidations.
#[derive(Default)]
struct Tokens {
    generation: AtomicUsize,
    invalidated: AtomicUsize,
}

#[async_trait]
impl AccessTokenProvider for Tokens {
    async fn access_token(&self) -> Result<String> {
        Ok(format!("t{}", self.generation.load(Ordering::SeqCst) + 1))
    }
    async fn invalidate(&self, _rejected: &str) {
        self.invalidated.fetch_add(1, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

fn fast() -> RetryPolicy {
    RetryPolicy {
        base: Duration::from_millis(1),
        max: Duration::from_millis(4),
        attempts: 4,
    }
}

fn drive(server: &MockServer, tokens: Arc<Tokens>, root: DriveRoot) -> DriveRemote {
    DriveRemote::new(tokens, root)
        .unwrap()
        .with_base_url(&server.uri())
        .with_retry(fast())
}

fn seg_key(seq: u64) -> String {
    format!("{DB}/devices/{DEV}/seg-{seq:016x}.bseg")
}

fn sha(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn list_follows_pages_filters_by_db_and_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("spaces", "appDataFolder"))
        .and(query_param_contains("q", "'appDataFolder' in parents"))
        .and(query_param_contains("q", format!("value='{DB}'")))
        .and(query_param("pageToken", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [{"id": "f2", "name": seg_key(2), "size": "20"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(header("authorization", "Bearer t1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "nextPageToken": "p2",
            "files": [
                {"id": "f1", "name": seg_key(1), "size": "10"},
                {"id": "fx", "name": format!("{DB}/manifest.json"), "size": "5"}
            ]
        })))
        .mount(&server)
        .await;
    let d = drive(&server, Arc::default(), DriveRoot::AppData);
    let keys: Vec<_> = d
        .list(&format!("{DB}/devices/"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.key, e.size))
        .collect();
    assert_eq!(keys, vec![(seg_key(1), 10), (seg_key(2), 20)]);
}

#[tokio::test]
async fn reserved_upload_is_multipart_with_id_name_and_app_properties() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/generateIds"))
        .and(query_param("space", "appDataFolder"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ids": ["ID1"]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/upload/drive/v3/files"))
        .and(query_param("uploadType", "multipart"))
        .and(body_string_contains("\"id\":\"ID1\""))
        .and(body_string_contains(format!("\"name\":\"{}\"", seg_key(3))))
        .and(body_string_contains("\"bothy_seq\":\"3\""))
        .and(body_string_contains("\"parents\":[\"appDataFolder\"]"))
        .and(body_string_contains("SEGMENT-BYTES"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "ID1"})))
        .expect(1)
        .mount(&server)
        .await;
    let d = drive(&server, Arc::default(), DriveRoot::AppData);
    let r = d.reserve(&seg_key(3)).await.unwrap();
    assert_eq!(r.as_deref(), Some("ID1"));
    let out = d
        .put_new(&seg_key(3), b"SEGMENT-BYTES", r.as_deref())
        .await
        .unwrap();
    assert_eq!(out, PutOutcome::Created);
}

#[tokio::test]
async fn conflict_on_a_reserved_id_compares_sha256() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload/drive/v3/files"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": {"code": 409, "errors": [{"reason": "fileIdInUse"}]}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/ID1"))
        .and(query_param("fields", "sha256Checksum"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"sha256Checksum": sha(b"same")})),
        )
        .mount(&server)
        .await;
    let d = drive(&server, Arc::default(), DriveRoot::AppData);
    assert_eq!(
        d.put_new(&seg_key(1), b"same", Some("ID1")).await.unwrap(),
        PutOutcome::AlreadyExists { same_content: true }
    );
    assert_eq!(
        d.put_new(&seg_key(1), b"different", Some("ID1"))
            .await
            .unwrap(),
        PutOutcome::AlreadyExists {
            same_content: false
        }
    );
}

#[tokio::test]
async fn unreserved_upload_checks_for_an_existing_file_by_name_first() {
    let server = MockServer::start().await;
    let key = format!("{DB}/manifest.json");
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param_contains("q", format!("name = '{key}'")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"files": [{"id": "M1", "name": key}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/M1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"sha256Checksum": sha(b"{}")})),
        )
        .mount(&server)
        .await;
    let d = drive(&server, Arc::default(), DriveRoot::AppData);
    assert_eq!(
        d.put_new(&key, b"{}", None).await.unwrap(),
        PutOutcome::AlreadyExists { same_content: true }
    );
}

#[tokio::test]
async fn get_downloads_by_id_and_missing_files_are_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param_contains(
            "q",
            format!("name = '{}'", seg_key(1)),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"files": [{"id": "F1", "name": seg_key(1)}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files": []})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/F1"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"BYTES".to_vec()))
        .mount(&server)
        .await;
    let d = drive(&server, Arc::default(), DriveRoot::AppData);
    assert_eq!(d.get(&seg_key(1)).await.unwrap().unwrap(), b"BYTES");
    assert_eq!(d.get(&seg_key(2)).await.unwrap(), None);
}

#[tokio::test]
async fn unauthorized_refreshes_once_then_requires_sign_in() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/generateIds"))
        .and(header("authorization", "Bearer t2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ids": ["X"]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let tokens = Arc::new(Tokens::default());
    let d = drive(&server, tokens.clone(), DriveRoot::AppData);
    assert_eq!(d.reserve("k").await.unwrap().as_deref(), Some("X"));
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);

    // A token that is rejected even after a refresh means the user must sign in again.
    let server2 = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server2)
        .await;
    let tokens = Arc::new(Tokens::default());
    let d = drive(&server2, tokens.clone(), DriveRoot::AppData);
    assert_eq!(
        d.reserve("k").await.unwrap_err().kind(),
        ErrorKind::AuthRequired
    );
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rate_limits_and_server_errors_back_off_and_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ids": ["OK"]})))
        .mount(&server)
        .await;
    let d = drive(&server, Arc::default(), DriveRoot::AppData);
    assert_eq!(d.reserve("k").await.unwrap().as_deref(), Some("OK"));
}

async fn error_kind(status: u16, reason: Option<&str>) -> ErrorKind {
    let server = MockServer::start().await;
    let mut resp = ResponseTemplate::new(status);
    if let Some(r) = reason {
        resp = resp.set_body_json(json!({"error": {"code": status, "errors": [{"reason": r}]}}));
    }
    Mock::given(method("GET"))
        .respond_with(resp)
        .mount(&server)
        .await;
    let d = drive(&server, Arc::default(), DriveRoot::AppData);
    d.reserve("k").await.unwrap_err().kind()
}

#[tokio::test]
async fn errors_map_to_kinds() {
    assert_eq!(
        error_kind(403, Some("userRateLimitExceeded")).await,
        ErrorKind::RateLimited
    );
    assert_eq!(
        error_kind(403, Some("rateLimitExceeded")).await,
        ErrorKind::RateLimited
    );
    assert_eq!(error_kind(429, None).await, ErrorKind::RateLimited);
    assert_eq!(
        error_kind(403, Some("storageQuotaExceeded")).await,
        ErrorKind::QuotaExceeded
    );
    assert_eq!(
        error_kind(403, Some("insufficientPermissions")).await,
        ErrorKind::AuthRequired
    );
    assert_eq!(error_kind(500, None).await, ErrorKind::Offline);

    // Nothing listening: a network failure.
    let d = DriveRemote::appdata(Arc::new(Tokens::default()))
        .unwrap()
        .with_base_url("http://127.0.0.1:9")
        .with_retry(fast());
    assert_eq!(d.reserve("k").await.unwrap_err().kind(), ErrorKind::Offline);
}

#[tokio::test]
async fn changes_start_with_a_listing_then_follow_pages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/changes/startPageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"startPageToken": "c1"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"files": [{"id": "F1", "name": seg_key(1)}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/changes"))
        .and(query_param("pageToken", "c1"))
        .and(query_param("includeRemoved", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "nextPageToken": "c1b",
            "changes": [{"fileId": "F1", "removed": true}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/changes"))
        .and(query_param("pageToken", "c1b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "newStartPageToken": "c2",
            "changes": [{"fileId": "F2", "file": {"id": "F2", "name": seg_key(2), "size": "7"}}]
        })))
        .mount(&server)
        .await;
    let d = drive(&server, Arc::default(), DriveRoot::AppData);
    let start = d.changes_since(None).await.unwrap().unwrap();
    assert_eq!((start.changed.len(), start.cursor.as_str()), (1, "c1"));
    let next = d.changes_since(Some("c1")).await.unwrap().unwrap();
    assert_eq!(next.removed, vec![seg_key(1)]);
    assert_eq!(next.changed[0].key, seg_key(2));
    assert_eq!(next.cursor, "c2");
}

#[tokio::test]
async fn folder_mode_creates_bothy_and_the_named_folder_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param_contains(
            "q",
            "mimeType = 'application/vnd.google-apps.folder'",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files": []})))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/drive/v3/files"))
        .and(body_string_contains("\"name\":\"Bothy\""))
        .and(body_string_contains("\"parents\":[\"root\"]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "TOP"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/drive/v3/files"))
        .and(body_string_contains("\"name\":\"notes\""))
        .and(body_string_contains("\"parents\":[\"TOP\"]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "FOLDER"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("spaces", "drive"))
        .and(query_param_contains("q", "'FOLDER' in parents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files": []})))
        .expect(2)
        .mount(&server)
        .await;
    let d = drive(&server, Arc::default(), DriveRoot::Folder("notes".into()));
    assert!(d.list("").await.unwrap().is_empty());
    assert!(d.list("").await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// A small stateful fake of the Drive API, enough for the engine to run against it.

#[derive(Default)]
struct FakeFile {
    name: String,
    data: Vec<u8>,
    db: Option<String>,
}

#[derive(Default)]
struct FakeDrive {
    files: Mutex<BTreeMap<String, FakeFile>>,
    next_id: AtomicUsize,
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

struct FakeResponder(Arc<FakeDrive>);

impl Respond for FakeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let path = req.url.path().to_owned();
        let query: BTreeMap<String, String> = req.url.query_pairs().into_owned().collect();
        let mut files = self.0.files.lock().unwrap();
        match (req.method.as_str(), path.as_str()) {
            ("GET", "/drive/v3/files/generateIds") => {
                let n = self.0.next_id.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({"ids": [format!("id{n}")]}))
            }
            ("GET", "/drive/v3/files") => {
                let q = query.get("q").cloned().unwrap_or_default();
                let name = between(&q, "name = '", "'").map(str::to_owned);
                let db = between(&q, "value='", "'").map(str::to_owned);
                let list: Vec<J> = files
                    .iter()
                    .filter(|(_, f)| name.as_ref().is_none_or(|n| *n == f.name))
                    .filter(|(_, f)| db.is_none() || f.db == db)
                    .map(|(id, f)| json!({"id": id, "name": f.name, "size": f.data.len().to_string()}))
                    .collect();
                ResponseTemplate::new(200).set_body_json(json!({"files": list}))
            }
            ("POST", "/upload/drive/v3/files") => {
                // multipart/related: headers, blank line, JSON; boundary; headers, blank line, bytes.
                let b = &req.body;
                let meta_start = find(b, b"\r\n\r\n", 0).unwrap() + 4;
                let meta_end = find(b, b"\r\n--", meta_start).unwrap();
                let meta: J = serde_json::from_slice(&b[meta_start..meta_end]).unwrap();
                let data_start = find(b, b"\r\n\r\n", meta_end).unwrap() + 4;
                let data_end = rfind(b, b"\r\n--").unwrap();
                let id = meta["id"].as_str().unwrap().to_owned();
                if files.contains_key(&id) {
                    return ResponseTemplate::new(409)
                        .set_body_json(json!({"error": {"errors": [{"reason": "fileIdInUse"}]}}));
                }
                files.insert(
                    id.clone(),
                    FakeFile {
                        name: meta["name"].as_str().unwrap().to_owned(),
                        data: b[data_start..data_end].to_vec(),
                        db: meta["appProperties"]["bothy_db"]
                            .as_str()
                            .map(str::to_owned),
                    },
                );
                ResponseTemplate::new(200).set_body_json(json!({"id": id}))
            }
            ("GET", p) if p.starts_with("/drive/v3/files/") => {
                let id = &p["/drive/v3/files/".len()..];
                match files.get(id) {
                    None => ResponseTemplate::new(404),
                    Some(f) if query.get("alt").map(String::as_str) == Some("media") => {
                        ResponseTemplate::new(200).set_body_bytes(f.data.clone())
                    }
                    Some(f) => ResponseTemplate::new(200)
                        .set_body_json(json!({"sha256Checksum": sha(&f.data)})),
                }
            }
            ("DELETE", p) if p.starts_with("/drive/v3/files/") => {
                files.remove(&p["/drive/v3/files/".len()..]);
                ResponseTemplate::new(204)
            }
            ("GET", "/drive/v3/changes/startPageToken") => {
                // Report no change feed position changes: the engine lists on every sync.
                ResponseTemplate::new(404)
            }
            _ => ResponseTemplate::new(400),
        }
    }
}

/// Lists instead of using the change feed, since the fake has none.
struct NoFeed(DriveRemote);

#[async_trait]
impl RemoteStore for NoFeed {
    async fn list(&self, prefix: &str) -> Result<Vec<bothy::remote::RemoteEntry>> {
        self.0.list(prefix).await
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.0.get(key).await
    }
    async fn reserve(&self, key: &str) -> Result<Option<String>> {
        self.0.reserve(key).await
    }
    async fn put_new(&self, key: &str, data: &[u8], r: Option<&str>) -> Result<PutOutcome> {
        self.0.put_new(key, data, r).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.delete(key).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_devices_sync_through_a_fake_drive() {
    let server = MockServer::start().await;
    let fake = Arc::new(FakeDrive::default());
    Mock::given(wiremock::matchers::any())
        .respond_with(FakeResponder(fake.clone()))
        .mount(&server)
        .await;
    let remote = |tokens: Arc<Tokens>| -> Arc<dyn RemoteStore> {
        Arc::new(NoFeed(drive(&server, tokens, DriveRoot::AppData)))
    };
    let dir = tempfile::tempdir().unwrap();
    let a = Database::open(
        Config::new(dir.path().join("a.db"), "com.example.notes"),
        remote(Arc::default()),
    )
    .await
    .unwrap();
    let db_id = a.init().await.unwrap();
    a.collection("notes")
        .unwrap()
        .put("n1", json!({"title": "over Drive"}))
        .await
        .unwrap();
    a.sync().await.unwrap();

    let b = Database::open(
        Config::new(dir.path().join("b.db"), "com.example.notes"),
        remote(Arc::default()),
    )
    .await
    .unwrap();
    b.join(db_id).await.unwrap();
    b.collection("notes")
        .unwrap()
        .patch("n1", json!({"tags": ["x"]}), &[])
        .await
        .unwrap();
    b.sync().await.unwrap();
    a.sync().await.unwrap();

    assert_eq!(
        a.canonical_dump().await.unwrap(),
        b.canonical_dump().await.unwrap()
    );
    let doc = a
        .collection("notes")
        .unwrap()
        .get("n1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.to_json(), json!({"title": "over Drive", "tags": ["x"]}));
    // manifest, two device.json files and two segments.
    assert_eq!(fake.files.lock().unwrap().len(), 5);
}
