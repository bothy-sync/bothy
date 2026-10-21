//! Client-side encryption (feature `crypto`): Argon2id key derivation and XChaCha20-Poly1305.
//!
//! A random data key (DEK) encrypts every remote file. The DEK is wrapped with a key derived
//! from the password and stored in key files (`<db>/keys/<device>/key-<hlc>.json`). Only
//! RustCrypto primitives are used; key material is zeroized on drop.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::{Zeroize, Zeroizing};

use crate::error::{Error, ErrorKind, Result};
use crate::model::DbId;

/// Nonce length.
pub const NONCE_LEN: usize = 24;
/// Authentication tag length.
pub const TAG_LEN: usize = 16;
/// Length of a sealed value's overhead (nonce, then ciphertext, then tag).
pub const SEALED_OVERHEAD: usize = NONCE_LEN + TAG_LEN;

const KEY_CHECK: &[u8] = b"bothy-key-check-v1";
const DEK_AAD_PREFIX: &[u8] = b"bothy-dek-v1";

/// The data key. Cheap to clone; zeroized when the last copy is dropped.
#[derive(Clone)]
pub struct DataKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for DataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataKey(..)")
    }
}

impl PartialEq for DataKey {
    fn eq(&self, other: &Self) -> bool {
        // Constant-time enough for a local comparison of two in-memory keys.
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

impl Eq for DataKey {}

impl DataKey {
    /// A new random key.
    pub fn random() -> Result<Self> {
        let mut k = Zeroizing::new([0u8; 32]);
        getrandom::fill(k.as_mut()).map_err(|_| internal("system random generator failed"))?;
        Ok(Self(k))
    }

    /// Wraps raw key bytes, e.g. from an app's secure storage.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let mut b = bytes;
        let k = Self(Zeroizing::new(b));
        b.zeroize();
        k
    }

    /// The raw key bytes, for an app that caches the key in secure storage.
    pub fn to_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(*self.0)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Argon2id parameters stored with each key file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_kib: u32,
    /// Iterations.
    pub t: u32,
    /// Lanes.
    pub p: u32,
    /// Random salt.
    pub salt: [u8; 16],
}

impl KdfParams {
    /// The default cost (64 MiB, 3 passes, 1 lane) with a new random salt. Provisional until
    /// measured on weak devices.
    pub fn new_default() -> Result<Self> {
        Self::with_cost(64 * 1024, 3, 1)
    }

    /// Custom cost with a new random salt (weak devices, tests).
    pub fn with_cost(m_kib: u32, t: u32, p: u32) -> Result<Self> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|_| internal("system random generator failed"))?;
        Ok(Self { m_kib, t, p, salt })
    }
}

fn internal(msg: &'static str) -> Error {
    Error::new(ErrorKind::Internal, msg)
}

fn random_nonce() -> Result<[u8; NONCE_LEN]> {
    let mut n = [0u8; NONCE_LEN];
    getrandom::fill(&mut n).map_err(|_| internal("system random generator failed"))?;
    Ok(n)
}

/// Derives the key-encryption key from a password.
pub fn derive_kek(password: &str, kdf: &KdfParams) -> Result<Zeroizing<[u8; 32]>> {
    let params = argon2::Params::new(kdf.m_kib, kdf.t, kdf.p, Some(32))
        .map_err(|_| Error::invalid_input("invalid Argon2id parameters"))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password.as_bytes(), &kdf.salt, out.as_mut())
        .map_err(|_| Error::invalid_input("Argon2id failed (bad parameters)"))?;
    Ok(out)
}

