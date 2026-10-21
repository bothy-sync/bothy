//! Operation encoding: `[hlc, collection, doc_id, kind, payload]`.

use std::collections::{BTreeMap, BTreeSet};

use crate::Hlc;
use crate::codec::cbor::{Decoder, Encoder, sorted_text_keys};
use crate::error::{Error, Result};
use crate::model::{CollectionName, DocId, FieldName, Op, OpKind};

/// Writes one operation.
pub fn encode_op(e: &mut Encoder, op: &Op) -> Result<()> {
    e.array(5);
    e.uint(op.hlc.as_u64());
    e.text(op.collection.as_str());
    e.text(op.doc_id.as_str());
    e.uint(op.kind.code());
    match &op.kind {
        OpKind::SetFields(fields) => {
            e.map(fields.len());
            for (key, value) in sorted_text_keys(fields.iter()) {
                e.raw(&key);
                e.value(value)?;
            }
        }
        OpKind::UnsetFields(fields) => {
            // BTreeSet<FieldName> iterates in byte order, as the format requires.
            e.array(fields.len());
            for f in fields {
                e.text(f.as_str());
            }
        }
        OpKind::DeleteDoc => e.null(),
    }
    Ok(())
}

/// Reads one operation. An unknown kind is `FormatTooNew`; anything malformed is `Corrupt`.
pub fn decode_op(d: &mut Decoder<'_>) -> Result<Op> {
    if d.array()? != 5 {
        return Err(Error::corrupt("op: expected a 5-element array"));
    }
    let hlc = Hlc::from_u64(d.uint()?);
    let collection = CollectionName::new(d.text()?).map_err(as_corrupt)?;
    let doc_id = DocId::new(d.text()?).map_err(as_corrupt)?;
    let kind = match d.uint()? {
        0 => {
            let n = d.map()?;
            if n == 0 {
                return Err(Error::corrupt("op: SetFields is empty"));
            }
            let mut fields = BTreeMap::new();
            let mut prev = None;
            for _ in 0..n {
                let (key, encoded) = d.map_text_key(prev)?;
                prev = Some(encoded);
                fields.insert(FieldName::new(key).map_err(as_corrupt)?, d.value()?);
            }
            OpKind::SetFields(fields)
        }
        1 => {
            let n = d.array()?;
            if n == 0 {
                return Err(Error::corrupt("op: UnsetFields is empty"));
            }
            let mut fields = BTreeSet::new();
            let mut prev: Option<&str> = None;
            for _ in 0..n {
                let name = d.text()?;
                if prev.is_some_and(|p| p >= name) {
                    return Err(Error::corrupt("op: UnsetFields is not sorted or repeats"));
                }
                prev = Some(name);
                fields.insert(FieldName::new(name).map_err(as_corrupt)?);
            }
            OpKind::UnsetFields(fields)
        }
        2 => {
            d.null()?;
            OpKind::DeleteDoc
        }
        k => return Err(Error::format_too_new(format!("op: unknown kind {k}"))),
    };
    Ok(Op {
        hlc,
        collection,
        doc_id,
        kind,
    })
}

fn as_corrupt(e: Error) -> Error {
    Error::corrupt(format!("op: {}", e.message()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use crate::model::{Value, to_hex};

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn op(kind: OpKind) -> Op {
        Op {
            hlc: Hlc::from_parts(1_700_000_000_000, 1).unwrap(),
            collection: CollectionName::new("notes").unwrap(),
            doc_id: DocId::new("n1").unwrap(),
            kind,
        }
    }

    fn encode(op: &Op) -> Vec<u8> {
        let mut e = Encoder::new();
        encode_op(&mut e, op).unwrap();
        e.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Op> {
        let mut d = Decoder::new(bytes);
        let op = decode_op(&mut d)?;
        d.finish()?;
        Ok(op)
    }

    fn field(s: &str) -> FieldName {
        FieldName::new(s).unwrap()
    }

    const PREFIX: &str = "851b018bcfe568000001656e6f746573626e31";

    #[test]
    fn set_fields_encoding() {
        let mut m = BTreeMap::new();
        m.insert(field("title"), Value::from("Hi"));
        m.insert(field("pinned"), Value::Bool(false));
        let o = op(OpKind::SetFields(m));
        let bytes = encode(&o);
        // "title" (5 bytes) sorts before "pinned" (6 bytes).
        assert_eq!(
            to_hex(&bytes),
            format!("{PREFIX}00a2657469746c656248696670696e6e6564f4")
        );
        assert_eq!(decode(&bytes).unwrap(), o);
    }

    #[test]
    fn unset_fields_and_delete_encoding() {
        let o = op(OpKind::UnsetFields([field("b"), field("a")].into()));
        assert_eq!(to_hex(&encode(&o)), format!("{PREFIX}018261616162"));
        assert_eq!(decode(&encode(&o)).unwrap(), o);

        let o = op(OpKind::DeleteDoc);
        assert_eq!(to_hex(&encode(&o)), format!("{PREFIX}02f6"));
        assert_eq!(decode(&encode(&o)).unwrap(), o);
    }

    #[test]
    fn unknown_kind_is_format_too_new() {
        let err = decode(&hex(&format!("{PREFIX}03f6"))).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::FormatTooNew);
    }

    #[test]
    fn malformed_ops_are_corrupt() {
        for bad in [
            format!("{PREFIX}00a0"),                  // empty SetFields
            format!("{PREFIX}0180"),                  // empty UnsetFields
            format!("{PREFIX}0182616261 61"),         // unsorted UnsetFields
            format!("{PREFIX}0182616161 61"),         // repeated UnsetFields
            format!("{PREFIX}0200"),                  // DeleteDoc payload not null
            format!("{PREFIX}00a16001"),              // empty field name
            "8400656e6f746573626e3102".to_owned(),    // 4 elements
            "850065 4e6f746573626e3102f6".to_owned(), // bad collection name
        ] {
            let bad = bad.replace(' ', "");
            let err = decode(&hex(&bad)).expect_err(&bad);
            assert_eq!(err.kind(), ErrorKind::Corrupt, "{bad}");
        }
    }
}
