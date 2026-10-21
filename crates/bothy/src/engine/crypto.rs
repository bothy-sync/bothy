//! Key files, unlocking and password changes for encrypted databases (ADR-0009).

use std::sync::Arc;

use crate::Hlc;
use crate::codec::keys::{KeyKind, LogicalKey};
use crate::codec::manifest::{EncryptionInfo, KeyFile};
use crate::crypto::{self, DataKey, KdfParams};
use crate::database::{Inner, Secret};
use crate::error::{Error, ErrorKind, Result};
use crate::model::{DbId, DeviceId};
use crate::remote::RemoteEntry;

fn key_required() -> Error {
    Error::new(
        ErrorKind::KeyRequired,
        "the database is encrypted; unlock it with its password or key",
    )
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::new(ErrorKind::Internal, "key derivation task failed").with_source(e))?
}

/// The newest key file on the remote: `(hlc, owner, logical key)`.
async fn newest_remote(inner: &Inner, db_id: DbId) -> Result<Option<(Hlc, DeviceId, String)>> {
    let mut best: Option<(Hlc, DeviceId, String)> = None;
    for e in inner.remote.list(&format!("{db_id}/keys/")).await? {
        if let Some(LogicalKey {
            kind: KeyKind::KeyFile(dev, hlc),
            ..
        }) = LogicalKey::parse(&e.key)
            && best.as_ref().is_none_or(|(h, _, _)| hlc > *h)
        {
            best = Some((hlc, dev, e.key));
        }
    }
    Ok(best)
}

/// Downloads the newest key file into the local cache if it is newer. Returns its owner.
async fn refresh(inner: &Arc<Inner>, db_id: DbId) -> Result<Option<DeviceId>> {
    let Some((hlc, owner, key)) = newest_remote(inner, db_id).await? else {
        return Ok(None);
    };
    if let Some(bytes) = inner.remote.get(&key).await? {
        KeyFile::from_json(&bytes)?;
        inner
            .store(move |s| s.cache_key_file(hlc, &key, &bytes))
            .await?;
    }
    Ok(Some(owner))
}

/// Unlocks with a password against the cached newest key file (works offline).
pub(crate) async fn unlock_password(inner: &Arc<Inner>, db_id: DbId, password: &str) -> Result<()> {
    let (check, cached) = inner
        .store(|s| Ok((s.key_check()?, s.cached_key_file()?)))
        .await?;
    let check = check.ok_or_else(|| Error::invalid_input("the database is not encrypted"))?;
    let cached = match cached {
        Some(c) => c,
        None => {
            refresh(inner, db_id).await?;
            inner.store(|s| s.cached_key_file()).await?.ok_or_else(|| {
                Error::new(
                    ErrorKind::RemoteInconsistent,
                    "encrypted database has no key file",
                )
            })?
        }
    };
    let (_, name, json) = cached;
    let file = KeyFile::from_json(&json)?;
    let password = zeroize::Zeroizing::new(password.to_owned());
    let dek = blocking(move || {
        let kdf = KdfParams {
            m_kib: file.m_kib,
            t: file.t,
            p: file.p,
            salt: file.salt,
        };
        crypto::unwrap_dek(&password, &kdf, &file.wrapped_dek, &db_id, &name)
    })
    .await?;
    if !crypto::verify_key_check(&dek, &db_id, &check) {
        return Err(Error::new(
            ErrorKind::WrongPassword,
            "the password does not unlock this database",
        ));
    }
    inner.set_key(Some(dek));
    Ok(())
}

/// Unlocks with a data key the app cached earlier.
pub(crate) async fn unlock_key(inner: &Arc<Inner>, db_id: DbId, dek: DataKey) -> Result<()> {
    let check = inner
        .store(|s| s.key_check())
        .await?
        .ok_or_else(|| Error::invalid_input("the database is not encrypted"))?;
    if !crypto::verify_key_check(&dek, &db_id, &check) {
        return Err(Error::new(
            ErrorKind::WrongPassword,
            "this key does not belong to the database",
        ));
    }
    inner.set_key(Some(dek));
    Ok(())
}

/// Unlocks with the secret from `Config`, if any. Called on open (offline) and on join.
pub(crate) async fn unlock_with_secret(inner: &Arc<Inner>, db_id: DbId) -> Result<bool> {
    let secret = inner
        .secret
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    match secret {
        Some(Secret::Password(p)) => unlock_password(inner, db_id, &p).await.map(|_| true),
        Some(Secret::Key(k)) => unlock_key(inner, db_id, k).await.map(|_| true),
        None => Ok(false),
    }
}

