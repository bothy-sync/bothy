//! `manifest.json` and `device.json`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value as J, json};

use crate::Hlc;
use crate::error::{Error, Result};
use crate::model::{DbId, DeviceId};

/// `bothy_format` written and read by this build.
pub const FORMAT: u64 = 1;

/// Default compression of new segments and snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// zstd.
    Zstd,
    /// No compression.
    None,
}

/// `manifest.encryption`: the cipher and a check value that proves a data key is this
/// database's. Wrapped keys live in key files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptionInfo {
    /// `nonce || ciphertext || tag` of a fixed string under the data key.
    pub key_check: Vec<u8>,
}

const ALG: &str = "xchacha20poly1305";

/// Database-wide settings, written once by the creating device.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    /// Database id.
    pub db_id: DbId,
    /// HLC at creation.
    pub created_hlc: Hlc,
    /// Creating device.
    pub created_by: DeviceId,
    /// Reverse-domain application id, 1 to 255 bytes.
    pub app_id: String,
    /// Application schema version.
    pub app_schema_version: u64,
    /// Default compression for writers.
    pub compression: Compression,
    /// Set for encrypted databases.
    pub encryption: Option<EncryptionInfo>,
    /// Days a covered segment or superseded snapshot is kept before deletion.
    pub grace_period_days: u32,
}

fn corrupt(msg: impl Into<String>) -> Error {
    Error::corrupt(format!("manifest: {}", msg.into()))
}

fn field<'a>(obj: &'a Map<String, J>, key: &str) -> Result<&'a J> {
    obj.get(key)
        .ok_or_else(|| corrupt(format!("missing {key}")))
}

fn str_field<'a>(obj: &'a Map<String, J>, key: &str) -> Result<&'a str> {
    field(obj, key)?
        .as_str()
        .ok_or_else(|| corrupt(format!("{key} must be a string")))
}

fn uint_field(obj: &Map<String, J>, key: &str) -> Result<u64> {
    field(obj, key)?
        .as_u64()
        .ok_or_else(|| corrupt(format!("{key} must be a non-negative integer")))
}

fn hlc_field(obj: &Map<String, J>, key: &str) -> Result<Hlc> {
    str_field(obj, key)?
        .parse()
        .map_err(|_| corrupt(format!("{key} must be 16 lowercase hex characters")))
}

fn object(bytes: &[u8], what: &str) -> Result<Map<String, J>> {
    match serde_json::from_slice::<J>(bytes) {
        Ok(J::Object(m)) => Ok(m),
        Ok(_) => Err(Error::corrupt(format!("{what}: not a JSON object"))),
        Err(e) => Err(Error::corrupt(format!("{what}: invalid JSON")).with_source(e)),
    }
}

fn check_app_id(app_id: &str) -> Result<()> {
    if app_id.is_empty() || app_id.len() > 255 || app_id.chars().any(|c| c.is_control()) {
        return Err(Error::invalid_input(
            "app_id must be 1 to 255 bytes without control characters",
        ));
    }
    Ok(())
}

impl Manifest {
    /// A manifest for a new unencrypted database with default settings.
    pub fn new(db_id: DbId, created_by: DeviceId, created_hlc: Hlc, app_id: &str) -> Result<Self> {
        check_app_id(app_id)?;
        Ok(Self {
            db_id,
            created_hlc,
            created_by,
            app_id: app_id.to_owned(),
            app_schema_version: 0,
            compression: Compression::Zstd,
            encryption: None,
            grace_period_days: 30,
        })
    }

    /// The JSON bytes to upload.
    pub fn to_json(&self) -> Vec<u8> {
        let compression = match self.compression {
            Compression::Zstd => "zstd",
            Compression::None => "none",
        };
        let v = json!({
            "bothy_format": FORMAT,
            "db_id": self.db_id.to_hex(),
            "created_hlc": self.created_hlc.to_hex(),
            "created_by": self.created_by.to_hex(),
            "app_id": self.app_id,
            "app_schema_version": self.app_schema_version,
            "codec": {"serialization": "cbor", "compression": compression},
            "encryption": self.encryption.as_ref().map_or(J::Null, |e| json!({"alg": ALG, "key_check": B64.encode(&e.key_check)})),
            "compaction": {"grace_period_days": self.grace_period_days},
        });
        serde_json::to_vec_pretty(&v).expect("JSON values always serialize")
    }