/// Encrypts with an explicit nonce: `ciphertext || tag`.
pub fn encrypt_with_nonce(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(&Key::from(*key));
    cipher
        .encrypt(
            &XNonce::from(*nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| internal("encryption failed"))
}

/// Decrypts `ciphertext || tag`. Fails for a wrong key or tampered data.
pub fn decrypt(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
    aad: &[u8],
) -> Option<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(&Key::from(*key));
    cipher
        .decrypt(
            &XNonce::from(*nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .ok()
}

/// Seals with a fresh random nonce: `nonce || ciphertext || tag`.
pub fn seal(key: &[u8; 32], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    seal_with_nonce(key, &random_nonce()?, plaintext, aad)
}

/// Seals with an explicit nonce (conformance vectors).
pub fn seal_with_nonce(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(plaintext.len() + SEALED_OVERHEAD);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&encrypt_with_nonce(key, nonce, plaintext, aad)?);
    Ok(out)
}

/// Opens `nonce || ciphertext || tag`.
pub fn open(key: &[u8; 32], sealed: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < SEALED_OVERHEAD {
        return None;
    }
    let (nonce, ct) = sealed.split_at(NONCE_LEN);
    decrypt(key, nonce.try_into().expect("24 bytes"), ct, aad)
}

/// A fresh random nonce, for writers.
pub fn new_nonce() -> Result<[u8; NONCE_LEN]> {
    random_nonce()
}

/// `key_check` for the manifest: proves a DEK belongs to this database.
pub fn key_check(dek: &DataKey, db_id: &DbId, nonce: Option<[u8; NONCE_LEN]>) -> Result<Vec<u8>> {
    let nonce = match nonce {
        Some(n) => n,
        None => random_nonce()?,
    };
    seal_with_nonce(dek.as_bytes(), &nonce, KEY_CHECK, db_id.as_bytes())
}

/// Whether `dek` matches the manifest's `key_check`.
pub fn verify_key_check(dek: &DataKey, db_id: &DbId, check: &[u8]) -> bool {
    open(dek.as_bytes(), check, db_id.as_bytes()).is_some_and(|p| p == KEY_CHECK)
}

fn dek_aad(db_id: &DbId, key_file: &str) -> Vec<u8> {
    let mut aad = DEK_AAD_PREFIX.to_vec();
    aad.extend_from_slice(db_id.as_bytes());
    aad.extend_from_slice(key_file.as_bytes());
    aad
}

/// Wraps the DEK under a password for the key file named `key_file` (its logical key).
pub fn wrap_dek(
    dek: &DataKey,
    password: &str,
    kdf: &KdfParams,
    db_id: &DbId,
    key_file: &str,
    nonce: Option<[u8; NONCE_LEN]>,
) -> Result<Vec<u8>> {
    let kek = derive_kek(password, kdf)?;
    let nonce = match nonce {
        Some(n) => n,
        None => random_nonce()?,
    };
    seal_with_nonce(&kek, &nonce, dek.as_bytes(), &dek_aad(db_id, key_file))
}

/// Unwraps the DEK; `WrongPassword` if the password (or the file) does not match.
pub fn unwrap_dek(
    password: &str,
    kdf: &KdfParams,
    wrapped: &[u8],
    db_id: &DbId,
    key_file: &str,
) -> Result<DataKey> {
    let kek = derive_kek(password, kdf)?;
    let mut plain = open(&kek, wrapped, &dek_aad(db_id, key_file)).ok_or_else(|| {
        Error::new(
            ErrorKind::WrongPassword,
            "the password does not unlock this database",
        )
    })?;
    let bytes: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| Error::corrupt("wrapped key has the wrong length"))?;
    plain.zeroize();
    Ok(DataKey::from_bytes(bytes))
}

/// Seals a JSON document (e.g. `device.json`) for an encrypted database:
/// `{"sealed": "xchacha20poly1305", "nonce": ..., "ct": ...}` with `aad` = its logical key.
pub fn seal_json(key: &DataKey, logical_key: &str, json: &[u8]) -> Result<Vec<u8>> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    let nonce = random_nonce()?;
    let ct = encrypt_with_nonce(key.as_bytes(), &nonce, json, logical_key.as_bytes())?;
    let v = serde_json::json!({"sealed": "xchacha20poly1305", "nonce": B64.encode(nonce), "ct": B64.encode(ct)});
    Ok(serde_json::to_vec_pretty(&v).expect("JSON values always serialize"))
}

