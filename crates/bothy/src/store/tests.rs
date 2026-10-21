use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::codec::cbor::Decoder;

const T0: u64 = 1_700_000_000_000;

#[derive(Debug)]
struct FakeClock(AtomicU64);

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(FakeClock(AtomicU64::new(T0)))
    }
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn coll() -> CollectionName {
    CollectionName::new("notes").unwrap()
}

fn id(s: &str) -> DocId {
    DocId::new(s).unwrap()
}

fn f(s: &str) -> FieldName {
    FieldName::new(s).unwrap()
}

fn doc(pairs: &[(&str, Value)]) -> Document {
    pairs.iter().map(|(k, v)| (f(k), v.clone())).collect()
}

fn open(dir: &tempfile::TempDir, name: &str, clock: Arc<FakeClock>) -> Store {
    Store::open(dir.path().join(name), clock).unwrap()
}

#[test]
fn put_get_and_full_replace() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&dir, "a.db", FakeClock::new());
    assert!(
        s.put(
            &coll(),
            &id("n1"),
            doc(&[("title", "Hi".into()), ("pinned", false.into())])
        )
        .unwrap()
    );
    assert_eq!(
        s.get(&coll(), &id("n1")).unwrap().unwrap(),
        doc(&[("pinned", false.into()), ("title", "Hi".into())])
    );
    // Same content again: no new operations.
    assert!(
        !s.put(
            &coll(),
            &id("n1"),
            doc(&[("title", "Hi".into()), ("pinned", false.into())])
        )
        .unwrap()
    );
    assert_eq!(s.unpacked_ops().unwrap().len(), 1);
    // Full replace unsets the missing field.
    assert!(
        s.put(&coll(), &id("n1"), doc(&[("title", "Yo".into())]))
            .unwrap()
    );
    assert_eq!(
        s.get(&coll(), &id("n1")).unwrap().unwrap(),
        doc(&[("title", "Yo".into())])
    );
    let kinds: Vec<u64> = s
        .unpacked_ops()
        .unwrap()
        .iter()
        .map(|(_, op)| op.kind.code())
        .collect();
    assert_eq!(kinds, vec![0, 0, 1]);
}

#[test]
fn patch_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&dir, "a.db", FakeClock::new());
    s.put(&coll(), &id("n1"), doc(&[("a", 1.into()), ("b", 2.into())]))
        .unwrap();
    s.patch(&coll(), &id("n1"), doc(&[("c", 3.into())]), [f("a")].into())
        .unwrap();
    assert_eq!(
        s.get(&coll(), &id("n1")).unwrap().unwrap(),
        doc(&[("b", 2.into()), ("c", 3.into())])
    );
    assert!(
        s.patch(&coll(), &id("n1"), doc(&[("c", 1.into())]), [f("c")].into())
            .is_err()
    );
    assert!(s.delete(&coll(), &id("n1")).unwrap());
    assert_eq!(s.get(&coll(), &id("n1")).unwrap(), None);
    assert!(!s.delete(&coll(), &id("n1")).unwrap());
    // The tombstone stays in the merge state.
    assert!(
        s.doc_state(&coll(), &id("n1"))
            .unwrap()
            .deleted_at
            .is_some()
    );
}

#[test]
fn reserved_names_are_rejected_for_local_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&dir, "a.db", FakeClock::new());
    let reserved = CollectionName::new("_bothy_x").unwrap();
    assert_eq!(
        s.put(&reserved, &id("x"), doc(&[("a", 1.into())]))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert!(s.put(&coll(), &id("x"), doc(&[("_a", 1.into())])).is_err());
}

#[test]
fn list_pages_in_id_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&dir, "a.db", FakeClock::new());
    for n in ["c", "a", "d", "b"] {
        s.put(&coll(), &id(n), doc(&[("n", n.into())])).unwrap();
    }
    s.delete(&coll(), &id("c")).unwrap();
    let page1: Vec<String> = s
        .list(&coll(), 2, None)
        .unwrap()
        .into_iter()
        .map(|(i, _)| i.to_string())
        .collect();
    assert_eq!(page1, ["a", "b"]);
    let page2: Vec<String> = s
        .list(&coll(), 2, Some(&id("b")))
        .unwrap()
        .into_iter()
        .map(|(i, _)| i.to_string())
        .collect();
    assert_eq!(page2, ["d"]);
}

