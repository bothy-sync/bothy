//! Snapshots, joining from a snapshot, compaction, gap repair and device-id forks.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bothy::remote::{FolderRemote, RemoteStore};
use bothy::store::PackLimits;
use bothy::{Clock, Config, Database, DbId, SnapshotPolicy};
use serde_json::json;
use tempfile::TempDir;

const DAY: u64 = 24 * 60 * 60 * 1000;

struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Env {
    dir: TempDir,
    remote: Arc<FolderRemote>,
    clock: Arc<TestClock>,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(FolderRemote::new(dir.path().join("remote")).unwrap());
        Self {
            dir,
            remote,
            clock: Arc::new(TestClock(AtomicU64::new(1_700_000_000_000))),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(format!("{name}.db"))
    }

    fn config(&self, name: &str) -> Config {
        Config::new(self.path(name), "com.example.notes")
            .clock(self.clock.clone())
            // One op per segment and a manual snapshot policy keep counts predictable.
            .pack_limits(PackLimits {
                max_ops: 1,
                ..PackLimits::default()
            })
            .snapshot_policy(SnapshotPolicy {
                every_segments: u64::MAX,
                every_bytes: u64::MAX,
                grace_period_days: 1,
            })
    }

    async fn open(&self, name: &str) -> Database {
        Database::open(self.config(name), self.remote.clone())
            .await
            .unwrap()
    }

    fn advance(&self, ms: u64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }

    async fn count(&self, prefix: &str) -> usize {
        self.remote.list(prefix).await.unwrap().len()
    }
}

async fn write_n(db: &Database, from: usize, n: usize) {
    let notes = db.collection("notes").unwrap();
    for i in from..from + n {
        notes.put(&format!("n{i}"), json!({"i": i})).await.unwrap();
    }
}

async fn same(a: &Database, b: &Database) {
    assert_eq!(
        a.canonical_dump().await.unwrap(),
        b.canonical_dump().await.unwrap()
    );
}

#[tokio::test]
async fn new_device_starts_from_the_snapshot() {
    let env = Env::new();
    let a = env.open("a").await;
    let db_id = a.init().await.unwrap();
    write_n(&a, 0, 10).await;
    a.sync().await.unwrap();
    a.snapshot().await.unwrap();
    write_n(&a, 10, 2).await;
    a.sync().await.unwrap();

    let c = env.open("c").await;
    let report = c.join(db_id).await.unwrap();
    assert_eq!(report.snapshots_applied, 1);
    assert_eq!(
        report.segments_down, 2,
        "only the segments after the snapshot"
    );
    same(&a, &c).await;
}

#[tokio::test]
async fn compaction_waits_for_the_grace_period_and_keeps_the_newest_snapshot() {
    let env = Env::new();
    let a = env.open("a").await;
    let db_id = a.init().await.unwrap();
    let segs = format!("{db_id}/devices/{}/seg-", a.device_id());
    let snaps = format!("{db_id}/snapshots/{}/", a.device_id());
    write_n(&a, 0, 5).await;
    a.sync().await.unwrap();
    a.snapshot().await.unwrap();

    assert!(
        a.compact(false).await.unwrap().segments.is_empty(),
        "too young"
    );
    env.advance(DAY + 1);
    // The newest own segment always stays, so devices that fell behind can see the gap.
    let dry = a.compact(true).await.unwrap();
    assert_eq!(dry.segments.len(), 4);
    assert_eq!(env.count(&segs).await, 5, "dry run deletes nothing");
    let done = a.compact(false).await.unwrap();
    assert_eq!(done.segments.len(), 4);
    assert_eq!(env.count(&segs).await, 1);

    // A newer snapshot supersedes the old one only once it is past the grace period itself.
    write_n(&a, 5, 1).await;
    a.sync().await.unwrap();
    a.snapshot().await.unwrap();
    // sync also compacts: the old snapshot already covers seg 5, and seg 6 is the newest.
    assert_eq!(env.count(&segs).await, 1);
    assert!(
        a.compact(false).await.unwrap().snapshots.is_empty(),
        "new snapshot too young"
    );
    env.advance(DAY + 1);
    let done = a.compact(false).await.unwrap();
    assert_eq!(done.snapshots.len(), 1);
    assert_eq!(env.count(&segs).await, 1);
    assert_eq!(env.count(&snaps).await, 1);

    let late = env.open("late").await;
    late.join(db_id).await.unwrap();
    same(&a, &late).await;
}

#[tokio::test]
async fn long_offline_device_repairs_the_gap_from_a_snapshot() {
    let env = Env::new();
    let a = env.open("a").await;
    let db_id = a.init().await.unwrap();
    let b = env.open("b").await;
    b.join(db_id).await.unwrap();

    write_n(&a, 0, 4).await;
    a.sync().await.unwrap();
    b.sync().await.unwrap(); // b has seq 1..4
    write_n(&a, 4, 4).await;
    a.sync().await.unwrap(); // seq 5..8; b is offline from here
    a.snapshot().await.unwrap();
    env.advance(DAY + 1);
    a.compact(false).await.unwrap(); // deletes 1..8
    write_n(&a, 8, 2).await;
    a.sync().await.unwrap(); // seq 9..10

    let report = b.sync().await.unwrap();
    assert_eq!(report.snapshots_applied, 1);
    assert!(report.gaps.is_empty(), "{:?}", report.gaps);
    same(&a, &b).await;
}

