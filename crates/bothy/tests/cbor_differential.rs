//! Cross-checks the canonical CBOR codec against `ciborium`.

use std::collections::BTreeMap;

use bothy::Value;
use bothy::codec::cbor::{decode_value, encode_value};
use ciborium::Value as Cv;
use proptest::prelude::*;

fn arb_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        prop_oneof![
            any::<f64>().prop_filter("finite", |f| f.is_finite()),
            any::<f32>()
                .prop_filter("finite", |f| f.is_finite())
                .prop_map(f64::from),
            (-2048i32..2048).prop_map(|i| i as f64 / 8.0),
        ]
        .prop_map(|f| Value::Float(if f == 0.0 { 0.0 } else { f })),
        ".{0,12}".prop_map(Value::Text),
        prop::collection::vec(any::<u8>(), 0..12).prop_map(Value::Bytes),
    ];
    leaf.prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map(".{0,6}", inner, 0..6).prop_map(Value::Map),
        ]
    })
}

fn to_ciborium(v: &Value) -> Cv {
    match v {
        Value::Null => Cv::Null,
        Value::Bool(b) => Cv::Bool(*b),
        Value::Int(i) => Cv::Integer((*i).into()),
        Value::Float(f) => Cv::Float(*f),
        Value::Text(s) => Cv::Text(s.clone()),
        Value::Bytes(b) => Cv::Bytes(b.clone()),
        Value::Array(items) => Cv::Array(items.iter().map(to_ciborium).collect()),
        Value::Map(m) => {
            // Canonical order: by encoded key, i.e. shorter keys first, then bytewise.
            let mut pairs: Vec<_> = m.iter().collect();
            pairs.sort_by(|a, b| (a.0.len(), a.0.as_bytes()).cmp(&(b.0.len(), b.0.as_bytes())));
            Cv::Map(
                pairs
                    .into_iter()
                    .map(|(k, v)| (Cv::Text(k.clone()), to_ciborium(v)))
                    .collect(),
            )
        }
    }
}

fn from_ciborium(v: &Cv) -> Value {
    match v {
        Cv::Null => Value::Null,
        Cv::Bool(b) => Value::Bool(*b),
        Cv::Integer(i) => Value::Int(i64::try_from(*i).expect("i64 range")),
        Cv::Float(f) => Value::Float(*f),
        Cv::Text(s) => Value::Text(s.clone()),
        Cv::Bytes(b) => Value::Bytes(b.clone()),
        Cv::Array(items) => Value::Array(items.iter().map(from_ciborium).collect()),
        Cv::Map(pairs) => Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.as_text().expect("text key").to_owned(), from_ciborium(v)))
                .collect::<BTreeMap<_, _>>(),
        ),
        other => panic!("unexpected ciborium value {other:?}"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn round_trips(v in arb_value()) {
        let bytes = encode_value(&v).unwrap();
        prop_assert_eq!(decode_value(&bytes).unwrap(), v);
    }

    #[test]
    fn ciborium_reads_what_we_write(v in arb_value()) {
        let bytes = encode_value(&v).unwrap();
        let theirs: Cv = ciborium::from_reader(bytes.as_slice()).unwrap();
        prop_assert_eq!(from_ciborium(&theirs), v);
    }

    #[test]
    fn same_bytes_as_ciborium_for_canonical_input(v in arb_value()) {
        let mut theirs = Vec::new();
        ciborium::into_writer(&to_ciborium(&v), &mut theirs).unwrap();
        prop_assert_eq!(encode_value(&v).unwrap(), theirs);
    }
}
