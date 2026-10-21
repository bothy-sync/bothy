//! Encrypted databases: unlocking, password changes, and no plaintext on the remote.
#![cfg(feature = "crypto")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bothy::remote::{FolderRemote, PutOutcome, RemoteStore};
use bothy::store::PackLimits;
use bothy::{Config, Database, DbId, ErrorKind};
use serde_json::json;
use tempfile::TempDir;

const APP: &str = "com.example.secret";
const MARKERS: &[&str] = &[
    "secretcoll",
    "secretdocid",
    "secretfield",
    "SECRET-VALUE-42",
    "secret-host",
];

struct Env {
    dir: TempDir,
    remote: Arc<FolderRemote>,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let remote = Arc::new(FolderRemote::new(dir.path().join("remote")).unwrap());
        Self { dir, remote }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(format!("{name}.db"))
    }

    fn config(&self, name: &str) -> Config {
        // Cheap Argon2id for tests; no compression so the plaintext scan is meaningful.
        Config::new(self.path(name), APP)
            .device_name("secret-host")
            .kdf_cost(64, 1, 1)
            .pack_limits(PackLimits {
                compress: false,
                ..PackLimits::default()
            })
    }

    async fn open(&self, config: Config) -> bothy::Result<Database> {
        Database::open(config, self.remote.clone()).await
    }

    async fn creator(&self, password: &str) -> (Database, DbId) {
        let a = self.open(self.config("a").encrypt(password)).await.unwrap();
        let db_id = a.init().await.unwrap();
        (a, db_id)
    }
}

fn all_files(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            all_files(&p, out);
        } else {
            out.push((p.clone(), std::fs::read(&p).unwrap()));
        }
    }
}

fn assert_no_plaintext(remote: &Path) {
    let mut files = Vec::new();
    all_files(remote, &mut files);
    assert!(files.len() > 3, "expected several remote files");
    for (path, bytes) in files {
        for m in MARKERS {
            assert!(
                !bytes.windows(m.len()).any(|w| w == m.as_bytes()),
                "{} contains plaintext {m:?}",
                path.display()
            );
        }
    }
}

async fn same(a: &Database, b: &Database) {
    assert_eq!(
        a.canonical_dump().await.unwrap(),
        b.canonical_dump().await.unwrap()
    );
}

#[tokio::test]
async fn two_devices_share_an_encrypted_database_without_plaintext_on_the_remote() {
    let env = Env::new();
    let (a, db_id) = env.creator("correct horse").await;
    assert!(a.is_encrypted().await.unwrap() && !a.is_locked().await.unwrap());
    a.collection("secretcoll")
        .unwrap()
        .put("secretdocid", json!({"secretfield": "SECRET-VALUE-42"}))
        .await
        .unwrap();
    a.sync().await.unwrap();
    a.snapshot().await.unwrap();

    let b = env
        .open(env.config("b").password("correct horse"))
        .await
        .unwrap();
    b.join(db_id).await.unwrap();
    b.collection("secretcoll")
        .unwrap()
        .patch("secretdocid", json!({"other": "SECRET-VALUE-42"}), &[])
        .await
        .unwrap();
    b.sync().await.unwrap();
    a.sync().await.unwrap();
    same(&a, &b).await;
    let doc = a
        .collection("secretcoll")
        .unwrap()
        .get("secretdocid")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        doc.to_json(),
        json!({"secretfield": "SECRET-VALUE-42", "other": "SECRET-VALUE-42"})
    );

    assert_no_plaintext(&env.dir.path().join("remote"));
}

#[tokio::test]
async fn wrong_or_missing_password_is_refused() {
    let env = Env::new();
    let (_a, db_id) = env.creator("right").await;
    let b = env.open(env.config("b").password("wrong")).await.unwrap();
    assert_eq!(
        b.join(db_id).await.unwrap_err().kind(),
        ErrorKind::WrongPassword
    );
    let c = env.open(env.config("c")).await.unwrap();
    assert_eq!(
        c.join(db_id).await.unwrap_err().kind(),
        ErrorKind::KeyRequired
    );
    assert_eq!(
        c.db_id().await.unwrap(),
        None,
        "a failed join leaves the device unattached"
    );
}

