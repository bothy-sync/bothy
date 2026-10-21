//! Builds every vector file from fixed inputs.

use std::collections::BTreeMap;

use bothy::codec::cbor::{Encoder, encode_value};
use bothy::codec::manifest::{Compression, Manifest};
use bothy::codec::segment::{self, Segment};
use bothy::codec::snapshot::{SnapshotHeader, SnapshotWriter};
use bothy::codec::{encode_op, keys::LogicalKey};
use bothy::merge::DocState;
use bothy::{
    CollectionName, DbId, DeviceId, DocId, FieldName, Hlc, HlcClock, Op, OpKind, Stamp, Value,
};
use serde_json::{Value as J, json};

use crate::typed::{entry_to, hex, op_to, value_to};

/// Fixed database id used by all vectors.
pub const DB: DbId = DbId::from_bytes([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
/// Fixed device ids.
pub const DEV_A: DeviceId = DeviceId::from_bytes([0xa1; 16]);
/// Second device.
pub const DEV_B: DeviceId = DeviceId::from_bytes([0xb2; 16]);
/// Third device.
pub const DEV_C: DeviceId = DeviceId::from_bytes([0xc3; 16]);

const T: u64 = 1_700_000_000_000;

fn h(physical: u64, logical: u16) -> Hlc {
    Hlc::from_parts(physical, logical).expect("valid parts")
}

fn coll(s: &str) -> CollectionName {
    CollectionName::new(s).expect("valid collection")
}

fn doc(s: &str) -> DocId {
    DocId::new(s).expect("valid doc id")
}

fn f(s: &str) -> FieldName {
    FieldName::new(s).expect("valid field")
}

fn set(hlc: Hlc, c: &str, d: &str, fields: &[(&str, Value)]) -> Op {
    Op {
        hlc,
        collection: coll(c),
        doc_id: doc(d),
        kind: OpKind::SetFields(fields.iter().map(|(k, v)| (f(k), v.clone())).collect()),
    }
}

fn unset(hlc: Hlc, c: &str, d: &str, fields: &[&str]) -> Op {
    Op {
        hlc,
        collection: coll(c),
        doc_id: doc(d),
        kind: OpKind::UnsetFields(fields.iter().map(|k| f(k)).collect()),
    }
}

fn delete(hlc: Hlc, c: &str, d: &str) -> Op {
    Op {
        hlc,
        collection: coll(c),
        doc_id: doc(d),
        kind: OpKind::DeleteDoc,
    }
}

fn op_bytes(op: &Op) -> Vec<u8> {
    let mut e = Encoder::new();
    encode_op(&mut e, op).expect("valid op");
    e.into_bytes()
}

fn hlc_vectors() -> J {
    let parts: Vec<J> = [(T, 1u16), (0, 0), ((1 << 48) - 1, u16::MAX), (T + 123, 456)]
        .iter()
        .map(|&(p, l)| {
            let x = h(p, l);
            json!({"physical": p, "logical": l, "u64": x.as_u64().to_string(), "hex": x.to_hex()})
        })
        .collect();

    enum Step {
        Now(u64),
        Observe(Hlc, u64),
    }
    let scenarios: Vec<(&str, Hlc, u64, Vec<Step>)> = vec![
        (
            "same_millisecond_and_clock_going_back",
            Hlc::ZERO,
            3_600_000,
            vec![
                Step::Now(T),
                Step::Now(T),
                Step::Now(T - 5_000),
                Step::Now(T + 1),
            ],
        ),
        (
            "logical_overflow_borrows_next_ms",
            h(T, u16::MAX),
            3_600_000,
            vec![Step::Now(T)],
        ),
        (
            "observe_moves_past_remote",
            Hlc::ZERO,
            3_600_000,
            vec![Step::Now(T), Step::Observe(h(T + 10, 3), T), Step::Now(T)],
        ),
        (
            "observe_clamps_far_future",
            Hlc::ZERO,
            1_000,
            vec![Step::Observe(h(T + 3_600_000, 0), T), Step::Now(T)],
        ),
        (
            "wall_clock_beyond_48_bits",
            Hlc::ZERO,
            3_600_000,
            vec![Step::Now((1 << 48) + 5)],
        ),
    ];
    let clocks: Vec<J> = scenarios
        .into_iter()
        .map(|(name, start, drift, steps)| {
            let mut c = HlcClock::resume(start).with_max_drift_ms(drift);
            let steps: Vec<J> = steps
                .into_iter()
                .map(|s| match s {
                    Step::Now(w) => json!({"now": w, "expect": c.now(w).to_hex()}),
                    Step::Observe(r, w) => {
                        json!({"observe": r.to_hex(), "wall": w, "expect": c.observe(r, w).to_hex()})
                    }
                })
                .collect();
            json!({"name": name, "start": start.to_hex(), "max_drift_ms": drift, "steps": steps})
        })
        .collect();
    json!({"parts": parts, "clock": clocks})
}

/// Inputs every decoder must reject.
pub const INVALID_VALUES: &[(&str, &str)] = &[
    ("non_shortest_uint8", "1817"),
    ("non_shortest_uint16", "190017"),
    ("non_shortest_uint32", "1a0000ffff"),
    ("non_shortest_uint64", "1b00000000ffffffff"),
    ("non_shortest_negative", "3817"),
    ("non_shortest_length", "5801ff"),
    ("reserved_additional_info", "1c"),
    ("indefinite_array", "9fff"),
    ("indefinite_text", "7f6161ff"),
    ("tag", "c000"),
    ("undefined", "f7"),
    ("simple_16", "f0"),
    ("simple_24", "f818"),
    ("nan", "f97e00"),
    ("infinity", "f97c00"),
    ("negative_zero", "f98000"),
    ("float_1_as_f32", "fa3f800000"),
    ("float_1_as_f64", "fb3ff0000000000000"),
    ("f32_max_as_f64", "fb47efffffe0000000"),
    ("unsorted_map_keys", "a2616201616102"),
    ("duplicate_map_keys", "a2616101616102"),
    ("longer_key_before_shorter", "a262616101616202"),
    ("integer_map_key", "a1010102"),
    ("uint_above_i64_max", "1bffffffffffffffff"),
    ("negative_below_i64_min", "3b8000000000000000"),
    ("invalid_utf8", "62c328"),
    ("truncated", "1901"),
    ("array_longer_than_input", "9a7fffffff"),
    ("trailing_bytes", "0000"),
];

fn value_vectors() -> J {
    let mut nested = Value::Null;
    for _ in 0..32 {
        nested = Value::Array(vec![nested]);
    }
    let cases: Vec<(&str, Value)> = vec![
        ("null", Value::Null),
        ("false", Value::Bool(false)),
        ("true", Value::Bool(true)),
        ("int_0", Value::Int(0)),
        ("int_23", Value::Int(23)),
        ("int_24", Value::Int(24)),
        ("int_255", Value::Int(255)),
        ("int_256", Value::Int(256)),
        ("int_65536", Value::Int(65536)),
        ("int_2_pow_32", Value::Int(1 << 32)),
        ("int_max", Value::Int(i64::MAX)),
        ("int_minus_1", Value::Int(-1)),
        ("int_minus_25", Value::Int(-25)),
        ("int_min", Value::Int(i64::MIN)),
        ("float_0", Value::Float(0.0)),
        ("float_1_is_f16", Value::Float(1.0)),
        ("float_1_5_is_f16", Value::Float(1.5)),
        ("float_65504_is_f16", Value::Float(65504.0)),
        (
            "float_smallest_f16_subnormal",
            Value::Float(5.960464477539063e-8),
        ),
        ("float_100000_is_f32", Value::Float(100000.0)),
        ("float_f32_max", Value::Float(3.4028234663852886e38)),
        ("float_1_1_is_f64", Value::Float(1.1)),
        ("float_minus_4_1", Value::Float(-4.1)),
        ("float_1e300", Value::Float(1.0e300)),
        ("text_empty", Value::from("")),
        ("text_ascii", Value::from("IETF")),
        ("text_utf8", Value::from("ü水")),
        ("text_24_bytes", Value::from("x".repeat(24))),
        ("bytes_empty", Value::Bytes(vec![])),
        ("bytes", Value::Bytes(vec![1, 2, 3, 4])),
        ("array_empty", Value::Array(vec![])),
        (
            "array_nested",
            Value::Array(vec![Value::Int(1), Value::Array(vec![Value::Int(2)])]),
        ),
        ("map_empty", Value::Map(BTreeMap::new())),
        (
            "map_keys_shorter_first",
            Value::Map(
                [("bb", 1), ("c", 2), ("aaa", 3)]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), Value::Int(v)))
                    .collect(),
            ),
        ),
        ("depth_32", nested),
    ];
    let valid: Vec<J> = cases
        .iter()
        .map(|(name, v)| json!({"name": name, "value": value_to(v), "cbor": hex(&encode_value(v).unwrap())}))
        .collect();
    let mut invalid: Vec<J> = INVALID_VALUES
        .iter()
        .map(|(name, cbor)| json!({"name": name, "cbor": cbor, "error": "corrupt"}))
        .collect();
    invalid.push(
        json!({"name": "depth_33", "cbor": format!("{}f6", "81".repeat(33)), "error": "corrupt"}),
    );
    json!({"valid": valid, "invalid": invalid})
}

