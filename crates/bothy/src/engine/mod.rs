//! Sync engine: creating and joining databases, push and pull.

mod scheduler;

use std::sync::Arc;

use crate::codec::keys::{KeyKind, LogicalKey};
use crate::codec::manifest::{Compression, DeviceInfo, Manifest};
use crate::codec::segment;
use crate::database::{Inner, Origin, SyncReport};
use crate::error::{Error, ErrorKind, Result};
use crate::model::{DbId, DeviceId};
use crate::remote::{PutOutcome, RemoteEntry};

pub use scheduler::{AutoSync, SyncHandle, SyncStatus};

async fn put_once(inner: &Inner, key: &LogicalKey, bytes: &[u8]) -> Result<()> {
    let key = key.to_string();
    match inner.remote.put_new(&key, bytes, None).await? {
        PutOutcome::Created | PutOutcome::AlreadyExists { same_content: true } => Ok(()),
        PutOutcome::AlreadyExists {
            same_content: false,
        } => Err(Error::new(
            ErrorKind::RemoteInconsistent,
            format!("{key} already exists with different content"),
        )),
    }
}

async fn upload_device_info(inner: &Arc<Inner>, db_id: DbId) -> Result<()> {
    let created_hlc = inner.store(|s| s.tick()).await?;
    let info = DeviceInfo {
        device_id: inner.device,
        name: inner.device_name.clone(),
        platform: Some(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
        bothy_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        created_hlc,
    };
    put_once(
        inner,
        &LogicalKey::device(db_id, inner.device),
        &info.to_json(),
    )
    .await
}

fn already_attached(db: DbId) -> Error {
    Error::invalid_input(format!("this device is already attached to database {db}"))
}

pub(crate) async fn init(inner: &Arc<Inner>) -> Result<DbId> {
    let _sync = inner.sync_lock.lock().await;
    if let Some(db) = inner.store(|s| s.db_id()).await? {
        return Err(already_attached(db));
    }
    let db_id = DbId::random()?;
    let created_hlc = inner.store(|s| s.tick()).await?;
    let manifest = Manifest::new(db_id, inner.device, created_hlc, &inner.app_id)?;
    put_once(inner, &LogicalKey::manifest(db_id), &manifest.to_json()).await?;
    upload_device_info(inner, db_id).await?;
    let compress = manifest.compression == Compression::Zstd;
    inner
        .store(move |s| {
            s.set_compress(compress)?;
            s.set_db_id(db_id)
        })
        .await?;
    Ok(db_id)
}

async fn fetch_manifest(inner: &Inner, db_id: DbId) -> Result<Manifest> {
    let bytes = inner
        .remote
        .get(&LogicalKey::manifest(db_id).to_string())
        .await?
        .ok_or_else(|| {
            Error::new(
                ErrorKind::RemoteInconsistent,
                format!("database {db_id} has no manifest"),
            )
        })?;
    let manifest = Manifest::from_json(&bytes, Some(db_id))?;
    if manifest.encryption.is_some() {
        return Err(Error::format_too_new(
            "database is encrypted; this build has no encryption support",
        ));
    }
    Ok(manifest)
}

pub(crate) async fn join(inner: &Arc<Inner>, db_id: DbId) -> Result<()> {
    {
        let _sync = inner.sync_lock.lock().await;
        if let Some(db) = inner.store(|s| s.db_id()).await? {
            if db == db_id {
                return Ok(());
            }
            return Err(already_attached(db));
        }
        let manifest = fetch_manifest(inner, db_id).await?;
        if manifest.app_id != inner.app_id {
            return Err(Error::invalid_input(format!(
                "database {db_id} belongs to app {}, not {}",
                manifest.app_id, inner.app_id
            )));
        }
        upload_device_info(inner, db_id).await?;
        let compress = manifest.compression == Compression::Zstd;
        inner
            .store(move |s| {
                s.set_compress(compress)?;
                s.set_db_id(db_id)
            })
            .await?;
    }
    sync(inner).await.map(|_| ())
}

pub(crate) async fn init_or_join(inner: &Arc<Inner>) -> Result<DbId> {
    if let Some(db) = inner.store(|s| s.db_id()).await? {
        return Ok(db);
    }
    let mut candidates = Vec::new();
    for entry in inner.remote.list("").await? {
        let Some(key) = LogicalKey::parse(&entry.key) else {
            continue;
        };
        if key.kind != KeyKind::Manifest {
            continue;
        }
        match fetch_manifest(inner, key.db_id).await {
            Ok(m) if m.app_id == inner.app_id => candidates.push((m.created_hlc, m.db_id)),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Corrupt => {}
            Err(e) => return Err(e),
        }
    }
    match candidates.into_iter().min() {
        Some((_, db_id)) => {
            join(inner, db_id).await?;
            Ok(db_id)
        }
        None => init(inner).await,
    }
}

async fn push(inner: &Arc<Inner>, db_id: DbId, report: &mut SyncReport) -> Result<()> {
    let mut limits = inner.pack;
    limits.compress = inner.store(|s| s.compress()).await? && limits.compress;
    let packed = inner.store(move |s| s.pack_outbox(db_id, limits)).await?;
    report.pushed_ops += packed.ops;

    for p in inner.store(|s| s.pending_uploads()).await? {
        let key = LogicalKey::segment(db_id, inner.device, p.seq).to_string();
        let reservation = match p.reservation {
            Some(r) => Some(r),
            None => match inner.remote.reserve(&key).await? {
                Some(r) => {
                    let (seq, token) = (p.seq, r.clone());
                    inner.store(move |s| s.set_reservation(seq, &token)).await?;
                    Some(r)
                }
                None => None,
            },
        };
        let seq = p.seq;
        match inner
            .remote
            .put_new(&key, &p.bytes, reservation.as_deref())
            .await
        {
            Ok(PutOutcome::Created | PutOutcome::AlreadyExists { same_content: true }) => {
                inner.store(move |s| s.finish_upload(seq)).await?;
                report.segments_up += 1;
            }
            Ok(PutOutcome::AlreadyExists {
                same_content: false,
            }) => {
                return Err(Error::new(
                    ErrorKind::RemoteInconsistent,
                    format!("{key} was written by another install with the same device id"),
                ));
            }
            Err(e) => {
                inner.store(move |s| s.record_attempt(seq)).await?;
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Remote segments of other devices, in `(device, seq)` order.
fn remote_segments(
    db_id: DbId,
    me: DeviceId,
    entries: &[RemoteEntry],
) -> Vec<(DeviceId, u64, String)> {
    let mut out: Vec<_> = entries
        .iter()
        .filter_map(|e| {
            let key = LogicalKey::parse(&e.key)?;
            match key.kind {
                KeyKind::Segment(dev, seq) if key.db_id == db_id && dev != me => {
                    Some((dev, seq, e.key.clone()))
                }
                _ => None,
            }
        })
        .collect();
    out.sort();
    out
}

async fn pull(inner: &Arc<Inner>, db_id: DbId, report: &mut SyncReport) -> Result<()> {
    let cursor = inner.store(|s| s.change_cursor()).await?;
    let (entries, next_cursor) = match inner.remote.changes_since(cursor.as_deref()).await? {
        Some(batch) => (batch.changed, Some(batch.cursor)),
        None => (inner.remote.list(&format!("{db_id}/")).await?, None),
    };

    for (device, seq, key) in remote_segments(db_id, inner.device, &entries) {
        if inner.store(move |s| s.is_applied(device, seq)).await? {
            continue;
        }
        let Some(bytes) = inner.remote.get(&key).await? else {
            // Listed but gone: compacted or not yet visible. Gap handling picks it up later.
            continue;
        };
        let expected = segment::Expected {
            db_id,
            device_id: device,
            seq,
        };
        let seg = match segment::decode(&bytes, &expected) {
            Ok(seg) => seg,
            Err(e) if e.kind() == ErrorKind::Corrupt => {
                report.skipped.push(format!("{key}: {}", e.message()));
                continue;
            }
            Err(e) => return Err(e),
        };
        let n_ops = seg.ops.len();
        let changed = inner
            .store(move |s| s.apply_remote(device, seq, &seg.ops))
            .await?;
        if let Some(changed) = changed {
            report.segments_down += 1;
            report.pulled_ops += n_ops;
            report.changed_docs += changed.len();
            for (c, d) in changed {
                inner.emit(c, d, Origin::Remote);
            }
        }
    }

    if let Some(c) = next_cursor {
        inner.store(move |s| s.set_change_cursor(&c)).await?;
    }
    Ok(())
}

pub(crate) async fn sync(inner: &Arc<Inner>) -> Result<SyncReport> {
    let _sync = inner.sync_lock.lock().await;
    let db_id = inner.store(|s| s.db_id()).await?.ok_or_else(|| {
        Error::invalid_input("database is not initialised; call init or join first")
    })?;
    let mut report = SyncReport::default();
    match push(inner, db_id, &mut report).await {
        Ok(()) => {}
        // A full remote stops pushing but not pulling.
        Err(e) if e.kind() == ErrorKind::QuotaExceeded => {
            pull(inner, db_id, &mut report).await?;
            return Err(e);
        }
        Err(e) => return Err(e),
    }
    pull(inner, db_id, &mut report).await?;
    Ok(report)
}

pub(crate) use scheduler::start as start_auto_sync;
