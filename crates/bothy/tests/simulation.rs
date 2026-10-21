//! Seeded multi-device simulation with injected faults, crashes, clock skew, snapshots,
//! compaction and restored copies (device-id forks).
//!
//! Every scenario ends with faults off, a full sync of all devices and a device that joins from
//! scratch. Then every device's canonical dump must equal the state computed directly from the
//! remote files (snapshots and segments), and no local op may be left behind.
//!
//! `BOTHY_SIM_SEEDS=<n>` sets the seeds per test (default 50, 10 tests). `BOTHY_SIM_SEED=<s>`
//! replays one seed; all ids derive from the seed, so a replay is exact. With
//! `BOTHY_SIM_TRACE=1` every step is printed, and a mismatch prints the differing documents.
//! `BOTHY_SIM_NO_CLONE=1` turns off restored copies, to tell fork bugs from others.
//! Every fourth seed runs an encrypted database (with the `crypto` feature).
#![cfg(feature = "test-util")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use bothy::codec::DataKey;
use bothy::codec::cbor::Encoder;
use bothy::codec::keys::{KeyKind, LogicalKey};
use bothy::codec::segment;
use bothy::codec::snapshot::{self, SnapshotReader};
use bothy::codec::state::encode_entry;
use bothy::merge::DocState;
use bothy::remote::faulty::{Faults, FaultyRemote, is_crash};
use bothy::remote::{FolderRemote, RemoteStore};
use bothy::store::{PackLimits, Store};
use bothy::{Clock, CollectionName, Config, Database, DbId, DeviceId, DocId, SnapshotPolicy};
use serde_json::json;

const DEVICES: usize = 3;
const STEPS: usize = 100;
const T0: u64 = 1_700_000_000_000;
const DAY: u64 = 24 * 60 * 60 * 1000;

