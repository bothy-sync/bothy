//! Two (or three) devices sharing one FolderRemote.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bothy::remote::{ChangeBatch, FolderRemote, PutOutcome, RemoteEntry, RemoteStore};
use bothy::{AutoSync, Config, Database, Error, ErrorKind, Origin, Result, Value};
use serde_json::json;
use tempfile::TempDir;

const APP: &str = "com.example.notes";

struct Setup {
    dir: TempDir,
    remote: Arc<FolderRemote>,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(FolderRemote::new(dir.path().join("remote")).unwrap());
        Self { dir, remote }
    }

    fn config(&self, name: &str) -> Config {
        Config::new(self.dir.path().join(format!("{name}.db")), APP).device_name(name)
    }

    async fn open(&self, name: &str) -> Database {
        Database::open(self.config(name), self.remote.clone())
            .await
            .unwrap()
    }

    async fn pair(&self) -> (Database, Database) {
        let a = self.open("a").await;
        let db_id = a.init().await.unwrap();
        let b = self.open("b").await;
        b.join(db_id).await.unwrap();
        (a, b)
    }
}

async fn converge(devices: &[&Database]) {
    for _ in 0..2 {
        for d in devices {
            d.sync().await.unwrap();
        }
    }
    let first = devices[0].canonical_dump().await.unwrap();
    for d in &devices[1..] {
        assert_eq!(
            d.canonical_dump().await.unwrap(),
            first,
            "devices did not converge"
        );
    }
}

#[tokio::test]
async fn offline_edits_to_different_fields_both_survive() {
    let s = Setup::new();
    let (a, b) = s.pair().await;
    let (na, nb) = (
        a.collection("notes").unwrap(),
        b.collection("notes").unwrap(),
    );
    na.put("n1", json!({"title": "draft"})).await.unwrap();
    converge(&[&a, &b]).await;

    na.patch("n1", json!({"title": "from A"}), &[])
        .await
        .unwrap();
    nb.patch("n1", json!({"tags": ["x"]}), &[]).await.unwrap();
    converge(&[&a, &b]).await;

    let doc = nb.get("n1").await.unwrap().unwrap();
    assert_eq!(doc.to_json(), json!({"title": "from A", "tags": ["x"]}));
}

#[tokio::test]
async fn same_field_conflict_keeps_the_later_write_and_logs_the_loser() {
    let s = Setup::new();
    let (a, b) = s.pair().await;
    let (na, nb) = (
        a.collection("notes").unwrap(),
        b.collection("notes").unwrap(),
    );
    na.put("n1", json!({"title": "A"})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    nb.put("n1", json!({"title": "B"})).await.unwrap();
    converge(&[&a, &b]).await;

    assert_eq!(
        na.get("n1").await.unwrap().unwrap().to_json(),
        json!({"title": "B"})
    );
    let lost = na.conflicts("n1").await.unwrap();
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].loser_value, Some(Value::from("A")));
    let meta = na.get_with_meta("n1").await.unwrap();
    assert_eq!(
        meta.fields.values().next().unwrap().stamp.device,
        b.device_id()
    );
}

#[tokio::test]
async fn delete_wins_over_an_older_offline_edit() {
    let s = Setup::new();
    let (a, b) = s.pair().await;
    let (na, nb) = (
        a.collection("notes").unwrap(),
        b.collection("notes").unwrap(),
    );
    na.put("n1", json!({"title": "x"})).await.unwrap();
    converge(&[&a, &b]).await;

    nb.patch("n1", json!({"body": "edited offline"}), &[])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    na.delete("n1").await.unwrap();
    converge(&[&a, &b]).await;

    assert_eq!(na.get("n1").await.unwrap(), None);
    assert_eq!(nb.get("n1").await.unwrap(), None);
}

