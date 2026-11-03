//! Direct channels between two devices on loopback, signaling through a shared FolderRemote.
#![cfg(feature = "p2p")]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bothy::p2p::{P2pConfig, PeerState};
use bothy::remote::{ChangeBatch, FolderRemote, PutOutcome, RemoteEntry, RemoteStore};
use bothy::{AutoSync, Config, Database, Error, ErrorKind, Origin, Result};
use serde_json::json;
use tempfile::TempDir;

const APP: &str = "com.example.notes";

/// Counts downloads of segment files and can refuse them, to tell P2P delivery from a pull.
struct Counting {
    inner: Arc<FolderRemote>,
    segment_gets: AtomicUsize,
    refuse_segments: AtomicBool,
}

#[async_trait]
impl RemoteStore for Counting {
    async fn list(&self, prefix: &str) -> Result<Vec<RemoteEntry>> {
        self.inner.list(prefix).await
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        if key.ends_with(".bseg") {
            self.segment_gets.fetch_add(1, Ordering::SeqCst);
            if self.refuse_segments.load(Ordering::SeqCst) {
                return Err(Error::new(ErrorKind::Offline, "segment downloads are off"));
            }
        }
        self.inner.get(key).await
    }
    async fn put_new(
        &self,
        key: &str,
        data: &[u8],
        reservation: Option<&str>,
    ) -> Result<PutOutcome> {
        self.inner.put_new(key, data, reservation).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key).await
    }
    async fn changes_since(&self, cursor: Option<&str>) -> Result<Option<ChangeBatch>> {
        self.inner.changes_since(cursor).await
    }
}

struct Pair {
    _dir: TempDir,
    folder: Arc<FolderRemote>,
    a: Database,
    b: Database,
    b_remote: Arc<Counting>,
}

