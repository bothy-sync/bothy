//! Typed JSON: a lossless JSON form of values, ops, stamps and states for vector files.
//!
//! - `null`, `true`, `false`, strings and arrays map directly.
//! - `{"int": "-5"}`: integer as a decimal string (keeps 64-bit precision).
//! - `{"float": 1.5, "bits": "3ff8000000000000"}`: finite float. `bits` (IEEE 754 binary64,
//!   hex) is authoritative; `float` is for humans, since not every JSON parser reads floats exactly.
//! - `{"bytes": "00ff"}`: bytes as lowercase hex.
//! - `{"map": {...}}`: map with text keys.

use std::collections::{BTreeMap, BTreeSet};

use bothy::merge::{DocState, FieldState};
use bothy::{CollectionName, DeviceId, DocId, FieldName, Hlc, Op, OpKind, Stamp, Value};
use serde_json::{Map, Value as J, json};

/// Error while reading a vector file.
pub type Res<T> = Result<T, String>;

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses lowercase or uppercase hex.
pub fn unhex(s: &str) -> Res<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd-length hex {s:?}"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("bad hex {s:?}: {e}")))
        .collect()
}

fn obj(j: &J) -> Res<&Map<String, J>> {
    j.as_object()
        .ok_or_else(|| format!("expected object, got {j}"))
}

/// Reads a string member.
pub fn s<'a>(j: &'a J, key: &str) -> Res<&'a str> {
    j.get(key)
        .and_then(J::as_str)
        .ok_or_else(|| format!("missing string {key:?} in {j}"))
}

/// Reads an unsigned integer member.
pub fn u(j: &J, key: &str) -> Res<u64> {
    j.get(key)
        .and_then(J::as_u64)
        .ok_or_else(|| format!("missing integer {key:?} in {j}"))
}

/// Value to typed JSON.
pub fn value_to(v: &Value) -> J {
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(i) => json!({"int": i.to_string()}),
        Value::Float(f) => json!({"float": f, "bits": format!("{:016x}", f.to_bits())}),
        Value::Text(t) => J::String(t.clone()),
        Value::Bytes(b) => json!({"bytes": hex(b)}),
        Value::Array(items) => J::Array(items.iter().map(value_to).collect()),
        Value::Map(m) => {
            json!({"map": m.iter().map(|(k, v)| (k.clone(), value_to(v))).collect::<Map<_, _>>()})
        }
    }
}

/// Typed JSON to value.
pub fn value_from(j: &J) -> Res<Value> {
    Ok(match j {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::String(t) => Value::Text(t.clone()),
        J::Array(items) => Value::Array(items.iter().map(value_from).collect::<Res<_>>()?),
        J::Object(m) if m.len() == 2 && m.contains_key("float") => {
            let bits = m
                .get("bits")
                .and_then(J::as_str)
                .and_then(|b| u64::from_str_radix(b, 16).ok())
                .ok_or_else(|| format!("float needs hex bits: {j}"))?;
            Value::Float(f64::from_bits(bits))
        }
        J::Object(m) if m.len() == 1 => {
            let (tag, inner) = m.iter().next().expect("one entry");
            match tag.as_str() {
                "int" => Value::Int(
                    inner
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| format!("bad int {inner}"))?,
                ),
                "bytes" => Value::Bytes(unhex(inner.as_str().ok_or("bytes must be hex")?)?),
                "map" => Value::Map(
                    obj(inner)?
                        .iter()
                        .map(|(k, v)| Ok((k.clone(), value_from(v)?)))
                        .collect::<Res<BTreeMap<_, _>>>()?,
                ),
                other => return Err(format!("unknown value tag {other:?}")),
            }
        }
        other => return Err(format!("bad typed value {other}")),
    })
}

/// Stamp to `{"hlc": hex16, "device": hex32}`.
pub fn stamp_to(st: &Stamp) -> J {
    json!({"hlc": st.hlc.to_hex(), "device": st.device.to_hex()})
}

/// `{"hlc", "device"}` to stamp.
pub fn stamp_from(j: &J) -> Res<Stamp> {
    Ok(Stamp::new(hlc(s(j, "hlc")?)?, device(s(j, "device")?)?))
}

/// Parses an HLC hex string.
pub fn hlc(t: &str) -> Res<Hlc> {
    t.parse().map_err(|e| format!("bad hlc {t:?}: {e}"))
}