#[tokio::test]
async fn third_device_and_init_or_join() {
    let s = Setup::new();
    let (a, b) = s.pair().await;
    a.collection("notes")
        .unwrap()
        .put("n1", json!({"v": 1}))
        .await
        .unwrap();
    b.collection("todos")
        .unwrap()
        .put("t1", json!({"done": false}))
        .await
        .unwrap();
    converge(&[&a, &b]).await;

    let c = s.open("c").await;
    let joined = c.init_or_join().await.unwrap();
    assert_eq!(Some(joined), a.db_id().await.unwrap());
    converge(&[&a, &b, &c]).await;
    assert!(
        c.collection("todos")
            .unwrap()
            .get("t1")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn sync_reports_and_events() {
    let s = Setup::new();
    let (a, b) = s.pair().await;
    let mut events = b.subscribe();
    let na = a.collection("notes").unwrap();
    for i in 0..3 {
        na.put(&format!("n{i}"), json!({"i": i})).await.unwrap();
    }
    let up = a.sync().await.unwrap();
    assert_eq!((up.pushed_ops, up.segments_up), (3, 1));
    let down = b.sync().await.unwrap();
    assert_eq!(
        (down.segments_down, down.pulled_ops, down.changed_docs),
        (1, 3, 3)
    );
    for _ in 0..3 {
        let e = events.recv().await.unwrap();
        assert_eq!(e.origin, Origin::Remote);
    }
    // Nothing new: an idle sync does no work.
    let idle = b.sync().await.unwrap();
    assert_eq!((idle.segments_up, idle.segments_down), (0, 0));
}

/// Fails `put_new` on demand: either before writing, or after writing (a lost response).
struct Flaky {
    inner: Arc<FolderRemote>,
    fail_before: AtomicUsize,
    fail_after: AtomicUsize,
}

fn offline() -> Error {
    Error::new(ErrorKind::Offline, "simulated network failure")
}

fn take(n: &AtomicUsize) -> bool {
    // A compare_exchange loop works on every supported toolchain.
    let mut cur = n.load(Ordering::SeqCst);
    while cur > 0 {
        match n.compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => cur = now,
        }
    }
    false
}

#[async_trait]
impl RemoteStore for Flaky {
    async fn list(&self, prefix: &str) -> Result<Vec<RemoteEntry>> {
        self.inner.list(prefix).await
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(key).await
    }
    async fn put_new(&self, key: &str, data: &[u8], r: Option<&str>) -> Result<PutOutcome> {
        if take(&self.fail_before) {
            return Err(offline());
        }
        let out = self.inner.put_new(key, data, r).await?;
        if take(&self.fail_after) {
            return Err(offline());
        }
        Ok(out)
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key).await
    }
    async fn changes_since(&self, c: Option<&str>) -> Result<Option<ChangeBatch>> {
        self.inner.changes_since(c).await
    }
}

#[tokio::test]
async fn interrupted_uploads_resume_without_duplicates_or_loss() {
    let s = Setup::new();
    let flaky = Arc::new(Flaky {
        inner: s.remote.clone(),
        fail_before: AtomicUsize::new(0),
        fail_after: AtomicUsize::new(0),
    });
    let a = Database::open(s.config("a"), flaky.clone()).await.unwrap();
    let db_id = a.init().await.unwrap();
    let b = s.open("b").await;
    b.join(db_id).await.unwrap();
    let na = a.collection("notes").unwrap();

    // Network fails before the upload: the segment stays queued, also across a restart.
    na.put("n1", json!({"v": 1})).await.unwrap();
    flaky.fail_before.store(1, Ordering::SeqCst);
    assert_eq!(a.sync().await.unwrap_err().kind(), ErrorKind::Offline);
    drop((na, a));
    let a = Database::open(s.config("a"), flaky.clone()).await.unwrap();
    let na = a.collection("notes").unwrap();

    // The upload succeeds but the response is lost: the retry finds the same bytes.
    na.put("n2", json!({"v": 2})).await.unwrap();
    flaky.fail_after.store(1, Ordering::SeqCst);
    assert_eq!(a.sync().await.unwrap_err().kind(), ErrorKind::Offline);
    let retry = a.sync().await.unwrap();
    assert_eq!(retry.segments_up, 2);

    converge(&[&a, &b]).await;
    let nb = b.collection("notes").unwrap();
    assert!(nb.get("n1").await.unwrap().is_some());
    assert!(nb.get("n2").await.unwrap().is_some());
    let segments = s
        .remote
        .list(&format!("{db_id}/devices/{}/seg-", a.device_id()))
        .await
        .unwrap();
    assert_eq!(segments.len(), 2, "one segment per write, no duplicates");
}

