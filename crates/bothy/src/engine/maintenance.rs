//! Snapshots, joining from a snapshot, gap repair and compaction of a device's own files.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::Hlc;
use crate::codec::keys::{KeyKind, LogicalKey};
use crate::codec::snapshot::{self, SnapshotReader, VersionVector};
use crate::database::{Inner, Origin, SyncReport};
use crate::error::{Error, ErrorKind, Result};
use crate::model::{DbId, DeviceId};
use crate::remote::PutOutcome;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Files a compaction run deleted (or would delete, for a dry run).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactReport {
    /// Own segment keys.
    pub segments: Vec<String>,
    /// Own snapshot keys.
    pub snapshots: Vec<String>,
}

/// Builds a snapshot of the local state and uploads it. Returns its key.
pub(crate) async fn create_snapshot(inner: &Arc<Inner>, db_id: DbId) -> Result<String> {
    for _ in 0..3 {
        let device = inner.device();
        let key = super::sync_key(inner).await?;
        let (hlc, vv, bytes) = inner
            .store(move |s| {
                let hlc = s.tick()?;
                let compress = s.compress()?;
                let (vv, bytes) = s.build_snapshot(db_id, hlc, compress, key.as_ref())?;
                Ok((hlc, vv, bytes))
            })
            .await?;
        let key = LogicalKey::snapshot(db_id, device, hlc).to_string();
        inner.store(move |s| s.begin_own_snapshot(hlc, &vv)).await?;
        match inner.remote.put_new(&key, &bytes, None).await? {
            PutOutcome::Created | PutOutcome::AlreadyExists { same_content: true } => {
                inner.store(move |s| s.confirm_own_snapshot(hlc)).await?;
                return Ok(key);
            }
            // Another install with our id wrote this key: fork and snapshot under the new id.
            PutOutcome::AlreadyExists {
                same_content: false,
            } => {
                inner.store(move |s| s.abandon_own_snapshot(hlc)).await?;
                super::fork(inner, db_id, &mut SyncReport::default()).await?;
            }
        }
    }
    Err(Error::new(
        ErrorKind::RemoteInconsistent,
        "snapshot keys keep colliding",
    ))
}

/// A remote snapshot: `(key, device, hlc)`.
type Candidate = (String, DeviceId, Hlc);

/// The newest snapshot of every device.
async fn newest_snapshots(inner: &Inner, db_id: DbId) -> Result<Vec<Candidate>> {
    let mut newest: BTreeMap<DeviceId, (Hlc, String)> = BTreeMap::new();
    for e in inner.remote.list(&format!("{db_id}/snapshots/")).await? {
        let Some(key) = LogicalKey::parse(&e.key) else {
            continue;
        };
        if let KeyKind::Snapshot(dev, hlc) = key.kind
            && key.db_id == db_id
            && newest.get(&dev).is_none_or(|(h, _)| hlc > *h)
        {
            newest.insert(dev, (hlc, e.key));
        }
    }
    Ok(newest.into_iter().map(|(d, (h, k))| (k, d, h)).collect())
}

