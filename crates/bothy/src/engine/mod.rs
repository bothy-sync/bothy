//! Sync engine: creating and joining databases, push and pull.

mod maintenance;
mod scheduler;

use std::sync::Arc;

use crate::codec::keys::{KeyKind, LogicalKey};
use crate::codec::manifest::{Compression, DeviceInfo, Manifest};
use crate::codec::segment;
use crate::database::{Inner, Origin, SyncReport};
use crate::error::{Error, ErrorKind, Result};
use crate::model::{DbId, DeviceId};
use crate::remote::{PutOutcome, RemoteEntry};

pub use maintenance::CompactReport;
pub(crate) use maintenance::{compact, create_snapshot};
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
    let device = inner.device();
    let info = DeviceInfo {
        device_id: device,
        name: inner.device_name.clone(),
        platform: Some(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
        bothy_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        created_hlc,
    };
    put_once(inner, &LogicalKey::device(db_id, device), &info.to_json()).await
}

fn already_attached(db: DbId) -> Error {
    Error::invalid_input(format!("this device is already attached to database {db}"))
}

async fn attach(inner: &Arc<Inner>, manifest: &Manifest) -> Result<()> {
    let compress = manifest.compression == Compression::Zstd;
    let (grace, db_id) = (manifest.grace_period_days, manifest.db_id);
    inner
        .store(move |s| {
            s.set_compress(compress)?;
            s.set_grace_period_days(grace)?;
            s.set_db_id(db_id)
        })
        .await
}

pub(crate) async fn init(inner: &Arc<Inner>) -> Result<DbId> {
    let _sync = inner.sync_lock.lock().await;
    if let Some(db) = inner.store(|s| s.db_id()).await? {
        return Err(already_attached(db));
    }
    let db_id = inner.store(|s| s.new_db_id()).await?;
    let created_hlc = inner.store(|s| s.tick()).await?;
    let mut manifest = Manifest::new(db_id, inner.device(), created_hlc, &inner.app_id)?;
    manifest.grace_period_days = inner.policy.grace_period_days;
    put_once(inner, &LogicalKey::manifest(db_id), &manifest.to_json()).await?;
    upload_device_info(inner, db_id).await?;
    attach(inner, &manifest).await?;
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

pub(crate) async fn join(inner: &Arc<Inner>, db_id: DbId) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    {
        let _sync = inner.sync_lock.lock().await;
        match inner.store(|s| s.db_id()).await? {
            Some(db) if db == db_id => {}
            Some(db) => return Err(already_attached(db)),
            None => {
                let manifest = fetch_manifest(inner, db_id).await?;
                if manifest.app_id != inner.app_id {
                    return Err(Error::invalid_input(format!(
                        "database {db_id} belongs to app {}, not {}",
                        manifest.app_id, inner.app_id
                    )));
                }
                upload_device_info(inner, db_id).await?;
                attach(inner, &manifest).await?;
                maintenance::join_from_snapshot(inner, db_id, &mut report).await?;
            }
        }
    }
    let rest = sync(inner).await?;
    report.absorb(rest);
    Ok(report)
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

    let mut forks = 0;
    'retry: loop {
        let device = inner.device();
        for p in inner.store(|s| s.pending_uploads()).await? {
            let key = LogicalKey::segment(db_id, device, p.seq).to_string();
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
            let (seq, size) = (p.seq, p.bytes.len());
            match inner
                .remote
                .put_new(&key, &p.bytes, reservation.as_deref())
                .await
            {
                Ok(PutOutcome::Created | PutOutcome::AlreadyExists { same_content: true }) => {
                    inner
                        .store(move |s| {
                            s.finish_upload(seq)?;
                            s.count_segment(size)
                        })
                        .await?;
                    report.segments_up += 1;
                }
                Ok(PutOutcome::AlreadyExists {
                    same_content: false,
                }) => {
                    // Another install uses this device id (e.g. a restored copy): fork.
                    forks += 1;
                    if forks > 3 {
                        return Err(Error::new(
                            ErrorKind::RemoteInconsistent,
                            format!("{key} keeps colliding after taking new device ids"),
                        ));
                    }
                    fork(inner, db_id, report).await?;
                    let packed = inner.store(move |s| s.pack_outbox(db_id, limits)).await?;
                    report.pushed_ops += packed.ops;
                    continue 'retry;
                }
                Err(e) => {
                    inner.store(move |s| s.record_attempt(seq)).await?;
                    return Err(e);
                }
            }
        }
        return Ok(());
    }
}