fn p2p() -> P2pConfig {
    P2pConfig::new()
        .stun_servers(vec![])
        .host_address(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn auto() -> AutoSync {
    AutoSync {
        debounce: Duration::from_millis(100),
        p2p_debounce: Duration::from_millis(50),
        poll_interval: Duration::from_millis(500),
        ..AutoSync::default()
    }
}

async fn pair(p2p: P2pConfig, password: Option<&str>) -> Pair {
    let dir = tempfile::tempdir().unwrap();
    let folder = Arc::new(FolderRemote::new(dir.path().join("remote")).unwrap());
    let cfg = |name: &str| {
        let c = Config::new(dir.path().join(format!("{name}.db")), APP)
            .device_name(name)
            .p2p(p2p.clone());
        match password {
            #[cfg(feature = "crypto")]
            Some(pw) => c.kdf_cost(64, 1, 1).encrypt(pw),
            _ => c,
        }
    };
    let a = Database::open(cfg("a"), folder.clone()).await.unwrap();
    let db_id = a.init().await.unwrap();
    let b_remote = Arc::new(Counting {
        inner: folder.clone(),
        segment_gets: AtomicUsize::new(0),
        refuse_segments: AtomicBool::new(false),
    });
    let b = Database::open(cfg("b"), b_remote.clone()).await.unwrap();
    b.join(db_id).await.unwrap();
    Pair {
        _dir: dir,
        folder,
        a,
        b,
        b_remote,
    }
}

async fn wait_for(what: &str, limit: Duration, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn connected(db: &Database) -> bool {
    db.p2p_status()
        .peers
        .iter()
        .any(|p| p.state == PeerState::Connected)
}

/// Waits for a remote change to `id` on `db`; returns how long it took.
async fn arrival(db: &Database, id: &str, start: Instant) -> Duration {
    let mut events = db.subscribe();
    let id = id.to_owned();
    tokio::time::timeout(Duration::from_secs(20), async move {
        loop {
            let e = events.recv().await.unwrap();
            if e.origin == Origin::Remote && e.doc_id.as_str() == id {
                return start.elapsed();
            }
        }
    })
    .await
    .expect("the change never arrived")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_arrive_over_the_channel() {
    let p = pair(p2p(), None).await;
    let ha = p.a.start_auto_sync(auto());
    let hb = p.b.start_auto_sync(auto());
    wait_for("both channels", Duration::from_secs(30), || {
        connected(&p.a) && connected(&p.b)
    })
    .await;

    // B keeps polling, and a poll may see the segment before the peer's copy arrives. With
    // downloads refused such a poll just fails, and only the channel can deliver the write.
    p.b_remote.refuse_segments.store(true, Ordering::SeqCst);
    let start = Instant::now();
    let waiting = arrival(&p.b, "n1", start);
    p.a.collection("notes")
        .unwrap()
        .put("n1", json!({"title": "fast"}))
        .await
        .unwrap();
    let took = waiting.await;
    assert!(took < Duration::from_secs(2), "took {took:?}");
    p.b_remote.refuse_segments.store(false, Ordering::SeqCst);
    // And still on the remote, the durable record.
    let segs = p.folder.list("").await.unwrap();
    assert!(
        segs.iter()
            .any(|e| e.key.contains(&format!("devices/{}/seg-", p.a.device_id())))
    );

    ha.stop().await;
    hb.stop().await;
    // Stopping removes our signaling files.
    let left: Vec<_> = p
        .folder
        .list("")
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.key.contains("/signaling/") && e.key.contains("/online-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announced_segments_are_pulled() {
    let p = pair(p2p().inline_segments(false), None).await;
    let ha = p.a.start_auto_sync(auto());
    let hb = p.b.start_auto_sync(AutoSync {
        // Polling alone would take a minute: the hint must trigger the pull.
        poll_interval: Duration::from_secs(60),
        ..auto()
    });
    // Peers are found through polls; with B polling once a minute, sync it by hand until
    // the channel is up.
    let start = Instant::now();
    while !(connected(&p.a) && connected(&p.b)) {
        assert!(start.elapsed() < Duration::from_secs(30), "no channel");
        p.b.sync().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let start = Instant::now();
    let waiting = arrival(&p.b, "n2", start);
    p.a.collection("notes")
        .unwrap()
        .put("n2", json!({"title": "hinted"}))
        .await
        .unwrap();
    let took = waiting.await;
    assert!(took < Duration::from_secs(10), "took {took:?}");
    assert!(p.b_remote.segment_gets.load(Ordering::SeqCst) > 0);
    ha.stop().await;
    hb.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn falls_back_to_the_remote_when_the_peer_goes_away() {
    let p = pair(p2p(), None).await;
    let ha = p.a.start_auto_sync(auto());
    let hb = p.b.start_auto_sync(auto());
    wait_for("both channels", Duration::from_secs(30), || {
        connected(&p.a) && connected(&p.b)
    })
    .await;
    // A stops background sync (and P2P) but keeps writing and syncing by hand.
    ha.stop().await;
    assert!(!p.a.p2p_status().running);
    let start = Instant::now();
    let waiting = arrival(&p.b, "n3", start);
    p.a.collection("notes")
        .unwrap()
        .put("n3", json!({"title": "via the remote"}))
        .await
        .unwrap();
    p.a.sync().await.unwrap();
    waiting.await;
    hb.stop().await;
}

#[cfg(feature = "crypto")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encrypted_signaling_leaks_no_addresses() {
    let p = pair(p2p(), Some("pw")).await;
    let ha = p.a.start_auto_sync(auto());
    let hb = p.b.start_auto_sync(auto());
    wait_for("both channels", Duration::from_secs(30), || {
        connected(&p.a) && connected(&p.b)
    })
    .await;
    let mut seen = 0;
    for e in p.folder.list("").await.unwrap() {
        if !e.key.contains("/signaling/") {
            continue;
        }
        let Some(body) = p.folder.get(&e.key).await.unwrap() else {
            continue;
        };
        seen += 1;
        let text = String::from_utf8_lossy(&body);
        for needle in ["127.0.0.1", "candidate", "v=0", "session"] {
            assert!(!text.contains(needle), "{}: {needle} in plain text", e.key);
        }
    }
    assert!(seen > 0, "no signaling files to check");
    ha.stop().await;
    hb.stop().await;
}