fn op_vectors() -> J {
    let ops = [
        (
            "set_one_field",
            set(h(T, 1), "notes", "n1", &[("title", "Hello".into())]),
        ),
        (
            "set_fields_sorted_by_encoded_key",
            set(
                h(T, 2),
                "notes",
                "n1",
                &[
                    ("pinned", false.into()),
                    ("title", "Hi".into()),
                    ("n", Value::Int(-7)),
                ],
            ),
        ),
        (
            "unset_fields_byte_order",
            unset(h(T, 3), "notes", "n1", &["b", "a", "B"]),
        ),
        ("delete", delete(h(T, 4), "notes", "n1")),
        (
            "set_nested_and_unicode",
            set(
                h(T + 1, 0),
                "todo_items",
                "ünïcødé/id",
                &[(
                    "data",
                    Value::Map([("k".to_string(), Value::Array(vec![Value::Float(0.5)]))].into()),
                )],
            ),
        ),
    ];
    let valid: Vec<J> = ops
        .iter()
        .map(|(name, op)| json!({"name": name, "op": op_to(op), "cbor": hex(&op_bytes(op))}))
        .collect();
    let prefix = "851b018bcfe568000001656e6f746573626e31";
    let invalid = vec![
        json!({"name": "unknown_kind", "cbor": format!("{prefix}03f6"), "error": "format_too_new"}),
        json!({"name": "empty_set_fields", "cbor": format!("{prefix}00a0"), "error": "corrupt"}),
        json!({"name": "empty_unset_fields", "cbor": format!("{prefix}0180"), "error": "corrupt"}),
        json!({"name": "unset_not_sorted", "cbor": format!("{prefix}018261626161"), "error": "corrupt"}),
        json!({"name": "unset_repeated", "cbor": format!("{prefix}018261616161"), "error": "corrupt"}),
        json!({"name": "delete_payload_not_null", "cbor": format!("{prefix}0200"), "error": "corrupt"}),
        json!({"name": "empty_field_name", "cbor": format!("{prefix}00a16001"), "error": "corrupt"}),
        json!({"name": "four_elements", "cbor": "8400656e6f746573626e3102", "error": "corrupt"}),
        json!({"name": "bad_collection_name", "cbor": "8500654e6f746573626e3102f6", "error": "corrupt"}),
        json!({"name": "empty_doc_id", "cbor": "850065 6e6f7465736002f6".replace(' ', ""), "error": "corrupt"}),
    ];
    json!({"valid": valid, "invalid": invalid})
}