/// Takes a new device id after a collision with another install using ours.
async fn fork(inner: &Arc<Inner>, db_id: DbId, report: &mut SyncReport) -> Result<()> {
    let new = inner.store(|s| s.fork_device()).await?;
    inner.set_device(new);
    report.forked_to = Some(new);
    upload_device_info(inner, db_id).await
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

/// Remote files new since the saved cursor (or all files, without a change feed).
struct Fetched {
    entries: Vec<RemoteEntry>,
    next_cursor: Option<String>,
}

/// How often a change feed is backed up by a full listing, in case it missed a file.
const FULL_LIST_EVERY_MS: u64 = 24 * 60 * 60 * 1000;

async fn fetch(inner: &Arc<Inner>, db_id: DbId) -> Result<Fetched> {
    let cursor = inner.store(|s| s.change_cursor()).await?;
    let prefix = format!("{db_id}/");
    Ok(match inner.remote.changes_since(cursor.as_deref()).await? {
        Some(mut batch) => {
            let (now, last) = inner
                .store(|s| Ok((s.wall_ms(), s.last_full_list_ms()?)))
                .await?;
            if cursor.is_some() && now.saturating_sub(last) >= FULL_LIST_EVERY_MS {
                batch.changed.extend(inner.remote.list(&prefix).await?);
                inner.store(move |s| s.set_last_full_list_ms(now)).await?;
            }
            Fetched {
                entries: batch.changed,
                next_cursor: Some(batch.cursor),
            }
        }
        None => Fetched {
            entries: inner.remote.list(&prefix).await?,
            next_cursor: None,
        },
    })
}

/// Detects another install writing with our device id (e.g. a restored copy), before we
/// push: a segment with a seq we never used, or a snapshot we never started. Either means
/// our keys may collide or be reused after compaction, so we fork first.
async fn check_fork(
    inner: &Arc<Inner>,
    db_id: DbId,
    entries: &[RemoteEntry],
    report: &mut SyncReport,
) -> Result<()> {
    let me = inner.device();
    let next_seq = inner.store(|s| s.next_seq()).await?;
    let mut foreign = false;
    for e in entries {
        let Some(k) = LogicalKey::parse(&e.key) else {
            continue;
        };
        if k.db_id != db_id {
            continue;
        }
        match k.kind {
            KeyKind::Segment(d, seq) if d == me && seq >= next_seq => foreign = true,
            KeyKind::Snapshot(d, hlc) if d == me => {
                if inner.store(move |s| s.knows_own_snapshot(hlc)).await? {
                    // Ours; confirms an upload whose response was lost.
                    inner.store(move |s| s.confirm_own_snapshot(hlc)).await?;
                } else {
                    foreign = true;
                }
            }
            _ => {}
        }
    }
    if foreign {
        fork(inner, db_id, report).await?;
    }
    Ok(())
}

async fn pull(
    inner: &Arc<Inner>,
    db_id: DbId,
    fetched: Fetched,
    report: &mut SyncReport,
) -> Result<()> {
    let Fetched {
        entries,
        next_cursor,
    } = fetched;
    for (device, seq, key) in remote_segments(db_id, inner.device(), &entries) {
        if inner.store(move |s| s.is_applied(device, seq)).await? {
            continue;
        }
        let Some(bytes) = inner.remote.get(&key).await? else {
            // Listed but gone: compacted. Gap handling below picks it up from a snapshot.
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
        let (n_ops, size) = (seg.ops.len(), bytes.len());
        let changed = inner
            .store(move |s| {
                let changed = s.apply_remote(device, seq, &seg.ops)?;
                if changed.is_some() {
                    s.count_segment(size)?;
                }
                Ok(changed)
            })
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
    maintenance::fill_gaps(inner, db_id, report).await
}

pub(crate) async fn sync(inner: &Arc<Inner>) -> Result<SyncReport> {
    let _sync = inner.sync_lock.lock().await;
    let db_id = inner.store(|s| s.db_id()).await?.ok_or_else(|| {
        Error::invalid_input("database is not initialised; call init or join first")
    })?;
    let mut report = SyncReport::default();
    let fetched = fetch(inner, db_id).await?;
    check_fork(inner, db_id, &fetched.entries, &mut report).await?;
    match push(inner, db_id, &mut report).await {
        Ok(()) => {}
        // A full remote stops pushing but not pulling.
        Err(e) if e.kind() == ErrorKind::QuotaExceeded => {
            pull(inner, db_id, fetched, &mut report).await?;
            return Err(e);
        }
        Err(e) => return Err(e),
    }
    pull(inner, db_id, fetched, &mut report).await?;
    maintenance::auto(inner, db_id, &mut report).await?;
    Ok(report)
}

pub(crate) use scheduler::start as start_auto_sync;