    /// Parses and validates a manifest. `expected_db_id` comes from the file's key.
    pub fn from_json(bytes: &[u8], expected_db_id: Option<DbId>) -> Result<Self> {
        let obj = object(bytes, "manifest")?;
        let format = field(&obj, "bothy_format")?
            .as_u64()
            .ok_or_else(|| corrupt("bothy_format must be a positive integer"))?;
        if format > FORMAT {
            return Err(Error::format_too_new(format!(
                "database uses bothy_format {format}; this build reads up to {FORMAT}"
            )));
        }
        if format == 0 {
            return Err(corrupt("bothy_format 0"));
        }
        let db_id: DbId = str_field(&obj, "db_id")?
            .parse()
            .map_err(|_| corrupt("db_id must be 32 lowercase hex characters"))?;
        if expected_db_id.is_some_and(|e| e != db_id) {
            return Err(corrupt("db_id does not match the key"));
        }
        let created_by: DeviceId = str_field(&obj, "created_by")?
            .parse()
            .map_err(|_| corrupt("created_by must be 32 lowercase hex characters"))?;
        let app_id = str_field(&obj, "app_id")?;
        check_app_id(app_id).map_err(|e| corrupt(e.message().to_owned()))?;

        let codec = field(&obj, "codec")?
            .as_object()
            .ok_or_else(|| corrupt("codec must be an object"))?;
        match str_field(codec, "serialization")? {
            "cbor" => {}
            other => {
                return Err(Error::format_too_new(format!(
                    "unknown serialization {other}"
                )));
            }
        }
        let compression = match str_field(codec, "compression")? {
            "zstd" => Compression::Zstd,
            "none" => Compression::None,
            other => {
                return Err(Error::format_too_new(format!(
                    "unknown compression {other}"
                )));
            }
        };
        let encryption = match field(&obj, "encryption")? {
            J::Null => None,
            J::Object(e) => {
                match str_field(e, "alg")? {
                    ALG => {}
                    other => return Err(Error::format_too_new(format!("unknown cipher {other}"))),
                }
                let check = B64
                    .decode(str_field(e, "key_check")?)
                    .map_err(|_| corrupt("key_check must be base64"))?;
                Some(EncryptionInfo { key_check: check })
            }
            _ => return Err(corrupt("encryption must be an object or null")),
        };
        let compaction = field(&obj, "compaction")?
            .as_object()
            .ok_or_else(|| corrupt("compaction must be an object"))?;
        let grace = uint_field(compaction, "grace_period_days")?;
        if grace == 0 || grace > u32::MAX as u64 {
            return Err(corrupt("grace_period_days must be at least 1"));
        }
        Ok(Self {
            db_id,
            created_hlc: hlc_field(&obj, "created_hlc")?,
            created_by,
            app_id: app_id.to_owned(),
            app_schema_version: uint_field(&obj, "app_schema_version")?,
            compression,
            encryption,
            grace_period_days: grace as u32,
        })
    }
}

/// A key file (`<db>/keys/<device>/key-<hlc>.json`): the data key wrapped under a password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyFile {
    /// Argon2id memory in KiB.
    pub m_kib: u32,
    /// Argon2id iterations.
    pub t: u32,
    /// Argon2id lanes.
    pub p: u32,
    /// Argon2id salt.
    pub salt: [u8; 16],
    /// `nonce || ciphertext || tag` of the data key under the password-derived key.
    pub wrapped_dek: Vec<u8>,
}

impl KeyFile {
    /// The JSON bytes to upload.
    pub fn to_json(&self) -> Vec<u8> {
        let v = json!({
            "kdf": {"alg": "argon2id", "m_kib": self.m_kib, "t": self.t, "p": self.p, "salt": B64.encode(self.salt)},
            "wrapped_dek": B64.encode(&self.wrapped_dek),
        });
        serde_json::to_vec_pretty(&v).expect("JSON values always serialize")
    }

    /// Parses a key file.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let obj = object(bytes, "key file")?;
        let bad = |m: &str| Error::corrupt(format!("key file: {m}"));
        let kdf = obj
            .get("kdf")
            .and_then(J::as_object)
            .ok_or_else(|| bad("kdf must be an object"))?;
        if kdf.get("alg").and_then(J::as_str) != Some("argon2id") {
            return Err(Error::format_too_new(
                "key file uses an unknown key derivation",
            ));
        }
        let num = |k: &str| -> Result<u32> {
            kdf.get(k)
                .and_then(J::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .filter(|&v| v > 0)
                .ok_or_else(|| bad("kdf parameters must be positive integers"))
        };
        let salt: [u8; 16] = kdf
            .get("salt")
            .and_then(J::as_str)
            .and_then(|s| B64.decode(s).ok())
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| bad("salt must be 16 bytes of base64"))?;
        let wrapped_dek = obj
            .get("wrapped_dek")
            .and_then(J::as_str)
            .and_then(|s| B64.decode(s).ok())
            .filter(|b| b.len() == 24 + 32 + 16)
            .ok_or_else(|| bad("wrapped_dek must be 72 bytes of base64"))?;
        Ok(Self {
            m_kib: num("m_kib")?,
            t: num("t")?,
            p: num("p")?,
            salt,
            wrapped_dek,
        })
    }
}

