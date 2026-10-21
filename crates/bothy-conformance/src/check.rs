//! Checks vector files against this implementation.

use std::collections::BTreeMap;
use std::path::Path;

use bothy::codec::cbor::{Decoder, Encoder, decode_value, encode_value};
use bothy::codec::manifest::Manifest;
use bothy::codec::segment::{self, Segment};
use bothy::codec::snapshot::{self, SnapshotReader, SnapshotWriter};
use bothy::codec::state::encode_entry;
use bothy::codec::{decode_op, encode_op};
use bothy::merge::DocState;
use bothy::{CollectionName, DbId, DocId, ErrorKind, Hlc, HlcClock, Stamp};
use serde_json::Value as J;

use crate::typed::{Res, device, entry_from, hex, hlc, op_from, s, u, unhex, value_from};

fn kind_name(k: ErrorKind) -> &'static str {
    match k {
        ErrorKind::Corrupt => "corrupt",
        ErrorKind::FormatTooNew => "format_too_new",
        ErrorKind::KeyRequired => "key_required",
        ErrorKind::WrongPassword => "wrong_password",
        _ => "other",
    }
}

fn expect_error<T>(name: &str, r: bothy::Result<T>, error: &str) -> Res<()> {
    match r {
        Ok(_) => Err(format!("{name}: expected {error}, but it was accepted")),
        Err(e) if kind_name(e.kind()) == error => Ok(()),
        Err(e) => Err(format!("{name}: expected {error}, got {e}")),
    }
}

fn list<'a>(j: &'a J, key: &str) -> Res<&'a Vec<J>> {
    j.get(key)
        .and_then(J::as_array)
        .ok_or_else(|| format!("missing array {key:?}"))
}

fn read(path: &Path) -> Res<J> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn db(t: &str) -> Res<DbId> {
    t.parse().map_err(|e| format!("bad db id {t:?}: {e}"))
}

fn check_hlc(j: &J) -> Res<usize> {
    let mut n = 0;
    for p in list(j, "parts")? {
        let x = Hlc::from_parts(u(p, "physical")?, u(p, "logical")? as u16).ok_or("bad parts")?;
        if x.to_hex() != s(p, "hex")? || x.as_u64().to_string() != s(p, "u64")? {
            return Err(format!("hlc parts mismatch for {p}"));
        }
        if hlc(s(p, "hex")?)? != x {
            return Err(format!("hlc hex parse mismatch for {p}"));
        }
        n += 1;
    }
    for c in list(j, "clock")? {
        let mut clock =
            HlcClock::resume(hlc(s(c, "start")?)?).with_max_drift_ms(u(c, "max_drift_ms")?);
        for step in list(c, "steps")? {
            let got = if let Some(w) = step.get("now").and_then(J::as_u64) {
                clock.now(w)
            } else {
                clock.observe(hlc(s(step, "observe")?)?, u(step, "wall")?)
            };
            if got.to_hex() != s(step, "expect")? {
                return Err(format!("{}: step {step} gave {got}", s(c, "name")?));
            }
            n += 1;
        }
    }
    Ok(n)
}

fn check_values(j: &J) -> Res<usize> {
    let mut n = 0;
    for v in list(j, "valid")? {
        let name = s(v, "name")?;
        let cbor = unhex(s(v, "cbor")?)?;
        let value = value_from(v.get("value").ok_or("missing value")?)?;
        let decoded = decode_value(&cbor).map_err(|e| format!("{name}: {e}"))?;
        if decoded != value {
            return Err(format!("{name}: decoded {decoded:?}, expected {value:?}"));
        }
        if encode_value(&value).map_err(|e| e.to_string())? != cbor {
            return Err(format!("{name}: encoding differs"));
        }
        n += 1;
    }
    for v in list(j, "invalid")? {
        let name = s(v, "name")?;
        expect_error(name, decode_value(&unhex(s(v, "cbor")?)?), s(v, "error")?)?;
        n += 1;
    }
    Ok(n)
}

