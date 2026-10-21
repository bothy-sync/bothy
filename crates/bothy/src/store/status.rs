//! Read-only summaries for status displays and tools.

use rusqlite::{OptionalExtension, params};

use super::{Store, device_from_sql, hlc_from_sql, set_meta};
use crate::Hlc;
use crate::error::{Error, Result};
use crate::model::{CollectionName, DeviceId};

/// What the local database knows about one remote device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceStatus {
    /// The device.
    pub device_id: DeviceId,
    /// Local label, if set.
    pub name: Option<String>,
    /// Highest seq of this device applied without gaps.
    pub watermark: u64,
    /// Largest HLC seen from this device.
    pub last_seen: Option<Hlc>,
}

/// A snapshot of local counters.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalStatus {
    /// Visible documents.
    pub documents: u64,
    /// Deleted documents kept as tombstones.
    pub tombstones: u64,
    /// Local ops not yet packed into a segment.
    pub unpacked_ops: u64,
    /// Packed segments not yet uploaded.
    pub pending_uploads: u64,
    /// Wall-clock time of the last successful sync (ms since the epoch).
    pub last_sync_ms: Option<u64>,
    /// Latest HLC produced or observed.
    pub last_hlc: Hlc,
    /// Other devices, by id.
    pub devices: Vec<DeviceStatus>,
}

impl Store {
    fn count(&self, sql: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(sql, [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Local counters and known devices.
    pub fn status(&self) -> Result<LocalStatus> {
        let last_sync: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_sync_ms'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(LocalStatus {
            documents: self.count("SELECT COUNT(*) FROM docs")?,
            tombstones: self.count("SELECT COUNT(*) FROM doc_tombstones")?,
            unpacked_ops: self.count("SELECT COUNT(*) FROM outbox WHERE segment_seq IS NULL")?,
            pending_uploads: self
                .count("SELECT COUNT(*) FROM pending_uploads WHERE state = 'pending'")?,
            last_sync_ms: last_sync.map(|v| v as u64),
            last_hlc: self.last_hlc(),
            devices: self.devices()?,
        })
    }

    /// Other devices this database has seen, ordered by id.
    pub fn devices(&self) -> Result<Vec<DeviceStatus>> {
        let mut stmt = self.conn.prepare(
            "SELECT device_id, name, watermark, last_seen_hlc FROM devices ORDER BY device_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (d, name, w, seen) = row?;
            Ok(DeviceStatus {
                device_id: device_from_sql(d)?,
                name,
                watermark: w as u64,
                last_seen: seen.filter(|&s| s != 0).map(hlc_from_sql),
            })
        })
        .collect()
    }

    /// Sets a local label for a device (not synced: `device.json` is immutable).
    pub fn set_device_name(&mut self, device: DeviceId, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO devices (device_id, name) VALUES (?1, ?2)
             ON CONFLICT (device_id) DO UPDATE SET name = excluded.name",
            params![device.as_bytes().as_slice(), name],
        )?;
        Ok(())
    }

    /// Collections that have at least one visible document.
    pub fn collections(&self) -> Result<Vec<CollectionName>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT collection FROM docs ORDER BY 1")?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        names
            .iter()
            .map(|n| CollectionName::new(n).map_err(|_| Error::corrupt("bad stored collection")))
            .collect()
    }

    /// Records a successful sync.
    pub fn set_last_sync_ms(&mut self, ms: u64) -> Result<()> {
        set_meta(&self.conn, "last_sync_ms", ms.min(i64::MAX as u64) as i64)
    }
}