/// Contents of `device.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The device.
    pub device_id: DeviceId,
    /// Human-readable name.
    pub name: Option<String>,
    /// Platform, e.g. `linux-aarch64`.
    pub platform: Option<String>,
    /// Bothy version that wrote the file.
    pub bothy_version: Option<String>,
    /// HLC when the device joined.
    pub created_hlc: Hlc,
}

impl DeviceInfo {
    /// The JSON bytes to upload.
    pub fn to_json(&self) -> Vec<u8> {
        let mut m = Map::new();
        m.insert("device_id".into(), J::String(self.device_id.to_hex()));
        m.insert("created_hlc".into(), J::String(self.created_hlc.to_hex()));
        for (k, v) in [
            ("name", &self.name),
            ("platform", &self.platform),
            ("bothy_version", &self.bothy_version),
        ] {
            if let Some(v) = v {
                m.insert(k.into(), J::String(v.clone()));
            }
        }
        serde_json::to_vec_pretty(&J::Object(m)).expect("JSON values always serialize")
    }

    /// Parses `device.json`. `expected` comes from the file's key.
    pub fn from_json(bytes: &[u8], expected: DeviceId) -> Result<Self> {
        let obj = object(bytes, "device.json")?;
        let bad = |m: &str| Error::corrupt(format!("device.json: {m}"));
        let device_id: DeviceId = obj
            .get("device_id")
            .and_then(J::as_str)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad("device_id must be 32 lowercase hex characters"))?;
        if device_id != expected {
            return Err(bad("device_id does not match the key"));
        }
        let created_hlc = obj
            .get("created_hlc")
            .and_then(J::as_str)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad("created_hlc must be 16 lowercase hex characters"))?;
        let opt = |k: &str| obj.get(k).and_then(J::as_str).map(str::to_owned);
        Ok(Self {
            device_id,
            name: opt("name"),
            platform: opt("platform"),
            bothy_version: opt("bothy_version"),
            created_hlc,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    fn sample() -> Manifest {
        Manifest::new(
            DbId::from_bytes([0xab; 16]),
            DeviceId::from_bytes([0xcd; 16]),
            Hlc::from_u64(0x018bcfe568000001),
            "com.example.notes",
        )
        .unwrap()
    }

    fn edited(f: impl FnOnce(&mut Map<String, J>)) -> Vec<u8> {
        let mut v: Map<String, J> = serde_json::from_slice(&sample().to_json()).unwrap();
        f(&mut v);
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn round_trip_and_unknown_fields_are_ignored() {
        let m = sample();
        assert_eq!(Manifest::from_json(&m.to_json(), Some(m.db_id)).unwrap(), m);
        let extra = edited(|v| {
            v.insert("future_field".into(), json!(1));
        });
        assert_eq!(Manifest::from_json(&extra, None).unwrap(), m);
    }

    #[test]
    fn newer_format_and_codecs_are_format_too_new() {
        for bytes in [
            edited(|v| {
                v.insert("bothy_format".into(), json!(2));
            }),
            edited(|v| {
                v.insert(
                    "codec".into(),
                    json!({"serialization": "msgpack", "compression": "zstd"}),
                );
            }),
            edited(|v| {
                v.insert(
                    "codec".into(),
                    json!({"serialization": "cbor", "compression": "lz4"}),
                );
            }),
        ] {
            assert_eq!(
                Manifest::from_json(&bytes, None).unwrap_err().kind(),
                ErrorKind::FormatTooNew
            );
        }
    }

    #[test]
    fn malformed_manifests_are_corrupt() {
        let cases = [
            b"[]".to_vec(),
            b"not json".to_vec(),
            edited(|v| {
                v.remove("app_id");
            }),
            edited(|v| {
                v.insert("db_id".into(), json!("ABAB"));
            }),
            edited(|v| {
                v.insert("compaction".into(), json!({"grace_period_days": 0}));
            }),
            edited(|v| {
                v.insert("encryption".into(), json!("yes"));
            }),
            edited(|v| {
                v.insert("bothy_format".into(), json!(0));
            }),
        ];
        for bytes in cases {
            assert_eq!(
                Manifest::from_json(&bytes, None).unwrap_err().kind(),
                ErrorKind::Corrupt
            );
        }
        let m = sample();
        let other = DbId::from_bytes([1; 16]);
        assert!(Manifest::from_json(&m.to_json(), Some(other)).is_err());
    }

    #[test]
    fn device_info_round_trip() {
        let d = DeviceInfo {
            device_id: DeviceId::from_bytes([7; 16]),
            name: Some("laptop".into()),
            platform: None,
            bothy_version: Some("0.1.0".into()),
            created_hlc: Hlc::from_u64(5),
        };
        assert_eq!(DeviceInfo::from_json(&d.to_json(), d.device_id).unwrap(), d);
        assert!(DeviceInfo::from_json(&d.to_json(), DeviceId::from_bytes([8; 16])).is_err());
    }
}