fn check_ops(j: &J) -> Res<usize> {
    let decode = |b: &[u8]| -> bothy::Result<bothy::Op> {
        let mut d = Decoder::new(b);
        let op = decode_op(&mut d)?;
        d.finish()?;
        Ok(op)
    };
    let mut n = 0;
    for v in list(j, "valid")? {
        let name = s(v, "name")?;
        let cbor = unhex(s(v, "cbor")?)?;
        let op = op_from(v.get("op").ok_or("missing op")?)?;
        if decode(&cbor).map_err(|e| format!("{name}: {e}"))? != op {
            return Err(format!("{name}: decoded op differs"));
        }
        let mut e = Encoder::new();
        encode_op(&mut e, &op).map_err(|e| e.to_string())?;
        if e.into_bytes() != cbor {
            return Err(format!("{name}: encoding differs"));
        }
        n += 1;
    }
    for v in list(j, "invalid")? {
        let name = s(v, "name")?;
        expect_error(name, decode(&unhex(s(v, "cbor")?)?), s(v, "error")?)?;
        n += 1;
    }
    Ok(n)
}

fn check_merge(j: &J) -> Res<usize> {
    let name = s(j, "name")?;
    let ops: Vec<(Stamp, bothy::Op)> = list(j, "ops")?
        .iter()
        .map(|o| {
            let op = op_from(o.get("op").ok_or("missing op")?)?;
            Ok((Stamp::new(op.hlc, device(s(o, "device")?)?), op))
        })
        .collect::<Res<_>>()?;
    let expected = s(j, "dump")?;
    // Listed order and reverse order must both give the expected dump.
    for order in [false, true] {
        let mut states: BTreeMap<(CollectionName, DocId), DocState> = BTreeMap::new();
        let iter: Box<dyn Iterator<Item = &(Stamp, bothy::Op)>> = if order {
            Box::new(ops.iter().rev())
        } else {
            Box::new(ops.iter())
        };
        for (stamp, op) in iter {
            states
                .entry((op.collection.clone(), op.doc_id.clone()))
                .or_default()
                .apply(&op.kind, *stamp);
        }
        let mut e = Encoder::new();
        e.array(states.len());
        for ((c, d), st) in &states {
            encode_entry(&mut e, c, d, st).map_err(|e| e.to_string())?;
        }
        if hex(&e.into_bytes()) != expected {
            return Err(format!("{name}: dump differs (reversed: {order})"));
        }
        let listed: Vec<_> = list(j, "state")?
            .iter()
            .map(entry_from)
            .collect::<Res<_>>()?;
        let got: Vec<_> = states.into_iter().map(|((c, d), st)| (c, d, st)).collect();
        if listed != got {
            return Err(format!("{name}: state differs (reversed: {order})"));
        }
    }
    Ok(1)
}

fn check_segment(j: &J) -> Res<usize> {
    if let Some(invalid) = j.get("invalid") {
        let mut n = 0;
        for v in invalid.as_array().ok_or("invalid must be an array")? {
            let name = s(v, "name")?;
            let exp = segment::Expected {
                db_id: db(s(v, "db_id")?)?,
                device_id: device(s(v, "device_id")?)?,
                seq: u(v, "seq")?,
            };
            expect_error(
                name,
                segment::decode(&unhex(s(v, "bytes")?)?, &exp),
                s(v, "error")?,
            )?;
            n += 1;
        }
        return Ok(n);
    }
    let name = s(j, "name")?;
    let bytes = unhex(s(j, "bytes")?)?;
    let want = Segment {
        db_id: db(s(j, "db_id")?)?,
        device_id: device(s(j, "device_id")?)?,
        seq: u(j, "seq")?,
        ops: list(j, "ops")?.iter().map(op_from).collect::<Res<_>>()?,
    };
    let exp = segment::Expected {
        db_id: want.db_id,
        device_id: want.device_id,
        seq: want.seq,
    };
    let got = segment::decode(&bytes, &exp).map_err(|e| format!("{name}: {e}"))?;
    if got != want {
        return Err(format!("{name}: decoded segment differs"));
    }
    // zstd output may differ between libraries; only uncompressed bytes must match exactly.
    if j.get("compressed") == Some(&J::Bool(false))
        && segment::encode(&want, false).map_err(|e| e.to_string())? != bytes
    {
        return Err(format!("{name}: encoding differs"));
    }
    Ok(1)
}