#[test]
fn outbox_hlcs_strictly_increase_even_if_the_clock_goes_back() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::new();
    let mut s = open(&dir, "a.db", clock.clone());
    s.put(&coll(), &id("a"), doc(&[("x", 1.into())])).unwrap();
    clock.0.store(T0 - 60_000, Ordering::SeqCst);
    s.put(&coll(), &id("a"), doc(&[("y", 1.into())])).unwrap();
    let hlcs: Vec<Hlc> = s
        .unpacked_ops()
        .unwrap()
        .iter()
        .map(|(_, op)| op.hlc)
        .collect();
    assert_eq!(hlcs.len(), 3); // set x; set y + unset x
    assert!(hlcs.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn reopen_keeps_device_id_and_clock() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::new();
    let (device, last) = {
        let mut s = open(&dir, "a.db", clock.clone());
        s.put(&coll(), &id("a"), doc(&[("x", 1.into())])).unwrap();
        (s.device_id(), s.last_hlc())
    };
    clock.0.store(T0 - 1_000, Ordering::SeqCst);
    let mut s = open(&dir, "a.db", clock);
    assert_eq!(s.device_id(), device);
    s.put(&coll(), &id("a"), doc(&[("x", 2.into())])).unwrap();
    assert!(s.last_hlc() > last);
    assert_eq!(
        s.get(&coll(), &id("a")).unwrap().unwrap(),
        doc(&[("x", 2.into())])
    );
}

#[test]
fn second_open_is_database_locked() {
    let dir = tempfile::tempdir().unwrap();
    let _a = open(&dir, "a.db", FakeClock::new());
    let err = Store::open(dir.path().join("a.db"), FakeClock::new()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::DatabaseLocked);
}

fn ship(from: &Store, to: &mut Store, seq: u64) -> Option<Vec<(CollectionName, DocId)>> {
    let ops: Vec<Op> = from
        .unpacked_ops()
        .unwrap()
        .into_iter()
        .map(|(_, op)| op)
        .collect();
    to.apply_remote(from.device_id(), seq, &ops).unwrap()
}

#[test]
fn remote_ops_merge_and_converge() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::new();
    let mut a = open(&dir, "a.db", clock.clone());
    let mut b = open(&dir, "b.db", clock.clone());
    a.put(
        &coll(),
        &id("n1"),
        doc(&[("title", "A".into()), ("tags", "x".into())]),
    )
    .unwrap();
    clock.advance(10);
    b.put(&coll(), &id("n1"), doc(&[("title", "B".into())]))
        .unwrap();

    let changed = ship(&a, &mut b, 1).unwrap();
    assert_eq!(changed, vec![(coll(), id("n1"))]);
    ship(&b, &mut a, 1).unwrap();

    assert_eq!(a.canonical_dump().unwrap(), b.canonical_dump().unwrap());
    // b's full put came later: it wins "title" and unsets nothing it never saw, so "tags" survives.
    assert_eq!(
        a.get(&coll(), &id("n1")).unwrap().unwrap(),
        doc(&[("tags", "x".into()), ("title", "B".into())])
    );
    let conflicts = b.conflicts(&coll(), &id("n1")).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].loser_value, Some("A".into()));
}

#[test]
fn applying_a_segment_twice_is_a_no_op_and_watermark_advances_without_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = open(&dir, "a.db", FakeClock::new());
    let mut b = open(&dir, "b.db", FakeClock::new());
    a.put(&coll(), &id("n1"), doc(&[("x", 1.into())])).unwrap();
    let ops: Vec<Op> = a
        .unpacked_ops()
        .unwrap()
        .into_iter()
        .map(|(_, op)| op)
        .collect();

    assert!(b.apply_remote(a.device_id(), 2, &ops).unwrap().is_some());
    assert_eq!(b.watermark(a.device_id()).unwrap(), 0); // seq 1 still missing
    assert!(b.apply_remote(a.device_id(), 2, &ops).unwrap().is_none());
    b.apply_remote(a.device_id(), 1, &ops).unwrap();
    assert_eq!(b.watermark(a.device_id()).unwrap(), 2);
}