#[tokio::test]
async fn reopening_locks_until_unlocked_and_unlock_works_offline() {
    let env = Env::new();
    let (a, _) = env.creator("pw").await;
    a.collection("notes")
        .unwrap()
        .put("n1", json!({"x": 1}))
        .await
        .unwrap();
    drop(a);

    let a = env.open(env.config("a")).await.unwrap();
    assert!(a.is_locked().await.unwrap());
    assert_eq!(a.sync().await.unwrap_err().kind(), ErrorKind::KeyRequired);
    // Local reads work while locked: the local database is not encrypted.
    assert!(
        a.collection("notes")
            .unwrap()
            .get("n1")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        a.unlock("nope").await.unwrap_err().kind(),
        ErrorKind::WrongPassword
    );
    a.unlock("pw").await.unwrap();
    a.sync().await.unwrap();
    a.lock();
    assert!(a.is_locked().await.unwrap());
    drop(a);

    // With the password in the config it unlocks on open, without the remote.
    let offline = Arc::new(FolderRemote::new(env.dir.path().join("nowhere")).unwrap());
    let a = Database::open(env.config("a").password("pw"), offline)
        .await
        .unwrap();
    assert!(!a.is_locked().await.unwrap());
}

#[tokio::test]
async fn password_change_retires_the_old_password_but_not_cached_keys() {
    let env = Env::new();
    let (a, db_id) = env.creator("old").await;
    let b = env.open(env.config("b").password("old")).await.unwrap();
    b.join(db_id).await.unwrap();
    let cached = b.export_key().unwrap();

    a.change_password("wrong", "new").await.unwrap_err();
    a.change_password("old", "new").await.unwrap();
    let report = b.sync().await.unwrap();
    assert!(report.password_changed);
    // b's own key file is gone; only a's newest remains.
    let keys = env.remote.list(&format!("{db_id}/keys/")).await.unwrap();
    assert_eq!(keys.len(), 1, "{keys:?}");

    let c = env.open(env.config("c").password("old")).await.unwrap();
    assert_eq!(
        c.join(db_id).await.unwrap_err().kind(),
        ErrorKind::WrongPassword
    );
    let d = env.open(env.config("d").password("new")).await.unwrap();
    d.join(db_id).await.unwrap();

    // A key cached before the change still unlocks (the data key did not change).
    drop(b);
    let b = env.open(env.config("b").key(cached)).await.unwrap();
    assert!(!b.is_locked().await.unwrap());
    b.collection("notes")
        .unwrap()
        .put("after", json!({"ok": true}))
        .await
        .unwrap();
    b.sync().await.unwrap();
    d.sync().await.unwrap();
    assert!(
        d.collection("notes")
            .unwrap()
            .get("after")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn plaintext_or_tampered_segments_are_skipped() {
    let env = Env::new();
    let (a, db_id) = env.creator("pw").await;
    let b = env.open(env.config("b").password("pw")).await.unwrap();
    b.join(db_id).await.unwrap();

    // Someone with Drive access drops a plaintext segment under a's id.
    let plain = bothy::codec::segment::encode(
        &bothy::codec::segment::Segment {
            db_id,
            device_id: a.device_id(),
            seq: 1,
            ops: vec![bothy::Op {
                hlc: bothy::Hlc::from_u64(1),
                collection: bothy::CollectionName::new("notes").unwrap(),
                doc_id: bothy::DocId::new("evil").unwrap(),
                kind: bothy::OpKind::DeleteDoc,
            }],
        },
        false,
    )
    .unwrap();
    let key = format!("{db_id}/devices/{}/seg-{:016x}.bseg", a.device_id(), 1);
    assert_eq!(
        env.remote.put_new(&key, &plain, None).await.unwrap(),
        PutOutcome::Created
    );
    let report = b.sync().await.unwrap();
    assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
    assert_eq!(report.segments_down, 0);
}

#[tokio::test]
async fn the_plaintext_scan_does_find_markers_in_an_unencrypted_database() {
    let env = Env::new();
    let a = env.open(env.config("a")).await.unwrap();
    a.init().await.unwrap();
    a.collection("secretcoll")
        .unwrap()
        .put("secretdocid", json!({"secretfield": "SECRET-VALUE-42"}))
        .await
        .unwrap();
    a.sync().await.unwrap();
    let remote = env.dir.path().join("remote");
    let caught = std::panic::catch_unwind(|| assert_no_plaintext(&remote));
    assert!(
        caught.is_err(),
        "the scan must flag an unencrypted database"
    );
}