#[tokio::test]
async fn second_open_is_locked_and_sync_needs_init() {
    let s = Setup::new();
    let a = s.open("a").await;
    let err = Database::open(s.config("a"), s.remote.clone())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::DatabaseLocked);
    assert_eq!(a.sync().await.unwrap_err().kind(), ErrorKind::InvalidInput);
    a.init().await.unwrap();
    assert_eq!(a.init().await.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_sync_delivers_changes_in_the_background() {
    let s = Setup::new();
    let (a, b) = s.pair().await;
    let opts = AutoSync {
        debounce: Duration::from_millis(20),
        max_delay: Duration::from_millis(100),
        poll_interval: Duration::from_millis(50),
        max_backoff: Duration::from_secs(1),
        ..AutoSync::default()
    };
    let ha = a.start_auto_sync(opts);
    let hb = b.start_auto_sync(opts);
    let mut events = b.subscribe();
    a.collection("notes")
        .unwrap()
        .put("n1", json!({"v": 1}))
        .await
        .unwrap();
    let e = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("change arrives within 10 s")
        .unwrap();
    assert_eq!((e.doc_id.as_str(), e.origin), ("n1", Origin::Remote));
    ha.stop().await;
    hb.stop().await;
}

#[cfg(feature = "blocking")]
#[test]
fn blocking_facade_works_outside_and_refuses_inside_a_runtime() {
    let s = Setup::new();
    let a = bothy::blocking::Database::open(s.config("a"), s.remote.clone()).unwrap();
    a.init().unwrap();
    let notes = a.collection("notes").unwrap();
    notes.put("n1", json!({"v": 1})).unwrap();
    assert_eq!(a.sync().unwrap().segments_up, 1);
    assert_eq!(notes.get("n1").unwrap().unwrap().to_json(), json!({"v": 1}));

    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt.block_on(async { notes.get("n1") }).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

/// Signaling files (valid, malformed or from a newer version) live next to the data and never
/// disturb a sync; 0.3 readers already skip the whole `signaling/` prefix.
#[tokio::test]
async fn signaling_files_are_ignored_by_sync() {
    let s = Setup::new();
    let (a, b) = s.pair().await;
    let db = a.db_id().await.unwrap().unwrap();
    let me = a.device_id();
    let other = b.device_id();
    for (name, body) in [
        (
            "online-018bcfe568000001.json".to_owned(),
            format!(r#"{{"v":1,"device_id":"{me}","session":"0000000000000001"}}"#),
        ),
        (
            format!("offer-{other}-018bcfe568000002.json"),
            r#"{"v":9}"#.to_owned(),
        ),
        (
            format!("answer-{other}-018bcfe568000003.json"),
            "not json".to_owned(),
        ),
        ("hello.json".to_owned(), "{}".to_owned()),
    ] {
        let key = format!("{db}/signaling/{me}/{name}");
        s.remote.put_new(&key, body.as_bytes(), None).await.unwrap();
    }
    a.collection("notes")
        .unwrap()
        .put("n1", json!({"title": "next to signaling"}))
        .await
        .unwrap();
    converge(&[&a, &b]).await;
    let got = b.collection("notes").unwrap().get("n1").await.unwrap();
    assert!(got.is_some());
}

/// Signing in to another account changes the remote under an attached database.
#[tokio::test]
async fn a_remote_without_this_database_gets_nothing() {
    let setup = Setup::new();
    let a = setup.open("a").await;
    let db_id = a.init().await.unwrap();
    let notes = a.collection("notes").unwrap();
    notes.put("sent", json!({"v": 1})).await.unwrap();
    a.sync().await.unwrap();
    notes.put("unsent", json!({"v": 2})).await.unwrap();
    drop((a, notes));

    let other = Arc::new(FolderRemote::new(setup.dir.path().join("other")).unwrap());
    let a = Database::open(setup.config("a"), other.clone())
        .await
        .unwrap();
    let refused = |e: Error| {
        assert_eq!(e.kind(), ErrorKind::RemoteInconsistent, "{e}");
        assert!(e.message().contains("not on this remote"), "{e}");
    };
    refused(a.sync().await.unwrap_err());
    refused(a.snapshot().await.unwrap_err());
    refused(a.compact(false).await.unwrap_err());
    refused(a.prune_device(a.device_id()).await.unwrap_err());
    assert_eq!(other.list("").await.unwrap(), vec![]);

    // The local data is intact, and the right remote still works.
    let notes = a.collection("notes").unwrap();
    assert!(notes.get("sent").await.unwrap().is_some());
    assert!(notes.get("unsent").await.unwrap().is_some());
    drop((a, notes));
    let a = setup.open("a").await;
    assert_eq!(a.sync().await.unwrap().pushed_ops, 1);
    let b = setup.open("b").await;
    b.join(db_id).await.unwrap();
    assert!(
        b.collection("notes")
            .unwrap()
            .get("unsent")
            .await
            .unwrap()
            .is_some()
    );
}
