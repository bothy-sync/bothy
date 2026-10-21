//! Snapshots, compaction bookkeeping and device-id forks.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{
    Store, advance_watermark, device_from_sql, get_meta, hlc_from_sql, hlc_to_sql, load_doc,
    log_conflicts, save_doc, set_meta, visible_doc,
};
use crate::Hlc;
use crate::codec::DataKey;
use crate::codec::cbor::{Decoder, Encoder};
use crate::codec::snapshot::{SnapshotHeader, SnapshotReader, SnapshotWriter, VersionVector};
use crate::codec::{decode_op, encode_op};
use crate::error::{Error, Result};
use crate::merge::DocState;
use crate::model::{CollectionName, DbId, DeviceId, DocId, Op, OpKind, Stamp};

fn encode_vv(vv: &VersionVector) -> Vec<u8> {
    let mut e = Encoder::new();
    e.map(vv.len());
    for (d, s) in vv {
        e.bytes(d.as_bytes());
        e.uint(*s);
    }
    e.into_bytes()
}

fn decode_vv(b: &[u8]) -> Result<VersionVector> {
    let mut d = Decoder::new(b);
    let n = d.map()?;
    let mut vv = BTreeMap::new();
    let mut prev = None;
    for _ in 0..n {
        let (k, enc) = d.map_bytes_key(prev)?;
        prev = Some(enc);
        let k: [u8; 16] = k.try_into().map_err(|_| Error::corrupt("stored vv key"))?;
        vv.insert(DeviceId::from_bytes(k), d.uint()?);
    }
    d.finish()?;
    Ok(vv)
}

/// Segments and bytes seen since this device's last snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinceSnapshot {
    /// Segments uploaded or applied.
    pub segments: u64,
    /// Their total size.
    pub bytes: u64,
}

impl Store {
    /// Highest seq of this device's segments that are all uploaded (1..=n).
    pub fn uploaded_watermark(&self) -> Result<u64> {
        let pending: Option<i64> =
            self.conn
                .query_row("SELECT MIN(seq) FROM pending_uploads", [], |r| r.get(0))?;
        Ok(match pending {
            Some(p) => p as u64 - 1,
            None => self.next_seq()? - 1,
        })
    }