/// Deterministic pseudo-random generator for the random merge scenario.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

fn dump(ops: &[(DeviceId, Op)]) -> (Vec<u8>, Vec<J>) {
    let mut states: BTreeMap<(CollectionName, DocId), DocState> = BTreeMap::new();
    for (dev, op) in ops {
        states
            .entry((op.collection.clone(), op.doc_id.clone()))
            .or_default()
            .apply(&op.kind, Stamp::new(op.hlc, *dev));
    }
    let mut e = Encoder::new();
    e.array(states.len());
    let mut entries = Vec::new();
    for ((c, d), st) in &states {
        bothy::codec::state::encode_entry(&mut e, c, d, st).expect("valid state");
        entries.push(entry_to(c, d, st));
    }
    (e.into_bytes(), entries)
}

/// Name, description and device-tagged ops of one merge scenario.
type Scenario<'a> = (&'a str, &'a str, Vec<(DeviceId, Op)>);

fn merge_vectors() -> Vec<(String, J)> {
    let a = DEV_A;
    let b = DEV_B;
    let mut scenarios: Vec<Scenario<'_>> = vec![
        (
            "concurrent_fields",
            "Two devices edit different fields of the same document; both edits survive.",
            vec![
                (a, set(h(T, 0), "notes", "n1", &[("title", "A".into())])),
                (b, set(h(T, 0), "notes", "n1", &[("tags", "x".into())])),
            ],
        ),
        (
            "same_field_higher_hlc_wins",
            "Same field, different HLCs: the higher HLC wins whatever the order.",
            vec![
                (a, set(h(T + 5, 0), "notes", "n1", &[("title", "A".into())])),
                (b, set(h(T, 9), "notes", "n1", &[("title", "B".into())])),
            ],
        ),
        (
            "same_hlc_device_breaks_tie",
            "Same field and HLC: the larger device id wins.",
            vec![
                (b, set(h(T, 0), "notes", "n1", &[("title", "B".into())])),
                (a, set(h(T, 0), "notes", "n1", &[("title", "A".into())])),
            ],
        ),
        (
            "late_write_before_delete_stays_deleted",
            "A write stamped before a delete does not revive the document, even if it arrives later.",
            vec![
                (a, set(h(T, 0), "notes", "n1", &[("title", "A".into())])),
                (a, delete(h(T + 30, 0), "notes", "n1")),
                (
                    b,
                    set(h(T + 20, 0), "notes", "n1", &[("body", "late".into())]),
                ),
            ],
        ),
        (
            "write_after_delete_revives_new_fields_only",
            "A write after a delete revives the document with only the newer fields.",
            vec![
                (
                    a,
                    set(
                        h(T + 10, 0),
                        "notes",
                        "n1",
                        &[("a", 1.into()), ("b", 2.into())],
                    ),
                ),
                (a, delete(h(T + 20, 0), "notes", "n1")),
                (b, set(h(T + 30, 0), "notes", "n1", &[("b", 3.into())])),
            ],
        ),
        (
            "unset_and_tombstones_are_kept",
            "Unset fields and delete stamps stay in the state.",
            vec![
                (
                    a,
                    set(h(T, 0), "notes", "n1", &[("a", 1.into()), ("b", 2.into())]),
                ),
                (a, unset(h(T, 1), "notes", "n1", &["a"])),
                (b, delete(h(T + 1, 0), "notes", "n2")),
            ],
        ),
    ];

    let mut rng = Lcg(42);
    let devices = [DEV_A, DEV_B, DEV_C];
    let mut next_hlc = [T; 3];
    let mut random = Vec::new();
    for _ in 0..60 {
        let d = rng.next(3) as usize;
        next_hlc[d] += 1 + rng.next(3);
        let hlc = h(next_hlc[d], 0);
        let docid = format!("d{}", rng.next(4));
        let field = ["a", "b", "c"][rng.next(3) as usize];
        let op = match rng.next(6) {
            0 => delete(hlc, "items", &docid),
            1 => unset(hlc, "items", &docid, &[field]),
            _ => set(
                hlc,
                "items",
                &docid,
                &[(field, Value::Int(rng.next(100) as i64))],
            ),
        };
        random.push((devices[d], op));
    }
    scenarios.push((
        "random_three_devices",
        "60 pseudo-random ops from three devices.",
        random,
    ));

    scenarios
        .into_iter()
        .map(|(name, about, ops)| {
            let (bytes, entries) = dump(&ops);
            let ops_json: Vec<J> = ops
                .iter()
                .map(|(dev, op)| json!({"device": dev.to_hex(), "op": op_to(op)}))
                .collect();
            (
                format!("merge/{name}.json"),
                json!({"name": name, "about": about, "ops": ops_json, "state": entries, "dump": hex(&bytes)}),
            )
        })
        .collect()
}

