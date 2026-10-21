//! Encryption vectors: key derivation, key check, key files, sealed JSON, and encrypted
//! segments and snapshots, all with fixed keys and nonces.

use std::collections::BTreeMap;
use std::path::Path;

use bothy::codec::keys::LogicalKey;
use bothy::codec::manifest::KeyFile;
use bothy::codec::segment::{self, Segment};
use bothy::codec::snapshot::{self, SnapshotHeader, SnapshotReader, SnapshotWriter};
use bothy::crypto::{self, DataKey, KdfParams};
use bothy::merge::DocState;
use bothy::{CollectionName, DocId, ErrorKind, Hlc, Op, OpKind, Stamp, Value};
use serde_json::{Value as J, json};

use crate::generate::{DB, DEV_A, DEV_B};
use crate::typed::{Res, entry_from, entry_to, hex, op_from, op_to, s, u, unhex};

const DEK: [u8; 32] = [0x42; 32];
const PASSWORD: &str = "correct horse battery staple";

fn nonce(b: u8) -> [u8; 24] {
    [b; 24]
}

fn ops() -> Vec<Op> {
    let h = |l| Hlc::from_parts(1_700_000_000_000, l).unwrap();
    vec![
        Op {
            hlc: h(0),
            collection: CollectionName::new("notes").unwrap(),
            doc_id: DocId::new("n1").unwrap(),
            kind: OpKind::SetFields(
                [(
                    bothy::FieldName::new("title").unwrap(),
                    Value::from("secret"),
                )]
                .into(),
            ),
        },
        Op {
            hlc: h(1),
            collection: CollectionName::new("notes").unwrap(),
            doc_id: DocId::new("n2").unwrap(),
            kind: OpKind::DeleteDoc,
        },
    ]
}

fn crypto_json() -> J {
    let dek = DataKey::from_bytes(DEK);
    let kdf_cases: Vec<J> = [(64u32, 1u32, 1u32), (65536, 3, 1)]
        .iter()
        .map(|&(m, t, p)| {
            let kdf = KdfParams { m_kib: m, t, p, salt: [0x5a; 16] };
            let kek = crypto::derive_kek(PASSWORD, &kdf).unwrap();
            json!({"password": PASSWORD, "m_kib": m, "t": t, "p": p, "salt": hex(&kdf.salt), "kek": hex(kek.as_ref())})
        })
        .collect();
    let key_file_name =
        LogicalKey::key_file(DB, DEV_A, Hlc::from_u64(0x018bcfe568000000)).to_string();
    let kdf = KdfParams {
        m_kib: 64,
        t: 1,
        p: 1,
        salt: [0x5a; 16],
    };
    let wrapped =
        crypto::wrap_dek(&dek, PASSWORD, &kdf, &DB, &key_file_name, Some(nonce(1))).unwrap();
    let file = KeyFile {
        m_kib: kdf.m_kib,
        t: kdf.t,
        p: kdf.p,
        salt: kdf.salt,
        wrapped_dek: wrapped.clone(),
    };
    let check = crypto::key_check(&dek, &DB, Some(nonce(2))).unwrap();
    let device_key = LogicalKey::device(DB, DEV_A).to_string();
    let device_json = br#"{"device_id":"a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1"}"#;
    let ct =
        crypto::encrypt_with_nonce(&DEK, &nonce(3), device_json, device_key.as_bytes()).unwrap();
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let sealed = json!({"sealed": "xchacha20poly1305", "nonce": b64.encode(nonce(3)), "ct": b64.encode(&ct)});
    json!({
        "about": "Argon2id (v0x13, 32-byte output) and XChaCha20-Poly1305. Sealed values are nonce || ciphertext || tag.",
        "kdf": kdf_cases,
        "db_id": DB.to_hex(),
        "dek": hex(&DEK),
        "key_check": {"nonce": hex(&nonce(2)), "aad": "db_id bytes", "value": hex(&check)},
        "key_file": {
            "key": key_file_name,
            "password": PASSWORD,
            "aad": "\"bothy-dek-v1\" || db_id bytes || key (UTF-8)",
            "json": serde_json::from_slice::<J>(&file.to_json()).unwrap(),
            "wrapped_dek": hex(&wrapped),
        },
        "sealed_json": {"key": device_key, "plaintext": String::from_utf8_lossy(device_json), "sealed": sealed},
        "wrong_password": "Tr0ub4dor&3",
    })
}