    /// Version vector a snapshot of the current state may claim: the gap-free watermark of
    /// every known device, and this device's uploaded watermark.
    pub fn snapshot_vv(&self) -> Result<VersionVector> {
        let mut vv = BTreeMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT device_id, watermark FROM devices WHERE watermark > 0")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (d, w) = row?;
            vv.insert(device_from_sql(d)?, w as u64);
        }
        let own = self.uploaded_watermark()?;
        if own > 0 {
            vv.insert(self.device, own);
        }
        Ok(vv)
    }

    /// Builds a snapshot of the whole merge state with the given HLC.
    pub fn build_snapshot(
        &self,
        db_id: DbId,
        hlc: Hlc,
        compress: bool,
        key: Option<&DataKey>,
    ) -> Result<(VersionVector, Vec<u8>)> {
        let vv = self.snapshot_vv()?;
        let header = SnapshotHeader {
            db_id,
            device_id: self.device,
            hlc,
            vv: vv.clone(),
        };
        let w = SnapshotWriter::new(header, compress);
        let mut w = match key {
            None => w,
            #[cfg(feature = "crypto")]
            Some(k) => w.encrypted(k.clone()),
            #[cfg(not(feature = "crypto"))]
            Some(k) => match *k {},
        };
        let mut stmt = self.conn.prepare(
            "SELECT collection, doc_id FROM fields
             UNION SELECT collection, doc_id FROM doc_tombstones
             ORDER BY 1, 2",
        )?;
        let keys: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (c, d) in keys {
            let c = CollectionName::new(&c).map_err(|_| Error::corrupt("bad stored collection"))?;
            let d = DocId::new(&d).map_err(|_| Error::corrupt("bad stored document id"))?;
            w.push(&c, &d, &load_doc(&self.conn, &c, &d)?)?;
        }
        Ok((vv, w.finish()?))
    }

    /// Records an own snapshot before its upload starts, so a later listing that shows it is
    /// recognised as ours even if the upload response was lost.
    pub fn begin_own_snapshot(&mut self, hlc: Hlc, vv: &VersionVector) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO own_snapshots (hlc, vv, state) VALUES (?1, ?2, 0)",
            params![hlc_to_sql(hlc), encode_vv(vv)],
        )?;
        Ok(())
    }

    /// Marks an own snapshot as present on the remote and resets the counters.
    pub fn confirm_own_snapshot(&mut self, hlc: Hlc) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE own_snapshots SET state = 1 WHERE hlc = ?1 AND state = 0",
            [hlc_to_sql(hlc)],
        )?;
        set_meta(&tx, "since_snapshot_segments", 0i64)?;
        set_meta(&tx, "since_snapshot_bytes", 0i64)?;
        tx.commit()?;
        Ok(())
    }

    /// Drops the record of an own snapshot whose key turned out to belong to another install.
    pub fn abandon_own_snapshot(&mut self, hlc: Hlc) -> Result<()> {
        self.conn.execute(
            "DELETE FROM own_snapshots WHERE hlc = ?1 AND state = 0",
            [hlc_to_sql(hlc)],
        )?;
        Ok(())
    }

    /// Whether this device ever started an own snapshot with this HLC (any state).
    pub fn knows_own_snapshot(&self, hlc: Hlc) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM own_snapshots WHERE hlc = ?1)",
            [hlc_to_sql(hlc)],
            |r| r.get(0),
        )?)
    }

    /// This device's snapshots confirmed on the remote, oldest first.
    pub fn own_snapshots(&self) -> Result<Vec<(Hlc, VersionVector)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT hlc, vv FROM own_snapshots WHERE state = 1 ORDER BY hlc")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        rows.map(|row| {
            let (h, vv) = row?;
            Ok((hlc_from_sql(h), decode_vv(&vv)?))
        })
        .collect()
    }

    /// Marks an own snapshot as deleted. The row stays so a lagging listing is not mistaken for
    /// another install's file.
    pub fn forget_own_snapshot(&mut self, hlc: Hlc) -> Result<()> {
        self.conn.execute(
            "UPDATE own_snapshots SET state = 2 WHERE hlc = ?1",
            [hlc_to_sql(hlc)],
        )?;
        Ok(())
    }

    /// Counts segments uploaded or applied since the last own snapshot.
    pub fn count_segment(&mut self, bytes: usize) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('since_snapshot_segments', 1)
             ON CONFLICT (key) DO UPDATE SET value = value + 1",
            [],
        )?;
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('since_snapshot_bytes', ?1)
             ON CONFLICT (key) DO UPDATE SET value = value + excluded.value",
            [bytes as i64],
        )?;
        Ok(())
    }

    /// Segments and bytes since the last own snapshot.
    pub fn since_snapshot(&self) -> Result<SinceSnapshot> {
        let get = |k: &str| -> Result<u64> {
            let v: Option<i64> = self
                .conn
                .query_row("SELECT value FROM meta WHERE key = ?1", [k], |r| r.get(0))
                .optional()?;
            Ok(v.unwrap_or(0) as u64)
        };
        Ok(SinceSnapshot {
            segments: get("since_snapshot_segments")?,
            bytes: get("since_snapshot_bytes")?,
        })
    }

    /// Drops outbox rows of own segments `1..=seq` (after those segments were deleted remotely).
    pub fn prune_outbox(&mut self, seq: u64) -> Result<()> {
        self.conn
            .execute("DELETE FROM outbox WHERE segment_seq <= ?1", [seq as i64])?;
        Ok(())
    }

    /// Devices whose next segment is missing although later ones were applied:
    /// `(device, watermark + 1)`.
    pub fn gaps(&self) -> Result<Vec<(DeviceId, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.device_id, d.watermark FROM devices d
             WHERE EXISTS (SELECT 1 FROM applied_segments a
                           WHERE a.device_id = d.device_id AND a.seq > d.watermark + 1)",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?;
        rows.map(|row| {
            let (d, w) = row?;
            Ok((device_from_sql(d)?, w as u64 + 1))
        })
        .collect()
    }

    /// Whether the snapshot with this logical key was already merged.
    pub fn is_snapshot_applied(&self, key: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM applied_snapshots WHERE key = ?1)",
            [key],
            |r| r.get(0),
        )?)
    }

    /// Merges a snapshot into the local state in one transaction and raises the watermarks of
    /// the devices it covers. Returns `None` if it was already applied, otherwise the documents
    /// whose visible content changed.
    pub fn apply_snapshot(
        &mut self,
        key: &str,
        snapshot: &SnapshotReader<'_>,
    ) -> Result<Option<Vec<(CollectionName, DocId)>>> {
        if self.is_snapshot_applied(key)? {
            return Ok(None);
        }
        let me = self.device;
        let wall = self.clock.now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut changed = Vec::new();
        let mut max_hlc = Hlc::ZERO;
        snapshot.for_each_entry(|c, d, incoming| {
            if let Some(s) = incoming.max_stamp() {
                max_hlc = max_hlc.max(s.hlc);
            }
            let before = load_doc(&tx, &c, &d)?;
            let mut after = before.clone();
            let lost = after.merge(&incoming);
            log_conflicts(&tx, &c, &d, &lost, wall)?;
            if visible_doc(&before) != visible_doc(&after) {
                changed.push((c.clone(), d.clone()));
            }
            save_doc(&tx, &c, &d, &before, &after)
        })?;
        for (device, seq) in &snapshot.header().vv {
            if *device == me {
                continue;
            }
            tx.execute(
                "INSERT INTO devices (device_id, watermark) VALUES (?1, ?2)
                 ON CONFLICT (device_id) DO UPDATE SET watermark = MAX(watermark, excluded.watermark)",
                params![device.as_bytes().as_slice(), *seq as i64],
            )?;
            advance_watermark(&tx, *device, Hlc::ZERO)?;
        }
        tx.execute("INSERT INTO applied_snapshots (key) VALUES (?1)", [key])?;
        if max_hlc > Hlc::ZERO {
            self.hlc.observe(max_hlc, wall);
        }
        set_meta(&tx, "hlc_last", hlc_to_sql(self.hlc.last()))?;
        tx.commit()?;
        Ok(Some(changed))
    }

    /// Handles a device-id fork (this database file also runs elsewhere): takes a new device id
    /// and re-issues the unsent ops that still win under the new id. Returns the new id.
    pub fn fork_device(&mut self) -> Result<DeviceId> {
        let old = self.device;
        let new = self.new_fork_id()?;
        let uploaded = self.uploaded_watermark()?;
        let wall = self.clock.now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unsent: Vec<Vec<u8>> = {
            let mut stmt = tx.prepare(
                "SELECT op FROM outbox
                 WHERE segment_seq IS NULL OR segment_seq IN (SELECT seq FROM pending_uploads)
                 ORDER BY local_seq",
            )?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        tx.execute_batch(
            "DELETE FROM pending_uploads; DELETE FROM outbox; DELETE FROM own_snapshots;",
        )?;
        // Our segments under the old id that reached the remote are already in the state.
        tx.execute(
            "INSERT INTO devices (device_id, watermark) VALUES (?1, ?2)
             ON CONFLICT (device_id) DO UPDATE SET watermark = MAX(watermark, excluded.watermark)",
            params![old.as_bytes().as_slice(), uploaded as i64],
        )?;
        set_meta(&tx, "device_id", new.as_bytes().as_slice())?;
        set_meta(&tx, "next_seq", 1i64)?;
        set_meta(&tx, "since_snapshot_segments", 0i64)?;
        set_meta(&tx, "since_snapshot_bytes", 0i64)?;
        set_meta(&tx, "compacted_upto", 0i64)?;

        for bytes in unsent {
            let mut d = Decoder::new(&bytes);
            let op = decode_op(&mut d)?;
            let old_stamp = Stamp::new(op.hlc, old);
            let before = load_doc(&tx, &op.collection, &op.doc_id)?;
            // Keep only the parts of the op that still win.
            let kind = match &op.kind {
                OpKind::SetFields(f) => {
                    let still: BTreeMap<_, _> = f
                        .iter()
                        .filter(|(k, _)| {
                            before.fields.get(*k).is_some_and(|s| s.stamp == old_stamp)
                        })
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    (!still.is_empty()).then_some(OpKind::SetFields(still))
                }
                OpKind::UnsetFields(f) => {
                    let still: std::collections::BTreeSet<_> = f
                        .iter()
                        .filter(|k| before.fields.get(*k).is_some_and(|s| s.stamp == old_stamp))
                        .cloned()
                        .collect();
                    (!still.is_empty()).then_some(OpKind::UnsetFields(still))
                }
                OpKind::DeleteDoc => {
                    (before.deleted_at == Some(old_stamp)).then_some(OpKind::DeleteDoc)
                }
            };
            let Some(kind) = kind else { continue };
            let hlc = self.hlc.now(wall);
            let mut after: DocState = before.clone();
            after.apply(&kind, Stamp::new(hlc, new));
            let reissued = Op {
                hlc,
                collection: op.collection,
                doc_id: op.doc_id,
                kind,
            };
            let mut e = Encoder::new();
            encode_op(&mut e, &reissued)?;
            tx.execute(
                "INSERT INTO outbox (hlc, op) VALUES (?1, ?2)",
                params![hlc_to_sql(hlc), e.into_bytes()],
            )?;
            save_doc(&tx, &reissued.collection, &reissued.doc_id, &before, &after)?;
        }
        set_meta(&tx, "hlc_last", hlc_to_sql(self.hlc.last()))?;
        tx.commit()?;
        self.device = new;
        Ok(new)
    }

    /// Current wall-clock time of the store's clock.
    pub fn wall_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Highest own segment seq already deleted by compaction.
    pub fn compacted_upto(&self) -> Result<u64> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'compacted_upto'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.unwrap_or(0) as u64)
    }

    /// Records how far compaction got.
    pub fn set_compacted_upto(&mut self, seq: u64) -> Result<()> {
        set_meta(&self.conn, "compacted_upto", seq as i64)
    }

    /// Days a covered segment or superseded snapshot is kept (from the manifest; default 30).
    pub fn grace_period_days(&self) -> Result<u32> {
        Ok(get_meta(&self.conn, "grace_days")?
            .and_then(|b| std::str::from_utf8(&b).ok()?.parse().ok())
            .unwrap_or(30))
    }

    /// Stores the manifest's grace period.
    pub fn set_grace_period_days(&mut self, days: u32) -> Result<()> {
        set_meta(&self.conn, "grace_days", days.to_string().as_bytes())
    }
}