fn snapshot_expected(header: &J) -> Res<snapshot::Expected> {
    Ok(snapshot::Expected {
        db_id: db(s(header, "db_id")?)?,
        device_id: device(s(header, "device_id")?)?,
        hlc: hlc(s(header, "hlc")?)?,
    })
}

fn check_snapshot(j: &J) -> Res<usize> {
    let header = j.get("header").ok_or("missing header")?;
    let exp = snapshot_expected(header)?;
    if let Some(invalid) = j.get("invalid") {
        let mut n = 0;
        for v in invalid.as_array().ok_or("invalid must be an array")? {
            let name = s(v, "name")?;
            let bytes = unhex(s(v, "bytes")?)?;
            let r =
                SnapshotReader::open(&bytes, &exp).and_then(|r| r.for_each_entry(|_, _, _| Ok(())));
            expect_error(name, r, s(v, "error")?)?;
            n += 1;
        }
        return Ok(n);
    }
    let name = s(j, "name")?;
    let bytes = unhex(s(j, "bytes")?)?;
    let reader = SnapshotReader::open(&bytes, &exp).map_err(|e| format!("{name}: {e}"))?;
    let vv: BTreeMap<_, _> = header
        .get("vv")
        .and_then(J::as_object)
        .ok_or("missing vv")?
        .iter()
        .map(|(d, seq)| Ok((device(d)?, seq.as_u64().ok_or("vv seq must be an integer")?)))
        .collect::<Res<_>>()?;
    if reader.header().vv != vv {
        return Err(format!("{name}: version vector differs"));
    }
    let mut got = Vec::new();
    reader
        .for_each_entry(|c, d, st| {
            got.push((c, d, st));
            Ok(())
        })
        .map_err(|e| format!("{name}: {e}"))?;
    let want: Vec<_> = list(j, "entries")?
        .iter()
        .map(entry_from)
        .collect::<Res<_>>()?;
    if got != want {
        return Err(format!("{name}: entries differ"));
    }
    if j.get("compressed") == Some(&J::Bool(false)) {
        let mut w = SnapshotWriter::new(reader.header().clone(), false).with_chunk_target(u(
            j,
            "chunk_target",
        )?
            as usize);
        for (c, d, st) in &want {
            w.push(c, d, st).map_err(|e| e.to_string())?;
        }
        if w.finish().map_err(|e| e.to_string())? != bytes {
            return Err(format!("{name}: encoding differs"));
        }
    }
    Ok(1)
}

fn check_manifest(j: &J) -> Res<usize> {
    let key_db = s(j, "key")?.split('/').next().map(db).transpose()?;
    let mut n = 0;
    for v in list(j, "valid")? {
        let name = s(v, "name")?;
        let bytes = serde_json::to_vec(v.get("manifest").ok_or("missing manifest")?)
            .map_err(|e| e.to_string())?;
        Manifest::from_json(&bytes, key_db).map_err(|e| format!("{name}: {e}"))?;
        n += 1;
    }
    for v in list(j, "invalid")? {
        let name = s(v, "name")?;
        let bytes = serde_json::to_vec(v.get("manifest").ok_or("missing manifest")?)
            .map_err(|e| e.to_string())?;
        expect_error(name, Manifest::from_json(&bytes, key_db), s(v, "error")?)?;
        n += 1;
    }
    Ok(n)
}

/// Checks every vector under `dir`. Returns the number of checked cases.
pub fn check_dir(dir: &Path) -> Res<usize> {
    let mut total = 0;
    total += check_hlc(&read(&dir.join("hlc.json"))?)?;
    total += check_values(&read(&dir.join("values.json"))?)?;
    total += check_ops(&read(&dir.join("ops.json"))?)?;
    total += check_manifest(&read(&dir.join("manifest.json"))?)?;
    for (sub, check) in [
        ("merge", check_merge as fn(&J) -> Res<usize>),
        ("segment", check_segment),
        ("snapshot", check_snapshot),
    ] {
        let mut files: Vec<_> = std::fs::read_dir(dir.join(sub))
            .map_err(|e| format!("{sub}: {e}"))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        if files.is_empty() {
            return Err(format!("{sub}: no vector files"));
        }
        for path in files {
            total += check(&read(&path)?).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    total += crate::crypto_vectors::check(dir)?;
    Ok(total)
}