fn segment_json() -> J {
    let dek = DataKey::from_bytes(DEK);
    let seg = Segment {
        db_id: DB,
        device_id: DEV_A,
        seq: 1,
        ops: ops(),
    };
    let plain = segment::encode_with(&seg, false, Some((&dek, nonce(7)))).unwrap();
    let zstd = segment::encode_with(&seg, true, Some((&dek, nonce(8)))).unwrap();
    let mut flipped = plain.clone();
    flipped[100] ^= 1;
    let n = flipped.len() - 32;
    let t = blake3::hash(&flipped[..n]);
    flipped[n..].copy_from_slice(t.as_bytes());
    let unencrypted = segment::encode(&seg, false).unwrap();
    json!({
        "key": hex(&DEK),
        "db_id": DB.to_hex(),
        "device_id": DEV_A.to_hex(),
        "seq": 1,
        "ops": ops().iter().map(op_to).collect::<Vec<_>>(),
        "valid": [
            {"name": "encrypted", "compressed": false, "nonce": hex(&nonce(7)), "bytes": hex(&plain)},
            {"name": "encrypted_zstd", "compressed": true, "nonce": hex(&nonce(8)), "bytes": hex(&zstd)},
        ],
        "invalid": [
            {"name": "ciphertext_flipped_and_resealed", "bytes": hex(&flipped), "key": hex(&DEK), "error": "corrupt"},
            {"name": "wrong_key", "bytes": hex(&plain), "key": hex(&[0x43; 32]), "error": "corrupt"},
            {"name": "plaintext_in_encrypted_database", "bytes": hex(&unencrypted), "key": hex(&DEK), "error": "corrupt"},
            {"name": "no_key", "bytes": hex(&plain), "error": "key_required"},
        ],
    })
}

fn snapshot_json() -> J {
    let dek = DataKey::from_bytes(DEK);
    let mut states: BTreeMap<(CollectionName, DocId), DocState> = BTreeMap::new();
    for (i, op) in ops().iter().enumerate() {
        let dev = if i == 0 { DEV_A } else { DEV_B };
        states
            .entry((op.collection.clone(), op.doc_id.clone()))
            .or_default()
            .apply(&op.kind, Stamp::new(op.hlc, dev));
    }
    let header = SnapshotHeader {
        db_id: DB,
        device_id: DEV_A,
        hlc: Hlc::from_u64(99),
        vv: [(DEV_A, 1), (DEV_B, 1)].into(),
    };
    let nonces = vec![nonce(9), nonce(10)];
    let mut w = SnapshotWriter::new(header.clone(), false)
        .with_chunk_target(1)
        .encrypted(dek)
        .with_nonces(nonces.clone());
    for ((c, d), st) in &states {
        w.push(c, d, st).unwrap();
    }
    let bytes = w.finish().unwrap();
    json!({
        "key": hex(&DEK),
        "header": {
            "db_id": DB.to_hex(),
            "device_id": DEV_A.to_hex(),
            "hlc": header.hlc.to_hex(),
            "vv": header.vv.iter().map(|(d, s)| (d.to_hex(), J::from(*s))).collect::<serde_json::Map<_, _>>(),
        },
        "chunk_target": 1,
        "nonces": nonces.iter().map(|n| hex(n)).collect::<Vec<_>>(),
        "entries": states.iter().map(|((c, d), st)| entry_to(c, d, st)).collect::<Vec<_>>(),
        "bytes": hex(&bytes),
    })
}

/// Encryption vector files.
pub fn all() -> Vec<(String, J)> {
    vec![
        ("crypto.json".into(), crypto_json()),
        ("encrypted/segment.json".into(), segment_json()),
        ("encrypted/snapshot.json".into(), snapshot_json()),
    ]
}

fn key(t: &str) -> Res<DataKey> {
    let b: [u8; 32] = unhex(t)?.try_into().map_err(|_| "key must be 32 bytes")?;
    Ok(DataKey::from_bytes(b))
}

fn nonce_from(t: &str) -> Res<[u8; 24]> {
    unhex(t)?
        .try_into()
        .map_err(|_| "nonce must be 24 bytes".to_owned())
}

fn check_crypto(j: &J) -> Res<usize> {
    let mut n = 0;
    for c in j["kdf"].as_array().ok_or("kdf")? {
        let kdf = KdfParams {
            m_kib: u(c, "m_kib")? as u32,
            t: u(c, "t")? as u32,
            p: u(c, "p")? as u32,
            salt: unhex(s(c, "salt")?)?.try_into().map_err(|_| "salt")?,
        };
        let kek = crypto::derive_kek(s(c, "password")?, &kdf).map_err(|e| e.to_string())?;
        if hex(kek.as_ref()) != s(c, "kek")? {
            return Err(format!("kek differs for m_kib {}", kdf.m_kib));
        }
        n += 1;
    }
    let db: bothy::DbId = s(j, "db_id")?.parse().map_err(|_| "db_id")?;
    let dek = key(s(j, "dek")?)?;
    let check = unhex(s(&j["key_check"], "value")?)?;
    if !crypto::verify_key_check(&dek, &db, &check) {
        return Err("key_check does not verify".into());
    }
    let kf = &j["key_file"];
    let file =
        KeyFile::from_json(&serde_json::to_vec(&kf["json"]).unwrap()).map_err(|e| e.to_string())?;
    let kdf = KdfParams {
        m_kib: file.m_kib,
        t: file.t,
        p: file.p,
        salt: file.salt,
    };
    let name = s(kf, "key")?;
    let got = crypto::unwrap_dek(s(kf, "password")?, &kdf, &file.wrapped_dek, &db, name)
        .map_err(|e| e.to_string())?;
    if got != dek {
        return Err("key file unwraps to another key".into());
    }
    match crypto::unwrap_dek(s(j, "wrong_password")?, &kdf, &file.wrapped_dek, &db, name) {
        Err(e) if e.kind() == ErrorKind::WrongPassword => {}
        other => {
            return Err(format!(
                "wrong password: expected WrongPassword, got {other:?}"
            ));
        }
    }
    let sj = &j["sealed_json"];
    let opened = crypto::open_json(
        &dek,
        s(sj, "key")?,
        &serde_json::to_vec(&sj["sealed"]).unwrap(),
    )
    .map_err(|e| e.to_string())?;
    if opened != s(sj, "plaintext")?.as_bytes() {
        return Err("sealed JSON opens to other bytes".into());
    }
    Ok(n + 4)
}