/// Wraps `dek` under `password` in a new key file of this device, uploads and caches it.
async fn write_key_file(
    inner: &Arc<Inner>,
    db_id: DbId,
    dek: &DataKey,
    password: &str,
) -> Result<String> {
    let hlc = inner.store(|s| s.tick()).await?;
    let name = LogicalKey::key_file(db_id, inner.device(), hlc).to_string();
    let (m, t, p) = inner.kdf_cost;
    let (dek2, password, name2) = (
        dek.clone(),
        zeroize::Zeroizing::new(password.to_owned()),
        name.clone(),
    );
    let file = blocking(move || {
        let kdf = KdfParams::with_cost(m, t, p)?;
        let wrapped = crypto::wrap_dek(&dek2, &password, &kdf, &db_id, &name2, None)?;
        Ok(KeyFile {
            m_kib: kdf.m_kib,
            t: kdf.t,
            p: kdf.p,
            salt: kdf.salt,
            wrapped_dek: wrapped,
        })
    })
    .await?;
    let json = file.to_json();
    super::put_once(inner, &LogicalKey::parse(&name).expect("valid key"), &json).await?;
    let n = name.clone();
    inner
        .store(move |s| s.cache_key_file(hlc, &n, &json))
        .await?;
    Ok(name)
}

/// Sets up encryption for a new database: a DEK, its key check, and the first key file
/// (uploaded before the manifest).
pub(crate) async fn create(inner: &Arc<Inner>, db_id: DbId) -> Result<Option<EncryptionInfo>> {
    if !inner.encrypt {
        return Ok(None);
    }
    let password = match inner
        .secret
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        Some(Secret::Password(p)) => p,
        _ => {
            return Err(Error::invalid_input(
                "an encrypted database needs a password",
            ));
        }
    };
    let dek = DataKey::random()?;
    let check = crypto::key_check(&dek, &db_id, None)?;
    write_key_file(inner, db_id, &dek, &password).await?;
    let c = check.clone();
    inner.store(move |s| s.set_key_check(&c)).await?;
    inner.set_key(Some(dek));
    Ok(Some(EncryptionInfo { key_check: check }))
}

/// Unlocks a database being joined, from the `Config` secret.
pub(crate) async fn join(inner: &Arc<Inner>, db_id: DbId, info: &EncryptionInfo) -> Result<()> {
    let check = info.key_check.clone();
    inner.store(move |s| s.set_key_check(&check)).await?;
    refresh(inner, db_id).await?;
    if !unlock_with_secret(inner, db_id).await? {
        return Err(key_required());
    }
    Ok(())
}

/// The key for this sync: `None` for an unencrypted database, an error while locked.
pub(crate) async fn required_key(inner: &Arc<Inner>) -> Result<Option<DataKey>> {
    if inner.store(|s| s.key_check()).await?.is_none() {
        return Ok(None);
    }
    inner.key().map(Some).ok_or_else(key_required)
}

/// Sees new key files in a pull: caches the newest and, when another device wrote it, deletes
/// this device's older key files (their passwords no longer unlock anything).
pub(crate) async fn on_entries(
    inner: &Arc<Inner>,
    db_id: DbId,
    entries: &[RemoteEntry],
) -> Result<bool> {
    let seen = entries.iter().any(|e| {
        LogicalKey::parse(&e.key)
            .is_some_and(|k| k.db_id == db_id && matches!(k.kind, KeyKind::KeyFile(..)))
    });
    if !seen || inner.store(|s| s.key_check()).await?.is_none() {
        return Ok(false);
    }
    let before = inner
        .store(|s| s.cached_key_file())
        .await?
        .map(|(h, _, _)| h);
    let owner = refresh(inner, db_id).await?;
    let after = inner
        .store(|s| s.cached_key_file())
        .await?
        .map(|(h, _, _)| h);
    let me = inner.device();
    if let (Some(owner), Some(newest)) = (owner, after)
        && owner != me
    {
        delete_own_older(inner, db_id, newest).await?;
    }
    Ok(before.is_some() && after != before)
}

async fn delete_own_older(inner: &Arc<Inner>, db_id: DbId, newest: Hlc) -> Result<()> {
    let me = inner.device();
    for e in inner.remote.list(&format!("{db_id}/keys/{me}/")).await? {
        if let Some(LogicalKey {
            kind: KeyKind::KeyFile(_, h),
            ..
        }) = LogicalKey::parse(&e.key)
            && h < newest
        {
            inner.remote.delete(&e.key).await?;
        }
    }
    Ok(())
}

/// Re-wraps the data key under a new password. The old password must unlock the newest key
/// file; afterwards it no longer unlocks anything.
pub(crate) async fn change_password(
    inner: &Arc<Inner>,
    db_id: DbId,
    old: &str,
    new: &str,
) -> Result<()> {
    refresh(inner, db_id).await?;
    unlock_password(inner, db_id, old).await?;
    let dek = inner.key().ok_or_else(key_required)?;
    let name = write_key_file(inner, db_id, &dek, new).await?;
    let newest = match LogicalKey::parse(&name).map(|k| k.kind) {
        Some(KeyKind::KeyFile(_, h)) => h,
        _ => unreachable!("write_key_file returns a key file name"),
    };
    *inner.secret.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Secret::Password(zeroize::Zeroizing::new(new.to_owned())));
    delete_own_older(inner, db_id, newest).await
}

/// Seals `device.json` (and other JSON files) when the database is encrypted.
pub(crate) fn seal_json(
    key: Option<&DataKey>,
    logical_key: &str,
    json: Vec<u8>,
) -> Result<Vec<u8>> {
    match key {
        Some(k) => crypto::seal_json(k, logical_key, &json),
        None => Ok(json),
    }
}