#[tokio::test]
async fn restored_copy_forks_to_a_new_device_id_without_losing_writes() {
    let env = Env::new();
    let a = env.open("a").await;
    let db_id = a.init().await.unwrap();
    write_n(&a, 0, 2).await;
    a.sync().await.unwrap();
    let old_id = a.device_id();
    drop(a);
    std::fs::copy(env.path("a"), env.path("copy")).unwrap();

    let a = env.open("a").await;
    let copy = env.open("copy").await;
    assert_eq!(copy.device_id(), old_id);
    a.collection("notes")
        .unwrap()
        .put("from_a", json!({"v": 1}))
        .await
        .unwrap();
    copy.collection("notes")
        .unwrap()
        .put("from_copy", json!({"v": 2}))
        .await
        .unwrap();
    a.sync().await.unwrap();
    let report = copy.sync().await.unwrap();
    let new_id = report.forked_to.expect("the copy collides and forks");
    assert_ne!(new_id, old_id);
    assert_eq!(copy.device_id(), new_id);

    for _ in 0..2 {
        a.sync().await.unwrap();
        copy.sync().await.unwrap();
    }
    same(&a, &copy).await;
    let notes = a.collection("notes").unwrap();
    assert!(notes.get("from_copy").await.unwrap().is_some());
    assert!(notes.get("from_a").await.unwrap().is_some());

    let fresh = env.open("fresh").await;
    fresh.join(db_id).await.unwrap();
    same(&a, &fresh).await;
}

#[tokio::test]
async fn automatic_snapshot_after_enough_segments() {
    let env = Env::new();
    let config = env.config("a").snapshot_policy(SnapshotPolicy {
        every_segments: 3,
        every_bytes: u64::MAX,
        grace_period_days: 1,
    });
    let a = Database::open(config, env.remote.clone()).await.unwrap();
    let db_id: DbId = a.init().await.unwrap();
    write_n(&a, 0, 2).await;
    assert_eq!(a.sync().await.unwrap().snapshots_up, 0);
    write_n(&a, 2, 1).await;
    assert_eq!(a.sync().await.unwrap().snapshots_up, 1);
    assert_eq!(env.count(&format!("{db_id}/snapshots/")).await, 1);
}

#[tokio::test]
async fn status_devices_and_collections() {
    let env = Env::new();
    let a = env.open("a").await;
    let db_id = a.init().await.unwrap();
    let b = env.open("b").await;
    b.join(db_id).await.unwrap();
    write_n(&a, 0, 3).await;
    a.collection("todos")
        .unwrap()
        .put("t", json!({"x": 1}))
        .await
        .unwrap();
    a.collection("notes").unwrap().delete("n0").await.unwrap();

    let st = a.status().await.unwrap();
    assert_eq!((st.documents, st.tombstones, st.unpacked_ops), (3, 1, 5));
    assert_eq!(st.last_sync_ms, None);
    a.sync().await.unwrap();
    let st = a.status().await.unwrap();
    assert_eq!((st.unpacked_ops, st.pending_uploads), (0, 0));
    assert!(st.last_sync_ms.is_some());
    let names: Vec<_> = a
        .collections()
        .await
        .unwrap()
        .iter()
        .map(|c| c.to_string())
        .collect();
    assert_eq!(names, ["notes", "todos"]);

    b.sync().await.unwrap();
    let devs = b.devices().await.unwrap();
    assert_eq!(devs.len(), 1);
    assert_eq!((devs[0].device_id, devs[0].watermark), (a.device_id(), 5));
    b.rename_device(a.device_id(), "laptop").await.unwrap();
    assert_eq!(
        b.devices().await.unwrap()[0].name.as_deref(),
        Some("laptop")
    );
}

#[tokio::test]
async fn pruning_a_lost_device_keeps_its_data_and_newest_segment() {
    let env = Env::new();
    let lost = env.open("lost").await;
    let db_id = lost.init().await.unwrap();
    write_n(&lost, 0, 4).await;
    lost.sync().await.unwrap();
    let lost_id = lost.device_id();
    drop(lost);

    let a = env.open("a").await;
    a.join(db_id).await.unwrap();
    assert!(
        a.prune_device(a.device_id()).await.is_err(),
        "own device is compacted, not pruned"
    );
    let deleted = a.prune_device(lost_id).await.unwrap();
    assert_eq!(deleted.len(), 3);
    assert_eq!(
        env.count(&format!("{db_id}/devices/{lost_id}/seg-")).await,
        1
    );

    let late = env.open("late").await;
    late.join(db_id).await.unwrap();
    same(&a, &late).await;
    assert!(
        late.collection("notes")
            .unwrap()
            .get("n0")
            .await
            .unwrap()
            .is_some()
    );
}