/// Parses a device id hex string.
pub fn device(t: &str) -> Res<DeviceId> {
    t.parse().map_err(|e| format!("bad device id {t:?}: {e}"))
}

fn field(t: &str) -> Res<FieldName> {
    FieldName::new(t).map_err(|e| e.to_string())
}

/// Op to typed JSON.
pub fn op_to(op: &Op) -> J {
    let mut m = Map::new();
    m.insert("hlc".into(), J::String(op.hlc.to_hex()));
    m.insert("collection".into(), J::String(op.collection.to_string()));
    m.insert("doc_id".into(), J::String(op.doc_id.to_string()));
    match &op.kind {
        OpKind::SetFields(f) => {
            m.insert("kind".into(), "set".into());
            m.insert(
                "fields".into(),
                J::Object(
                    f.iter()
                        .map(|(k, v)| (k.to_string(), value_to(v)))
                        .collect(),
                ),
            );
        }
        OpKind::UnsetFields(f) => {
            m.insert("kind".into(), "unset".into());
            m.insert(
                "fields".into(),
                f.iter().map(|k| J::String(k.to_string())).collect(),
            );
        }
        OpKind::DeleteDoc => {
            m.insert("kind".into(), "delete".into());
        }
    }
    J::Object(m)
}

/// Typed JSON to op.
pub fn op_from(j: &J) -> Res<Op> {
    let kind = match s(j, "kind")? {
        "set" => OpKind::SetFields(
            obj(j.get("fields").ok_or("missing fields")?)?
                .iter()
                .map(|(k, v)| Ok((field(k)?, value_from(v)?)))
                .collect::<Res<BTreeMap<_, _>>>()?,
        ),
        "unset" => OpKind::UnsetFields(
            j.get("fields")
                .and_then(J::as_array)
                .ok_or("missing fields")?
                .iter()
                .map(|k| field(k.as_str().ok_or("field must be a string")?))
                .collect::<Res<BTreeSet<_>>>()?,
        ),
        "delete" => OpKind::DeleteDoc,
        other => return Err(format!("unknown op kind {other:?}")),
    };
    Ok(Op {
        hlc: hlc(s(j, "hlc")?)?,
        collection: CollectionName::new(s(j, "collection")?).map_err(|e| e.to_string())?,
        doc_id: DocId::new(s(j, "doc_id")?).map_err(|e| e.to_string())?,
        kind,
    })
}

/// Snapshot/dump entry to typed JSON. An absent `value` means the field is unset.
pub fn entry_to(c: &CollectionName, id: &DocId, st: &DocState) -> J {
    let fields: Vec<J> = st
        .fields
        .iter()
        .map(|(name, f)| {
            let mut m = Map::new();
            m.insert("name".into(), J::String(name.to_string()));
            m.insert("stamp".into(), stamp_to(&f.stamp));
            if let Some(v) = &f.value {
                m.insert("value".into(), value_to(v));
            }
            J::Object(m)
        })
        .collect();
    json!({
        "collection": c.to_string(),
        "doc_id": id.to_string(),
        "deleted_at": st.deleted_at.as_ref().map_or(J::Null, stamp_to),
        "fields": fields,
    })
}

/// Typed JSON to entry.
pub fn entry_from(j: &J) -> Res<(CollectionName, DocId, DocState)> {
    let deleted_at = match j.get("deleted_at") {
        None | Some(J::Null) => None,
        Some(st) => Some(stamp_from(st)?),
    };
    let mut fields = BTreeMap::new();
    for f in j
        .get("fields")
        .and_then(J::as_array)
        .ok_or("missing fields")?
    {
        let value = f.get("value").map(value_from).transpose()?;
        fields.insert(
            field(s(f, "name")?)?,
            FieldState {
                stamp: stamp_from(f.get("stamp").ok_or("missing stamp")?)?,
                value,
            },
        );
    }
    Ok((
        CollectionName::new(s(j, "collection")?).map_err(|e| e.to_string())?,
        DocId::new(s(j, "doc_id")?).map_err(|e| e.to_string())?,
        DocState { deleted_at, fields },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_values_round_trip() {
        let v = Value::Array(vec![
            Value::Null,
            Value::Int(i64::MIN),
            Value::Float(1.1),
            Value::Bytes(vec![0, 255]),
            Value::Map([("k".to_string(), Value::from("v"))].into()),
        ]);
        assert_eq!(value_from(&value_to(&v)).unwrap(), v);
    }
}