#[test]
fn remote_apply_moves_the_local_clock_past_remote_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let ahead = FakeClock::new();
    ahead.advance(5_000);
    let mut a = open(&dir, "a.db", ahead);
    let mut b = open(&dir, "b.db", FakeClock::new());
    a.put(&coll(), &id("n1"), doc(&[("x", 1.into())])).unwrap();
    ship(&a, &mut b, 1);
    b.put(&coll(), &id("n1"), doc(&[("x", 2.into())])).unwrap();
    // b's later write wins even though its wall clock is behind.
    assert_eq!(
        b.get(&coll(), &id("n1")).unwrap().unwrap(),
        doc(&[("x", 2.into())])
    );
    assert!(b.last_hlc() > a.last_hlc());
}

#[test]
fn own_segments_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = open(&dir, "a.db", FakeClock::new());
    let err = a.apply_remote(a.device_id(), 1, &[]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::RemoteInconsistent);
}

#[test]
fn canonical_dump_is_independent_of_arrival_order() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::new();
    let mut a = open(&dir, "a.db", clock.clone());
    let mut b = open(&dir, "b.db", clock.clone());
    let mut c1 = open(&dir, "c1.db", clock.clone());
    let mut c2 = open(&dir, "c2.db", clock.clone());
    a.put(&coll(), &id("n1"), doc(&[("x", 1.into())])).unwrap();
    a.delete(&coll(), &id("n1")).unwrap();
    clock.advance(1);
    b.put(&coll(), &id("n1"), doc(&[("y", 2.into())])).unwrap();
    b.put(&coll(), &id("n2"), doc(&[("z", 3.into())])).unwrap();

    ship(&a, &mut c1, 1);
    ship(&b, &mut c1, 1);
    ship(&b, &mut c2, 1);
    ship(&a, &mut c2, 1);
    let dump = c1.canonical_dump().unwrap();
    assert_eq!(dump, c2.canonical_dump().unwrap());

    // The dump is a CBOR array of entries in (collection, doc_id) order.
    let mut d = Decoder::new(&dump);
    assert_eq!(d.array().unwrap(), 2);
    let (_, first, state) = crate::codec::state::decode_entry(&mut d).unwrap();
    assert_eq!(first, id("n1"));
    assert!(state.deleted_at.is_some());
}

#[test]
fn json_view_tracks_visible_documents() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&dir, "a.db", FakeClock::new());
    s.put(
        &coll(),
        &id("n1"),
        doc(&[("title", "Hi".into()), ("n", 2.into())]),
    )
    .unwrap();
    let json: String = s
        .conn
        .query_row("SELECT json FROM docs WHERE doc_id = 'n1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(json, r#"{"n":2,"title":"Hi"}"#);
    s.delete(&coll(), &id("n1")).unwrap();
    let rows: i64 = s
        .conn
        .query_row("SELECT COUNT(*) FROM docs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn conflict_log_spans_documents_and_honours_its_limit() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::new();
    let mut a = open(&dir, "a.db", clock.clone());
    let mut b = open(&dir, "b.db", clock.clone());
    let mut c = open(&dir, "c.db", clock.clone());
    for i in 0..5 {
        a.put(&coll(), &id(&format!("n{i}")), doc(&[("t", "A".into())]))
            .unwrap();
    }
    clock.advance(10);
    for s in [&mut b, &mut c] {
        for i in 0..5 {
            s.put(
                &coll(),
                &id(&format!("n{i}")),
                doc(&[("t", "later".into())]),
            )
            .unwrap();
        }
    }
    b.set_conflict_log(Some(3));
    c.set_conflict_log(None);
    let start = clock.now_ms();
    ship(&a, &mut b, 1).unwrap();
    ship(&a, &mut c, 1).unwrap();

    let all = b.recent_conflicts(None).unwrap();
    let ids: Vec<_> = all.iter().map(|x| x.doc_id.as_str()).collect();
    assert_eq!(ids, ["n4", "n3", "n2"], "newest first, oldest trimmed");
    assert!(
        all.iter()
            .all(|x| x.collection == coll() && x.at_ms == start)
    );
    assert_eq!(all[0].loser_value, Some("A".into()));
    assert_eq!(b.recent_conflicts(Some(start)).unwrap().len(), 3);
    assert!(b.recent_conflicts(Some(start + 1)).unwrap().is_empty());
    assert!(c.recent_conflicts(None).unwrap().is_empty(), "logging off");

    assert_eq!(b.clear_conflicts().unwrap(), 3);
    assert!(b.recent_conflicts(None).unwrap().is_empty());
}