fn check_segment(j: &J) -> Res<usize> {
    let exp = segment::Expected {
        db_id: s(j, "db_id")?.parse().map_err(|_| "db_id")?,
        device_id: s(j, "device_id")?.parse().map_err(|_| "device_id")?,
        seq: u(j, "seq")?,
    };
    let want = Segment {
        db_id: exp.db_id,
        device_id: exp.device_id,
        seq: exp.seq,
        ops: j["ops"]
            .as_array()
            .ok_or("ops")?
            .iter()
            .map(op_from)
            .collect::<Res<_>>()?,
    };
    let dek = key(s(j, "key")?)?;
    let mut n = 0;
    for v in j["valid"].as_array().ok_or("valid")? {
        let name = s(v, "name")?;
        let bytes = unhex(s(v, "bytes")?)?;
        let got =
            segment::decode_with(&bytes, &exp, Some(&dek)).map_err(|e| format!("{name}: {e}"))?;
        if got != want {
            return Err(format!("{name}: decoded segment differs"));
        }
        if v["compressed"] == false {
            let again =
                segment::encode_with(&want, false, Some((&dek, nonce_from(s(v, "nonce")?)?)))
                    .map_err(|e| e.to_string())?;
            if again != bytes {
                return Err(format!("{name}: encoding differs"));
            }
        }
        n += 1;
    }
    for v in j["invalid"].as_array().ok_or("invalid")? {
        let name = s(v, "name")?;
        let bytes = unhex(s(v, "bytes")?)?;
        let k = v.get("key").and_then(J::as_str).map(key).transpose()?;
        let want_err = s(v, "error")?;
        match segment::decode_with(&bytes, &exp, k.as_ref()) {
            Err(e) if kind(e.kind()) == want_err => {}
            other => {
                return Err(format!(
                    "{name}: expected {want_err}, got {:?}",
                    other.map(|_| ())
                ));
            }
        }
        n += 1;
    }
    Ok(n)
}

fn kind(k: ErrorKind) -> &'static str {
    match k {
        ErrorKind::Corrupt => "corrupt",
        ErrorKind::KeyRequired => "key_required",
        ErrorKind::FormatTooNew => "format_too_new",
        _ => "other",
    }
}

fn check_snapshot(j: &J) -> Res<usize> {
    let h = &j["header"];
    let exp = snapshot::Expected {
        db_id: s(h, "db_id")?.parse().map_err(|_| "db_id")?,
        device_id: s(h, "device_id")?.parse().map_err(|_| "device_id")?,
        hlc: s(h, "hlc")?.parse().map_err(|_| "hlc")?,
    };
    let dek = key(s(j, "key")?)?;
    let bytes = unhex(s(j, "bytes")?)?;
    let r = SnapshotReader::open_with(&bytes, &exp, Some(&dek)).map_err(|e| e.to_string())?;
    let mut got = Vec::new();
    r.for_each_entry(|c, d, st| {
        got.push((c, d, st));
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    let want: Vec<_> = j["entries"]
        .as_array()
        .ok_or("entries")?
        .iter()
        .map(entry_from)
        .collect::<Res<_>>()?;
    if got != want {
        return Err("snapshot entries differ".into());
    }
    let nonces: Vec<[u8; 24]> = j["nonces"]
        .as_array()
        .ok_or("nonces")?
        .iter()
        .map(|n| nonce_from(n.as_str().unwrap_or_default()))
        .collect::<Res<_>>()?;
    let mut w = SnapshotWriter::new(r.header().clone(), false)
        .with_chunk_target(u(j, "chunk_target")? as usize)
        .encrypted(dek)
        .with_nonces(nonces);
    for (c, d, st) in &want {
        w.push(c, d, st).map_err(|e| e.to_string())?;
    }
    if w.finish().map_err(|e| e.to_string())? != bytes {
        return Err("snapshot encoding differs".into());
    }
    Ok(1)
}

fn read(path: &Path) -> Res<J> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Checks the encryption vectors under `dir`.
pub fn check(dir: &Path) -> Res<usize> {
    Ok(check_crypto(&read(&dir.join("crypto.json"))?)?
        + check_segment(&read(&dir.join("encrypted/segment.json"))?)?
        + check_snapshot(&read(&dir.join("encrypted/snapshot.json"))?)?)
}
