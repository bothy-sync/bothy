//! Sync bookkeeping: database id, outbox packing, pending uploads and the change cursor.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{Store, get_meta, set_meta};
use crate::codec::cbor::Decoder;
use crate::codec::decode_op;
use crate::codec::segment::{self, Segment};
use crate::error::{Error, Result};
use crate::model::{DbId, DeviceId};

/// A packed segment waiting to be uploaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingUpload {
    /// Segment sequence number.
    pub seq: u64,
    /// Backend upload token stored before the first attempt (Drive file id).
    pub reservation: Option<String>,
    /// The exact bytes to upload; retries always send these.
    pub bytes: Vec<u8>,
}

/// What [`Store::pack_outbox`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Packed {
    /// Segments created.
    pub segments: usize,
    /// Ops packed into them.
    pub ops: usize,
}

/// Limits for packing the outbox into segments.
#[derive(Clone, Copy, Debug)]
pub struct PackLimits {
    /// Maximum ops per segment.
    pub max_ops: usize,
    /// Plain body size to stay under (a single larger op still gets its own segment).
    pub max_plain_bytes: usize,
    /// Whether to zstd-compress the body.
    pub compress: bool,
}

impl Default for PackLimits {
    fn default() -> Self {
        Self {
            max_ops: segment::TARGET_OPS,
            max_plain_bytes: segment::TARGET_PLAIN_LEN,
            compress: true,
        }
    }
}

impl Store {
    /// The database this store is attached to, if any.
    pub fn db_id(&self) -> Result<Option<DbId>> {
        get_meta(&self.conn, "db_id")?
            .map(|b| {
                let b: [u8; 16] = b
                    .try_into()
                    .map_err(|_| Error::corrupt("stored db id is not 16 bytes"))?;
                Ok(DbId::from_bytes(b))
            })
            .transpose()
    }

    /// Attaches the store to a database. Fails if it is already attached to another one.
    pub fn set_db_id(&mut self, db_id: DbId) -> Result<()> {
        match self.db_id()? {
            Some(existing) if existing != db_id => Err(Error::invalid_input(format!(
                "local database is already attached to {existing}"
            ))),
            Some(_) => Ok(()),
            None => set_meta(&self.conn, "db_id", db_id.as_bytes().as_slice()),
        }
    }

    /// Next segment sequence number this device will use.
    pub fn next_seq(&self) -> Result<u64> {
        let v: Option<i64> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'next_seq'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(v.unwrap_or(1) as u64)
    }