/// A segment with arbitrary header fields, sealed with a correct trailer.
#[allow(clippy::too_many_arguments)]
fn raw_segment(
    version: u16,
    flags: u16,
    seq: u64,
    first: Hlc,
    last: Hlc,
    op_count: u32,
    body: &[u8],
    nonce: [u8; 24],
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&segment::MAGIC);
    b.extend_from_slice(&version.to_be_bytes());
    b.extend_from_slice(&flags.to_be_bytes());
    b.extend_from_slice(DB.as_bytes());
    b.extend_from_slice(DEV_A.as_bytes());
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(&first.as_u64().to_be_bytes());
    b.extend_from_slice(&last.as_u64().to_be_bytes());
    b.extend_from_slice(&op_count.to_be_bytes());
    b.extend_from_slice(&(body.len() as u32).to_be_bytes());
    b.extend_from_slice(&nonce);
    b.extend_from_slice(body);
    let t = blake3::hash(&b);
    b.extend_from_slice(t.as_bytes());
    b
}

fn segment_vectors() -> Vec<(String, J)> {
    let ops = vec![
        set(
            h(T, 0),
            "notes",
            "n1",
            &[("title", "Hello".into()), ("pinned", false.into())],
        ),
        unset(h(T, 1), "notes", "n1", &["pinned"]),
        set(
            h(T + 1, 0),
            "notes",
            "n2",
            &[("body", "x".repeat(200).into())],
        ),
        delete(h(T + 2, 0), "notes", "n2"),
    ];
    let seg = |ops: Vec<Op>| Segment {
        db_id: DB,
        device_id: DEV_A,
        seq: 1,
        ops,
    };
    let mut out = Vec::new();
    for (name, s, compress) in [
        ("plain_one_op", seg(ops[..1].to_vec()), false),
        ("plain_all_kinds", seg(ops.clone()), false),
        ("zstd_all_kinds", seg(ops.clone()), true),
    ] {
        let bytes = segment::encode(&s, compress).unwrap();
        out.push((
            format!("segment/{name}.json"),
            json!({
                "name": name,
                "key": LogicalKey::segment(DB, DEV_A, s.seq).to_string(),
                "db_id": DB.to_hex(),
                "device_id": DEV_A.to_hex(),
                "seq": s.seq,
                "compressed": compress,
                "ops": s.ops.iter().map(op_to).collect::<Vec<_>>(),
                "bytes": hex(&bytes),
            }),
        ));
    }

    let plain = segment::encode_plain(&ops).unwrap();
    let (first, last) = (ops[0].hlc, ops[3].hlc);
    let good = raw_segment(1, 0, 1, first, last, 4, &plain, [0; 24]);
    assert_eq!(good, segment::encode(&seg(ops.clone()), false).unwrap());
    let mut flipped = good.clone();
    let n = flipped.len();
    flipped[n - 1] ^= 1;
    let mut body_flip = good.clone();
    body_flip[100] ^= 1;
    let mut decreasing = ops.clone();
    decreasing.swap(0, 2);
    let dec_plain = segment::encode_plain(&decreasing).unwrap();
    let mut nonce = [0u8; 24];
    nonce[0] = 1;
    let invalid: Vec<(&str, Vec<u8>, &str, u64)> = vec![
        ("too_short", good[..100].to_vec(), "corrupt", 1),
        (
            "bad_magic",
            [b"BSEX".as_slice(), &good[4..]].concat(),
            "corrupt",
            1,
        ),
        (
            "version_2",
            raw_segment(2, 0, 1, first, last, 4, &plain, [0; 24]),
            "format_too_new",
            1,
        ),
        (
            "unknown_flag",
            raw_segment(1, 4, 1, first, last, 4, &plain, [0; 24]),
            "format_too_new",
            1,
        ),
        (
            "encrypted_without_key",
            raw_segment(1, 2, 1, first, last, 4, &plain, [0; 24]),
            "key_required",
            1,
        ),
        ("seq_does_not_match_key", good.clone(), "corrupt", 2),
        ("trailer_mismatch", flipped, "corrupt", 1),
        ("body_bit_flip", body_flip, "corrupt", 1),
        (
            "nonzero_nonce",
            raw_segment(1, 0, 1, first, last, 4, &plain, nonce),
            "corrupt",
            1,
        ),
        (
            "op_count_mismatch",
            raw_segment(1, 0, 1, first, last, 3, &plain, [0; 24]),
            "corrupt",
            1,
        ),
        (
            "last_hlc_mismatch",
            raw_segment(1, 0, 1, first, first, 4, &plain, [0; 24]),
            "corrupt",
            1,
        ),
        (
            "hlcs_not_increasing",
            raw_segment(
                1,
                0,
                1,
                decreasing[0].hlc,
                decreasing[3].hlc,
                4,
                &dec_plain,
                [0; 24],
            ),
            "corrupt",
            1,
        ),
        (
            "zero_ops",
            raw_segment(1, 0, 1, first, first, 0, &[0x80], [0; 24]),
            "corrupt",
            1,
        ),
    ];
    let invalid: Vec<J> = invalid
        .into_iter()
        .map(|(name, bytes, error, seq)| {
            json!({"name": name, "db_id": DB.to_hex(), "device_id": DEV_A.to_hex(), "seq": seq,
                   "bytes": hex(&bytes), "error": error})
        })
        .collect();
    out.push(("segment/invalid.json".into(), json!({"invalid": invalid})));
    out
}