/// Downloads a snapshot and reads its version vector. `None` if gone or invalid.
async fn fetch(
    inner: &Arc<Inner>,
    db_id: DbId,
    (key, device, hlc): &Candidate,
    report: &mut SyncReport,
) -> Result<Option<(Vec<u8>, VersionVector)>> {
    let Some(bytes) = inner.remote.get(key).await? else {
        return Ok(None);
    };
    report.snapshots_down += 1;
    let expected = snapshot::Expected {
        db_id,
        device_id: *device,
        hlc: *hlc,
    };
    let data_key = super::sync_key(inner).await?;
    match SnapshotReader::open_with(&bytes, &expected, data_key.as_ref()) {
        Ok(r) => {
            let vv = r.header().vv.clone();
            Ok(Some((bytes, vv)))
        }
        Err(e) if e.kind() == ErrorKind::Corrupt => {
            report.skipped.push(format!("{key}: {}", e.message()));
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

async fn apply(
    inner: &Arc<Inner>,
    db_id: DbId,
    key: String,
    device: DeviceId,
    hlc: Hlc,
    bytes: Vec<u8>,
    report: &mut SyncReport,
) -> Result<()> {
    let data_key = super::sync_key(inner).await?;
    let changed = inner
        .store(move |s| {
            let expected = snapshot::Expected {
                db_id,
                device_id: device,
                hlc,
            };
            let reader = SnapshotReader::open_with(&bytes, &expected, data_key.as_ref())?;
            s.apply_snapshot(&key, &reader)
        })
        .await?;
    if let Some(changed) = changed {
        report.snapshots_applied += 1;
        report.changed_docs += changed.len();
        for (c, d) in changed {
            inner.emit(c, d, Origin::Remote);
        }
    }
    Ok(())
}

/// A new device starts from the most complete snapshot instead of every segment.
pub(crate) async fn join_from_snapshot(
    inner: &Arc<Inner>,
    db_id: DbId,
    report: &mut SyncReport,
) -> Result<()> {
    let mut best: Option<((u64, Hlc), Candidate, Vec<u8>)> = None;
    for cand in newest_snapshots(inner, db_id).await? {
        if let Some((bytes, vv)) = fetch(inner, db_id, &cand, report).await? {
            let rank = (vv.values().sum::<u64>(), cand.2);
            if best.as_ref().is_none_or(|(r, _, _)| rank > *r) {
                best = Some((rank, cand, bytes));
            }
        }
    }
    if let Some((_, (key, device, hlc), bytes)) = best {
        apply(inner, db_id, key, device, hlc, bytes, report).await?;
    }
    Ok(())
}

/// Repairs gaps (a device's next segment is gone while later ones exist) from a snapshot that
/// covers them. Gaps that no snapshot covers yet are reported.
pub(crate) async fn fill_gaps(
    inner: &Arc<Inner>,
    db_id: DbId,
    report: &mut SyncReport,
) -> Result<()> {
    let gaps = inner.store(|s| s.gaps()).await?;
    if gaps.is_empty() {
        return Ok(());
    }
    for cand in newest_snapshots(inner, db_id).await? {
        let key = cand.0.clone();
        if cand.1 == inner.device() || inner.store(move |s| s.is_snapshot_applied(&key)).await? {
            continue;
        }
        let Some((bytes, vv)) = fetch(inner, db_id, &cand, report).await? else {
            continue;
        };
        if gaps
            .iter()
            .any(|(d, need)| vv.get(d).is_some_and(|s| s >= need))
        {
            let (key, device, hlc) = cand;
            apply(inner, db_id, key, device, hlc, bytes, report).await?;
        }
    }
    report.gaps = inner.store(|s| s.gaps()).await?;
    Ok(())
}

/// Snapshot when enough has accumulated, then compact.
pub(crate) async fn auto(inner: &Arc<Inner>, db_id: DbId, report: &mut SyncReport) -> Result<()> {
    let since = inner.store(|s| s.since_snapshot()).await?;
    if since.segments >= inner.policy.every_segments || since.bytes >= inner.policy.every_bytes {
        create_snapshot(inner, db_id).await?;
        report.snapshots_up += 1;
    }
    let done = compact(inner, db_id, false).await?;
    report.deleted_files += done.segments.len() + done.snapshots.len();
    Ok(())
}

/// Deletes the segments of another device that is gone for good, after uploading a snapshot
/// that covers them. Its newest segment stays, as for compaction. Needs the user's consent.
pub(crate) async fn prune_device(
    inner: &Arc<Inner>,
    db_id: DbId,
    target: DeviceId,
) -> Result<Vec<String>> {
    if target == inner.device() {
        return Err(Error::invalid_input(
            "cannot prune this device; use compact",
        ));
    }
    let watermark = inner.store(move |s| s.watermark(target)).await?;
    if watermark == 0 {
        return Ok(Vec::new());
    }
    // Our snapshot's vv covers the target up to its watermark.
    create_snapshot(inner, db_id).await?;
    let mut seqs: Vec<(u64, String)> = inner
        .remote
        .list(&format!("{db_id}/devices/{target}/seg-"))
        .await?
        .into_iter()
        .filter_map(|e| match LogicalKey::parse(&e.key)?.kind {
            KeyKind::Segment(_, seq) => Some((seq, e.key)),
            _ => None,
        })
        .collect();
    seqs.sort();
    let newest = seqs.last().map(|(s, _)| *s);
    let mut deleted = Vec::new();
    for (seq, key) in seqs {
        if seq <= watermark && Some(seq) != newest {
            inner.remote.delete(&key).await?;
            deleted.push(key);
        }
    }
    Ok(deleted)
}

/// Deletes this device's segments covered by an own snapshot older than the grace period, and
/// own snapshots superseded by such a snapshot. The newest own snapshot always stays.
pub(crate) async fn compact(
    inner: &Arc<Inner>,
    db_id: DbId,
    dry_run: bool,
) -> Result<CompactReport> {
    let me = inner.device();
    let (own, now, grace_days, done_upto, uploaded) = inner
        .store(|s| {
            Ok((
                s.own_snapshots()?,
                s.wall_ms(),
                s.grace_period_days()?,
                s.compacted_upto()?,
                s.uploaded_watermark()?,
            ))
        })
        .await?;
    let grace = u64::from(grace_days) * DAY_MS;
    let old_enough = |h: &Hlc| h.physical_ms().saturating_add(grace) <= now;
    let mut report = CompactReport::default();

    let covered = own
        .iter()
        .filter(|(h, _)| old_enough(h))
        .filter_map(|(_, vv)| vv.get(&me).copied())
        .max()
        .unwrap_or(0)
        // Keep the newest own segment: a device that fell behind sees it, notices the gap below
        // it and repairs from a snapshot. Without it, a fully compacted device looks idle.
        .min(uploaded.saturating_sub(1));
    if covered > done_upto {
        // Own segment seqs are known (1..=n), so delete by key instead of trusting a listing
        // that may lag behind. Deleting a missing file succeeds.
        for seq in done_upto + 1..=covered {
            let key = LogicalKey::segment(db_id, me, seq).to_string();
            if !dry_run {
                inner.remote.delete(&key).await?;
            }
            report.segments.push(key);
        }
        if !dry_run {
            inner
                .store(move |s| {
                    s.prune_outbox(covered)?;
                    s.set_compacted_upto(covered)
                })
                .await?;
        }
    }

    let dominates = |a: &VersionVector, b: &VersionVector| {
        b.iter().all(|(d, s)| a.get(d).is_some_and(|x| x >= s))
    };
    for (i, (h, vv)) in own.iter().enumerate() {
        let superseded = own[i + 1..]
            .iter()
            .any(|(nh, nvv)| old_enough(nh) && dominates(nvv, vv));
        if superseded {
            let key = LogicalKey::snapshot(db_id, me, *h).to_string();
            if !dry_run {
                inner.remote.delete(&key).await?;
                let h = *h;
                inner.store(move |s| s.forget_own_snapshot(h)).await?;
            }
            report.snapshots.push(key);
        }
    }
    Ok(report)
}