    /// Packs unpacked outbox ops into segments and queues them for upload, in one transaction.
    pub fn pack_outbox(&mut self, db_id: DbId, limits: PackLimits) -> Result<Packed> {
        let device_id = self.device;
        let mut seq = self.next_seq()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows: Vec<(i64, Vec<u8>)> = {
            let mut stmt = tx.prepare(
                "SELECT local_seq, op FROM outbox WHERE segment_seq IS NULL ORDER BY local_seq",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut groups: Vec<Vec<(i64, Vec<u8>)>> = Vec::new();
        let mut size = 0;
        for row in rows {
            let full = groups.last().is_none_or(|g| {
                g.len() >= limits.max_ops || size + row.1.len() > limits.max_plain_bytes
            });
            if full {
                groups.push(Vec::new());
                size = 0;
            }
            size += row.1.len();
            groups.last_mut().expect("pushed above").push(row);
        }
        for group in &groups {
            let ops = group
                .iter()
                .map(|(_, bytes)| {
                    let mut d = Decoder::new(bytes);
                    let op = decode_op(&mut d)?;
                    d.finish()?;
                    Ok(op)
                })
                .collect::<Result<Vec<_>>>()?;
            let bytes = segment::encode(
                &Segment {
                    db_id,
                    device_id,
                    seq,
                    ops,
                },
                limits.compress,
            )?;
            let seq_sql = seq as i64;
            tx.execute(
                "INSERT INTO pending_uploads (seq, file_id, bytes, state) VALUES (?1, NULL, ?2, 'pending')",
                params![seq_sql, bytes],
            )?;
            let (first, last) = (group[0].0, group[group.len() - 1].0);
            tx.execute(
                "UPDATE outbox SET segment_seq = ?1 WHERE segment_seq IS NULL AND local_seq BETWEEN ?2 AND ?3",
                params![seq_sql, first, last],
            )?;
            seq += 1;
        }
        set_meta(&tx, "next_seq", seq as i64)?;
        tx.commit()?;
        Ok(Packed {
            segments: groups.len(),
            ops: groups.iter().map(Vec::len).sum(),
        })
    }

    /// Advances the clock for a non-op event (database creation, joining) and persists it.
    pub fn tick(&mut self) -> Result<crate::Hlc> {
        let hlc = self.hlc.now(self.clock.now_ms());
        set_meta(&self.conn, "hlc_last", super::hlc_to_sql(hlc))?;
        Ok(hlc)
    }

    /// Whether new segments should be compressed (from the manifest).
    pub fn compress(&self) -> Result<bool> {
        Ok(get_meta(&self.conn, "compression")?.as_deref() != Some(b"none".as_slice()))
    }

    /// Stores the manifest's compression default.
    pub fn set_compress(&mut self, compress: bool) -> Result<()> {
        let v: &[u8] = if compress { b"zstd" } else { b"none" };
        set_meta(&self.conn, "compression", v)
    }

    /// Segments waiting for upload, oldest first.
    pub fn pending_uploads(&self) -> Result<Vec<PendingUpload>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, file_id, bytes FROM pending_uploads WHERE state = 'pending' ORDER BY seq",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingUpload {
                seq: r.get::<_, i64>(0)? as u64,
                reservation: r.get(1)?,
                bytes: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stores the backend token for a pending upload before its first attempt.
    pub fn set_reservation(&mut self, seq: u64, reservation: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_uploads SET file_id = ?2 WHERE seq = ?1",
            params![seq as i64, reservation],
        )?;
        Ok(())
    }

    /// Counts a failed attempt.
    pub fn record_attempt(&mut self, seq: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_uploads SET attempts = attempts + 1 WHERE seq = ?1",
            [seq as i64],
        )?;
        Ok(())
    }

    /// Drops an uploaded segment's bytes. The outbox rows stay until a snapshot covers them.
    pub fn finish_upload(&mut self, seq: u64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_uploads WHERE seq = ?1", [seq as i64])?;
        Ok(())
    }

    /// Whether segment `seq` of `device` has been applied.
    pub fn is_applied(&self, device: DeviceId, seq: u64) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM applied_segments WHERE device_id = ?1 AND seq = ?2)
                OR EXISTS (SELECT 1 FROM devices WHERE device_id = ?1 AND watermark >= ?2)",
            params![device.as_bytes().as_slice(), seq as i64],
            |r| r.get(0),
        )?)
    }

    /// Number of local ops ever written to the outbox (packed or not).
    pub fn outbox_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Saved change-feed cursor.
    pub fn change_cursor(&self) -> Result<Option<String>> {
        get_meta(&self.conn, "change_cursor")?
            .map(|b| String::from_utf8(b).map_err(|_| Error::corrupt("stored cursor is not UTF-8")))
            .transpose()
    }

    /// When the change feed was last backed up by a full listing.
    pub fn last_full_list_ms(&self) -> Result<u64> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_full_list_ms'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.unwrap_or(0) as u64)
    }

    /// Records a full listing.
    pub fn set_last_full_list_ms(&mut self, ms: u64) -> Result<()> {
        set_meta(
            &self.conn,
            "last_full_list_ms",
            ms.min(i64::MAX as u64) as i64,
        )
    }

    /// Saves the change-feed cursor.
    pub fn set_change_cursor(&mut self, cursor: &str) -> Result<()> {
        set_meta(&self.conn, "change_cursor", cursor.as_bytes())
    }
}