fn snapshot_vectors() -> Vec<(String, J)> {
    let ops: Vec<(DeviceId, Op)> = vec![
        (
            DEV_A,
            set(
                h(T, 0),
                "notes",
                "n1",
                &[("title", "A".into()), ("tags", "x".into())],
            ),
        ),
        (DEV_A, unset(h(T, 1), "notes", "n1", &["tags"])),
        (DEV_B, set(h(T, 0), "notes", "n2", &[("title", "B".into())])),
        (DEV_B, delete(h(T + 1, 0), "notes", "n3")),
        (
            DEV_A,
            set(h(T + 2, 0), "todos", "t1", &[("done", true.into())]),
        ),
    ];
    let mut states: BTreeMap<(CollectionName, DocId), DocState> = BTreeMap::new();
    for (dev, op) in &ops {
        states
            .entry((op.collection.clone(), op.doc_id.clone()))
            .or_default()
            .apply(&op.kind, Stamp::new(op.hlc, *dev));
    }
    let header = SnapshotHeader {
        db_id: DB,
        device_id: DEV_A,
        hlc: h(T + 3, 0),
        vv: [(DEV_A, 3), (DEV_B, 1)].into(),
    };
    let build =
        |entries: &BTreeMap<(CollectionName, DocId), DocState>, compress: bool, target: usize| {
            let mut w = SnapshotWriter::new(header.clone(), compress).with_chunk_target(target);
            for ((c, d), st) in entries {
                w.push(c, d, st).unwrap();
            }
            w.finish().unwrap()
        };
    let header_json = json!({
        "db_id": DB.to_hex(),
        "device_id": DEV_A.to_hex(),
        "hlc": header.hlc.to_hex(),
        "vv": header.vv.iter().map(|(d, s)| (d.to_hex(), J::from(*s))).collect::<serde_json::Map<_, _>>(),
    });
    let mut out = Vec::new();
    let empty = BTreeMap::new();
    for (name, entries, compress, target) in [
        ("empty", &empty, false, 1 << 20),
        ("single_chunk", &states, false, 1 << 20),
        ("multi_chunk", &states, false, 64),
        ("zstd_multi_chunk", &states, true, 64),
    ] {
        let bytes = build(entries, compress, target);
        out.push((
            format!("snapshot/{name}.json"),
            json!({
                "name": name,
                "key": LogicalKey::snapshot(DB, DEV_A, header.hlc).to_string(),
                "header": header_json,
                "compressed": compress,
                "chunk_target": target,
                "entries": entries.iter().map(|((c, d), st)| entry_to(c, d, st)).collect::<Vec<_>>(),
                "bytes": hex(&bytes),
            }),
        ));
    }

    let good = build(&states, false, 64);
    let mut flipped = good.clone();
    let n = flipped.len();
    flipped[n - 1] ^= 1;
    let mut v2 = good.clone();
    v2[5] = 2;
    let reseal = |mut b: Vec<u8>| {
        let n = b.len() - 32;
        let t = blake3::hash(&b[..n]);
        b[n..].copy_from_slice(t.as_bytes());
        b
    };
    // Swap the first two chunk records so entries go out of order across a chunk boundary.
    let vv_len = u32::from_be_bytes(good[52..56].try_into().unwrap()) as usize;
    let start = 56 + vv_len;
    let len1 = 4 + u32::from_be_bytes(good[start..start + 4].try_into().unwrap()) as usize;
    let len2 =
        4 + u32::from_be_bytes(good[start + len1..start + len1 + 4].try_into().unwrap()) as usize;
    let mut swapped = good[..start].to_vec();
    swapped.extend_from_slice(&good[start + len1..start + len1 + len2]);
    swapped.extend_from_slice(&good[start..start + len1]);
    swapped.extend_from_slice(&good[start + len1 + len2..]);
    let invalid = vec![
        json!({"name": "trailer_mismatch", "bytes": hex(&flipped), "error": "corrupt"}),
        json!({"name": "version_2", "bytes": hex(&v2), "error": "format_too_new"}),
        json!({"name": "entries_out_of_order_across_chunks", "bytes": hex(&reseal(swapped)), "error": "corrupt"}),
    ];
    out.push((
        "snapshot/invalid.json".into(),
        json!({"header": header_json, "invalid": invalid}),
    ));
    out
}