/// Opens a sealed JSON document. `Corrupt` if it is not sealed or does not authenticate.
pub fn open_json(key: &DataKey, logical_key: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    let bad = || Error::corrupt(format!("{logical_key}: not a valid sealed document"));
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| bad())?;
    if v["sealed"] != "xchacha20poly1305" {
        return Err(bad());
    }
    let nonce: [u8; NONCE_LEN] = v["nonce"]
        .as_str()
        .and_then(|s| B64.decode(s).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or_else(bad)?;
    let ct = v["ct"]
        .as_str()
        .and_then(|s| B64.decode(s).ok())
        .ok_or_else(bad)?;
    decrypt(key.as_bytes(), &nonce, &ct, logical_key.as_bytes()).ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> KdfParams {
        KdfParams {
            m_kib: 64,
            t: 1,
            p: 1,
            salt: [7; 16],
        }
    }

    #[test]
    fn seal_and_open_with_aad() {
        let k = [1u8; 32];
        let s = seal(&k, b"hello", b"aad").unwrap();
        assert_eq!(s.len(), 5 + SEALED_OVERHEAD);
        assert_eq!(open(&k, &s, b"aad").unwrap(), b"hello");
        assert!(open(&k, &s, b"other").is_none());
        assert!(open(&[2u8; 32], &s, b"aad").is_none());
        let mut t = s.clone();
        t[30] ^= 1;
        assert!(open(&k, &t, b"aad").is_none());
        assert_ne!(
            seal(&k, b"hello", b"aad").unwrap(),
            s,
            "fresh nonce each time"
        );
    }

    #[test]
    fn wrap_unwrap_and_wrong_password() {
        let dek = DataKey::random().unwrap();
        let db = DbId::from_bytes([3; 16]);
        let file = "db/keys/dev/key-0000000000000001.json";
        let w = wrap_dek(&dek, "correct horse", &fast(), &db, file, None).unwrap();
        assert_eq!(
            unwrap_dek("correct horse", &fast(), &w, &db, file).unwrap(),
            dek
        );
        let err = unwrap_dek("wrong", &fast(), &w, &db, file).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::WrongPassword);
        // A wrapped key copied under another name does not open.
        assert!(unwrap_dek("correct horse", &fast(), &w, &db, "db/keys/dev/key-2.json").is_err());
    }

    #[test]
    fn key_check_binds_dek_and_database() {
        let dek = DataKey::random().unwrap();
        let db = DbId::from_bytes([3; 16]);
        let c = key_check(&dek, &db, None).unwrap();
        assert!(verify_key_check(&dek, &db, &c));
        assert!(!verify_key_check(&DataKey::random().unwrap(), &db, &c));
        assert!(!verify_key_check(&dek, &DbId::from_bytes([4; 16]), &c));
    }

    #[test]
    fn sealed_json_binds_the_logical_key() {
        let k = DataKey::random().unwrap();
        let s = seal_json(&k, "db/devices/x/device.json", br#"{"name":"laptop"}"#).unwrap();
        assert!(!String::from_utf8_lossy(&s).contains("laptop"));
        assert_eq!(
            open_json(&k, "db/devices/x/device.json", &s).unwrap(),
            br#"{"name":"laptop"}"#
        );
        assert!(open_json(&k, "db/devices/y/device.json", &s).is_err());
        assert!(open_json(&k, "db/devices/x/device.json", br#"{"name":"laptop"}"#).is_err());
    }

    #[test]
    fn debug_never_shows_key_bytes() {
        let dek = DataKey::from_bytes([0xab; 32]);
        assert_eq!(format!("{dek:?}"), "DataKey(..)");
    }

    #[test]
    fn kdf_is_deterministic_for_fixed_inputs() {
        let a = derive_kek("pw", &fast()).unwrap();
        let b = derive_kek("pw", &fast()).unwrap();
        assert_eq!(*a, *b);
        assert_ne!(*a, *derive_kek("pw2", &fast()).unwrap());
    }
}
