//! Random and mutated input never makes a parser panic. A light, always-on companion to the
//! cargo-fuzz targets in `fuzz/`.

use bothy::codec::cbor::{Decoder, decode_value, encode_value};
use bothy::codec::decode_op;
use bothy::codec::keys::LogicalKey;
use bothy::codec::manifest::Manifest;
use bothy::codec::segment::{self, Segment};
use bothy::codec::snapshot::{self, SnapshotHeader, SnapshotReader, SnapshotWriter};
use bothy::merge::{DocState, FieldState};
use bothy::{CollectionName, DbId, DeviceId, DocId, FieldName, Hlc, Op, OpKind, Stamp, Value};
use proptest::prelude::*;

const DB: DbId = DbId::from_bytes([1; 16]);
const DEV: DeviceId = DeviceId::from_bytes([2; 16]);

fn sample_segment() -> Vec<u8> {
    let ops = (0..5u16)
        .map(|i| Op {
            hlc: Hlc::from_parts(1_700_000_000_000, i).unwrap(),
            collection: CollectionName::new("c").unwrap(),
            doc_id: DocId::new("d").unwrap(),
            kind: OpKind::SetFields([(FieldName::new("f").unwrap(), Value::Int(i.into()))].into()),
        })
        .collect();
    segment::encode(
        &Segment {
            db_id: DB,
            device_id: DEV,
            seq: 1,
            ops,
        },
        false,
    )
    .unwrap()
}

fn sample_snapshot() -> Vec<u8> {
    let header = SnapshotHeader {
        db_id: DB,
        device_id: DEV,
        hlc: Hlc::from_u64(9),
        vv: [(DEV, 1)].into(),
    };
    let mut w = SnapshotWriter::new(header, false).with_chunk_target(40);
    for i in 0..5 {
        let mut st = DocState::default();
        st.fields.insert(
            FieldName::new("f").unwrap(),
            FieldState {
                stamp: Stamp::new(Hlc::from_u64(1), DEV),
                value: Some(Value::Int(i)),
            },
        );
        w.push(
            &CollectionName::new("c").unwrap(),
            &DocId::new(&format!("d{i}")).unwrap(),
            &st,
        )
        .unwrap();
    }
    w.finish().unwrap()
}

/// Recomputes the BLAKE3 trailer so mutations reach the checks behind it.
fn reseal(mut b: Vec<u8>) -> Vec<u8> {
    if b.len() >= 32 {
        let n = b.len() - 32;
        let h = blake3::hash(&b[..n]);
        b[n..].copy_from_slice(h.as_bytes());
    }
    b
}

fn mutate(mut base: Vec<u8>, edits: &[(usize, u8)], cut: usize) -> Vec<u8> {
    for &(at, byte) in edits {
        if !base.is_empty() {
            let i = at % base.len();
            base[i] = byte;
        }
    }
    base.truncate(base.len().saturating_sub(cut % 8));
    base
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    #[test]
    fn cbor_accepts_only_canonical_input(data in prop::collection::vec(any::<u8>(), 0..64)) {
        if let Ok(v) = decode_value(&data) {
            prop_assert_eq!(encode_value(&v).unwrap(), data.clone());
        }
        let mut d = Decoder::new(&data);
        let _ = decode_op(&mut d);
    }

    #[test]
    fn mutated_segments(edits in prop::collection::vec((any::<usize>(), any::<u8>()), 1..4), cut in any::<usize>(), seal in any::<bool>()) {
        let mut bytes = mutate(sample_segment(), &edits, cut);
        if seal {
            bytes = reseal(bytes);
        }
        let _ = segment::decode(&bytes, &segment::Expected { db_id: DB, device_id: DEV, seq: 1 });
    }

    #[test]
    fn mutated_snapshots(edits in prop::collection::vec((any::<usize>(), any::<u8>()), 1..4), cut in any::<usize>(), seal in any::<bool>()) {
        let mut bytes = mutate(sample_snapshot(), &edits, cut);
        if seal {
            bytes = reseal(bytes);
        }
        let exp = snapshot::Expected { db_id: DB, device_id: DEV, hlc: Hlc::from_u64(9) };
        if let Ok(r) = SnapshotReader::open(&bytes, &exp) {
            let _ = r.for_each_entry(|_, _, _| Ok(()));
        }
    }

    #[test]
    fn manifests_and_keys(text in ".{0,200}") {
        let _ = Manifest::from_json(text.as_bytes(), None);
        let _ = bothy::codec::manifest::KeyFile::from_json(text.as_bytes());
        #[cfg(feature = "crypto")]
        let _ = bothy::crypto::open_json(&bothy::crypto::DataKey::from_bytes([1; 32]), "k", text.as_bytes());
        if let Some(k) = LogicalKey::parse(&text) {
            prop_assert_eq!(k.to_string(), text);
        }
    }
}