fn manifest_vectors() -> J {
    let base = Manifest::new(DB, DEV_A, h(T, 0), "com.example.notes").unwrap();
    let base_json: J = serde_json::from_slice(&base.to_json()).unwrap();
    let with = |key: &str, v: J| {
        let mut m = base_json.clone();
        m[key] = v;
        m
    };
    let without = |key: &str| {
        let mut m = base_json.clone();
        m.as_object_mut().unwrap().remove(key);
        m
    };
    let mut none = base.clone();
    none.compression = Compression::None;
    let valid = vec![
        json!({"name": "default", "manifest": base_json}),
        json!({"name": "compression_none", "manifest": serde_json::from_slice::<J>(&none.to_json()).unwrap()}),
        json!({"name": "unknown_field_ignored", "manifest": with("future_field", json!({"x": 1}))}),
    ];
    let invalid = vec![
        json!({"name": "format_2", "manifest": with("bothy_format", json!(2)), "error": "format_too_new"}),
        json!({"name": "unknown_serialization", "manifest": with("codec", json!({"serialization": "msgpack", "compression": "zstd"})), "error": "format_too_new"}),
        json!({"name": "unknown_compression", "manifest": with("codec", json!({"serialization": "cbor", "compression": "lz4"})), "error": "format_too_new"}),
        json!({"name": "missing_app_id", "manifest": without("app_id"), "error": "corrupt"}),
        json!({"name": "uppercase_db_id", "manifest": with("db_id", json!(DB.to_hex().to_uppercase())), "error": "corrupt"}),
        json!({"name": "grace_period_zero", "manifest": with("compaction", json!({"grace_period_days": 0})), "error": "corrupt"}),
        json!({"name": "encryption_not_object", "manifest": with("encryption", json!("yes")), "error": "corrupt"}),
        json!({"name": "not_an_object", "manifest": json!([1, 2]), "error": "corrupt"}),
    ];
    json!({"key": LogicalKey::manifest(DB).to_string(), "valid": valid, "invalid": invalid})
}

/// Every vector file as `(path relative to spec/vectors, contents)`.
pub fn all() -> Vec<(String, J)> {
    let mut out = vec![
        ("hlc.json".to_string(), hlc_vectors()),
        ("values.json".to_string(), value_vectors()),
        ("ops.json".to_string(), op_vectors()),
        ("manifest.json".to_string(), manifest_vectors()),
    ];
    out.extend(merge_vectors());
    out.extend(segment_vectors());
    out.extend(snapshot_vectors());
    out.extend(crate::crypto_vectors::all());
    out
}