fn trace() -> bool {
    std::env::var_os("BOTHY_SIM_TRACE").is_some()
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

/// Shared simulated time plus a per-device skew.
struct SimClock {
    base: Arc<AtomicU64>,
    skew: AtomicI64,
}

impl Clock for SimClock {
    fn now_ms(&self) -> u64 {
        self.base
            .load(Ordering::SeqCst)
            .saturating_add_signed(self.skew.load(Ordering::SeqCst))
    }
}

struct Device {
    path: PathBuf,
    clock: Arc<SimClock>,
    db: Option<Database>,
}

/// What happened during a run, to check that faults were really exercised.
#[derive(Default, Debug, Clone, Copy)]
struct Stats {
    crashes: u64,
    failed_syncs: u64,
    ok_syncs: u64,
    snapshots: u64,
    compacted_files: u64,
    forks: u64,
}

impl Stats {
    fn add(&mut self, o: Stats) {
        self.crashes += o.crashes;
        self.failed_syncs += o.failed_syncs;
        self.ok_syncs += o.ok_syncs;
        self.snapshots += o.snapshots;
        self.compacted_files += o.compacted_files;
        self.forks += o.forks;
    }
}

struct Sim {
    dir: tempfile::TempDir,
    remote: Arc<FaultyRemote<FolderRemote>>,
    base: Arc<AtomicU64>,
    devices: Vec<Device>,
    /// Device ids shared by a restored copy; their per-id op count check is skipped.
    cloned_ids: BTreeSet<DeviceId>,
    rng: Rng,
    seed: u64,
    stats: Stats,
}

impl Sim {
    fn new(seed: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let folder = FolderRemote::new(dir.path().join("remote")).unwrap();
        let faults = Faults {
            offline: 0.08,
            rate_limited: 0.04,
            lost_response: 0.1,
            list_lag: 3,
        };
        let remote = Arc::new(FaultyRemote::new(folder, faults, seed));
        let base = Arc::new(AtomicU64::new(T0));
        let mut sim = Self {
            dir,
            remote,
            base,
            devices: Vec::new(),
            cloned_ids: BTreeSet::new(),
            rng: Rng(seed ^ 0x5eed),
            seed,
            stats: Stats::default(),
        };
        for _ in 0..DEVICES {
            sim.add_device();
        }
        sim
    }

    fn add_device(&mut self) -> usize {
        let i = self.devices.len();
        self.devices.push(Device {
            path: self.dir.path().join(format!("d{i}.db")),
            clock: Arc::new(SimClock {
                base: self.base.clone(),
                skew: AtomicI64::new(0),
            }),
            db: None,
        });
        i
    }

    fn config(&self, i: usize) -> Config {
        // Small segments and frequent snapshots so packing, snapshots and compaction happen.
        let limits = PackLimits {
            max_ops: 4,
            max_plain_bytes: 300,
            compress: i.is_multiple_of(2),
        };
        let policy = SnapshotPolicy {
            every_segments: 6,
            every_bytes: u64::MAX,
            grace_period_days: 1,
        };
        // Seeded ids keep runs reproducible; each install has its own seed.
        let config = Config::new(&self.devices[i].path, "com.example.sim")
            .deterministic_ids(self.seed.wrapping_mul(1000) + i as u64)
            .clock(self.devices[i].clock.clone())
            .pack_limits(limits)
            .snapshot_policy(policy);
        #[cfg(feature = "crypto")]
        let config = match (self.encrypted(), i) {
            (true, 0) => config.kdf_cost(64, 1, 1).encrypt("sim"),
            (true, _) => config.kdf_cost(64, 1, 1).password("sim"),
            (false, _) => config,
        };
        config
    }

    fn encrypted(&self) -> bool {
        cfg!(feature = "crypto") && self.seed % 4 == 1
    }

    /// The data key the oracle needs to read an encrypted remote.
    async fn data_key(&mut self) -> Option<DataKey> {
        if !self.encrypted() {
            return None;
        }
        #[cfg(feature = "crypto")]
        return Some(self.db(0).await.export_key().unwrap());
        #[cfg(not(feature = "crypto"))]
        unreachable!()
    }

    async fn db(&mut self, i: usize) -> Database {
        if self.devices[i].db.is_none() {
            let db = Database::open(self.config(i), self.remote.clone())
                .await
                .unwrap();
            self.devices[i].db = Some(db);
        }
        self.devices[i].db.clone().unwrap()
    }

    fn crash(&mut self, i: usize) {
        self.devices[i].db = None;
    }

    async fn setup(&mut self) -> DbId {
        self.remote.set_enabled(false);
        let db_id = self.db(0).await.init().await.unwrap();
        for i in 1..DEVICES {
            self.db(i).await.join(db_id).await.unwrap();
        }
        self.remote.set_enabled(true);
        db_id
    }

    async fn write(&mut self, i: usize) {
        let db = self.db(i).await;
        let items = db.collection("items").unwrap();
        let id = format!("d{}", self.rng.below(5));
        let field = ["a", "b", "c"][self.rng.below(3) as usize];
        let value = self.rng.below(1000) as i64;
        match self.rng.below(10) {
            0..=3 => {
                let other = ["a", "b", "c"][self.rng.below(3) as usize];
                items
                    .put(&id, json!({ field: value, other: -value }))
                    .await
                    .unwrap();
            }
            4..=6 => {
                items
                    .patch(&id, json!({ field: value }), &[])
                    .await
                    .unwrap();
            }
            7 => {
                items.patch(&id, json!({}), &[field]).await.unwrap();
            }
            _ => {
                items.delete(&id).await.unwrap();
            }
        };
    }

    /// Records the outcome of a remote-touching call; a simulated crash drops the database.
    fn outcome<T>(&mut self, i: usize, r: bothy::Result<T>) -> Option<T> {
        match r {
            Ok(v) => {
                self.stats.ok_syncs += 1;
                Some(v)
            }
            Err(e) if is_crash(&e) => {
                self.stats.crashes += 1;
                self.crash(i);
                None
            }
            Err(_) => {
                self.stats.failed_syncs += 1;
                None
            }
        }
    }

    async fn sync(&mut self, i: usize) {
        let db = self.db(i).await;
        let r = db.sync().await;
        if trace() {
            eprintln!("  sync -> {r:?}");
        }
        if let Some(report) = self.outcome(i, r) {
            self.stats.snapshots += report.snapshots_up as u64;
            self.stats.compacted_files += report.deleted_files as u64;
            if report.forked_to.is_some() {
                self.stats.forks += 1;
            }
        }
    }

    /// Restores a copy of device `i`'s database file as a new device (same device id).
    async fn clone_device(&mut self, i: usize) {
        let id = self.db(i).await.device_id();
        self.crash(i);
        let j = self.add_device();
        std::fs::copy(&self.devices[i].path, &self.devices[j].path).unwrap();
        self.cloned_ids.insert(id);
    }

    async fn run(&mut self) -> Stats {
        let db_id = self.setup().await;
        let mut cloned = false;
        for step in 0..STEPS {
            let i = self.rng.below(self.devices.len() as u64) as usize;
            let action = self.rng.below(100);
            if trace() {
                let id = self.devices[i]
                    .db
                    .as_ref()
                    .map(|d| d.device_id().to_hex()[..6].to_owned());
                eprintln!("step {step}: device {i} ({id:?}) action {action}");
            }
            match action {
                0..=44 => self.write(i).await,
                45..=69 => self.sync(i).await,
                70..=77 => {
                    self.remote.crash_after(self.rng.below(4) as u32);
                    self.sync(i).await;
                    self.remote.disarm();
                }
                78..=80 => {
                    let db = self.db(i).await;
                    let r = db.compact(false).await;
                    if let Some(c) = self.outcome(i, r) {
                        self.stats.compacted_files += (c.segments.len() + c.snapshots.len()) as u64;
                    }
                }
                81..=82 => {
                    let db = self.db(i).await;
                    let r = db.snapshot().await;
                    if self.outcome(i, r).is_some() {
                        self.stats.snapshots += 1;
                    }
                }
                83..=85 => {
                    // A day or more passes: snapshots become old enough for compaction.
                    let ms = DAY + self.rng.below(DAY);
                    self.base.fetch_add(ms, Ordering::SeqCst);
                }
                86 if !cloned && std::env::var_os("BOTHY_SIM_NO_CLONE").is_none() => {
                    cloned = true;
                    self.clone_device(i).await;
                }
                86..=92 => {
                    let ms = self.rng.below(5_000);
                    self.base.fetch_add(ms, Ordering::SeqCst);
                }
                _ => {
                    // Clock skew: up to 10 minutes ahead or 10 seconds behind.
                    let skew = if self.rng.below(2) == 0 {
                        self.rng.below(600_000) as i64
                    } else {
                        -(self.rng.below(10_000) as i64)
                    };
                    self.devices[i].clock.skew.store(skew, Ordering::SeqCst);
                }
            }
        }
        self.finish(db_id).await;
        self.stats
    }

    async fn finish(&mut self, db_id: DbId) {
        let seed = self.seed;
        self.remote.set_enabled(false);
        for _ in 0..3 {
            for i in 0..self.devices.len() {
                let db = self.db(i).await;
                let r = db.sync().await;
                if trace() {
                    eprintln!(
                        "  final sync {i} ({}) -> {r:?}",
                        &db.device_id().to_hex()[..6]
                    );
                }
                r.unwrap_or_else(|e| panic!("seed {seed}: device {i}: {e}"));
            }
        }
        // A device joining now must reach the same state, from snapshots and the remaining segments.
        let late = self.add_device();
        self.db(late)
            .await
            .join(db_id)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: join: {e}"));

        let key = self.data_key().await;
        assert_eq!(
            self.db(0).await.is_encrypted().await.unwrap(),
            self.encrypted()
        );
        let expected = oracle(self.remote.inner(), db_id, key.as_ref()).await;
        for i in 0..self.devices.len() {
            let dump = self.db(i).await.canonical_dump().await.unwrap();
            if dump != expected {
                explain(&dump, &expected);
            }
            assert_eq!(
                dump, expected,
                "seed {seed}: device {i} differs from the remote state"
            );
        }
        // No local op was left behind: everything in the outbox reached the remote.
        for i in 0..self.devices.len() {
            self.crash(i);
            let store = Store::open(&self.devices[i].path, self.devices[i].clock.clone()).unwrap();
            assert!(
                store.unpacked_ops().unwrap().is_empty(),
                "seed {seed}: unpacked ops"
            );
            assert!(
                store.pending_uploads().unwrap().is_empty(),
                "seed {seed}: pending uploads"
            );
            let id = store.device_id();
            if !self.cloned_ids.contains(&id) {
                let remote_ops =
                    remote_op_count(self.remote.inner(), db_id, id, key.as_ref()).await;
                assert_eq!(
                    store.outbox_count().unwrap(),
                    remote_ops,
                    "seed {seed}: device {i} lost ops"
                );
            }
        }
    }
}

/// Prints the documents that differ between a device dump and the expected dump.
fn explain(got: &[u8], want: &[u8]) {
    fn entries(b: &[u8]) -> BTreeMap<(CollectionName, DocId), DocState> {
        let mut d = bothy::codec::cbor::Decoder::new(b);
        let n = d.array().unwrap();
        (0..n)
            .map(|_| {
                let (c, id, s) = bothy::codec::state::decode_entry(&mut d).unwrap();
                ((c, id), s)
            })
            .collect()
    }
    let (g, w) = (entries(got), entries(want));
    for key in g.keys().chain(w.keys()).collect::<BTreeSet<_>>() {
        if g.get(key) != w.get(key) {
            eprintln!(
                "differs: {key:?}\n  device: {:?}\n  remote: {:?}",
                g.get(key),
                w.get(key)
            );
        }
    }
}

/// State computed directly from every snapshot and segment in the remote. Merging is
/// idempotent and order-independent, so the union of all files gives the full state.
async fn oracle(remote: &FolderRemote, db_id: DbId, data_key: Option<&DataKey>) -> Vec<u8> {
    let mut states: BTreeMap<(CollectionName, DocId), DocState> = BTreeMap::new();
    for e in remote.list(&format!("{db_id}/")).await.unwrap() {
        let Some(key) = LogicalKey::parse(&e.key) else {
            continue;
        };
        let bytes = remote.get(&e.key).await.unwrap().unwrap();
        match key.kind {
            KeyKind::Segment(dev, seq) => {
                let seg = segment::decode_with(
                    &bytes,
                    &segment::Expected {
                        db_id,
                        device_id: dev,
                        seq,
                    },
                    data_key,
                )
                .unwrap();
                for op in seg.ops {
                    states
                        .entry((op.collection.clone(), op.doc_id.clone()))
                        .or_default()
                        .apply(&op.kind, op.stamp(dev));
                }
            }
            KeyKind::Snapshot(dev, hlc) => {
                let exp = snapshot::Expected {
                    db_id,
                    device_id: dev,
                    hlc,
                };
                SnapshotReader::open_with(&bytes, &exp, data_key)
                    .unwrap()
                    .for_each_entry(|c, d, s| {
                        states.entry((c, d)).or_default().merge(&s);
                        Ok(())
                    })
                    .unwrap();
            }
            _ => {}
        }
    }
    let mut enc = Encoder::new();
    enc.array(states.len());
    for ((c, d), s) in &states {
        encode_entry(&mut enc, c, d, s).unwrap();
    }
    enc.into_bytes()
}

async fn remote_op_count(
    remote: &FolderRemote,
    db_id: DbId,
    device: DeviceId,
    key: Option<&DataKey>,
) -> u64 {
    let mut n = 0;
    for e in remote
        .list(&format!("{db_id}/devices/{device}/"))
        .await
        .unwrap()
    {
        if let Some(LogicalKey {
            kind: KeyKind::Segment(_, seq),
            ..
        }) = LogicalKey::parse(&e.key)
        {
            let bytes = remote.get(&e.key).await.unwrap().unwrap();
            let exp = segment::Expected {
                db_id,
                device_id: device,
                seq,
            };
            n += segment::decode_with(&bytes, &exp, key).unwrap().ops.len() as u64;
        }
    }
    n
}

fn seeds_per_test() -> u64 {
    std::env::var("BOTHY_SIM_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
}

async fn run_range(block: u64) {
    if let Some(seed) = std::env::var("BOTHY_SIM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        if block == 0 {
            let s = Sim::new(seed).run().await;
            eprintln!("seed {seed}: {s:?}");
        }
        return;
    }
    let n = seeds_per_test();
    let mut total = Stats::default();
    for seed in block * n..(block + 1) * n {
        total.add(Sim::new(seed).run().await);
    }
    eprintln!("block {block}: {total:?}");
    if n >= 10 {
        let t = total;
        assert!(
            t.crashes > 0
                && t.failed_syncs > 0
                && t.ok_syncs > 0
                && t.snapshots > 0
                && t.compacted_files > 0
                && t.forks > 0,
            "not every fault or feature was exercised: {t:?}"
        );
    }
}

macro_rules! sim_blocks {
    ($($name:ident = $block:expr),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $name() {
                run_range($block).await;
            }
        )*
    };
}

sim_blocks!(
    seeds_block_0 = 0,
    seeds_block_1 = 1,
    seeds_block_2 = 2,
    seeds_block_3 = 3,
    seeds_block_4 = 4,
    seeds_block_5 = 5,
    seeds_block_6 = 6,
    seeds_block_7 = 7,
    seeds_block_8 = 8,
    seeds_block_9 = 9,
);
