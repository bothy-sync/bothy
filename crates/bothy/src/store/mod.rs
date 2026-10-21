//! Local SQLite store: merge state, read view and outbox, kept consistent in one transaction.
//!
//! This is the synchronous layer under the (upcoming) async `Database` API.

mod encryption;
mod schema;
mod snapshots;
mod status;
mod sync_state;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::codec::cbor::{Encoder, decode_value, encode_value};
use crate::codec::state::encode_entry;
use crate::codec::{decode_op, encode_op};
use crate::error::{Error, ErrorKind, Result};
use crate::hlc::{Clock, Hlc, HlcClock};
use crate::merge::{DocState, FieldState, Superseded};
use crate::model::{CollectionName, DeviceId, DocId, FieldName, Op, OpKind, Stamp, Value};

pub use schema::SCHEMA_VERSION;
pub use snapshots::SinceSnapshot;
pub use status::{DeviceStatus, LocalStatus};
pub use sync_state::{PackLimits, Packed, PendingUpload};

/// Most recent conflict records kept per database, by default.
pub const DEFAULT_CONFLICT_LOG_LIMIT: u32 = 1_000;

/// Fields of a visible document.
pub type Document = BTreeMap<FieldName, Value>;

/// A write that lost to a write from another device.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Conflict {
    /// The document's collection.
    pub collection: CollectionName,
    /// The document.
    pub doc_id: DocId,
    /// When this device recorded it (local wall clock, ms since the epoch).
    pub at_ms: u64,
    /// The field.
    pub field: FieldName,
    /// Stamp of the winning write.
    pub winner: Stamp,
    /// Stamp of the losing write.
    pub loser: Stamp,
    /// The losing value; `None` if the losing write unset the field.
    pub loser_value: Option<Value>,
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode as C;
        let kind = match e.sqlite_error_code() {
            Some(C::DatabaseBusy | C::DatabaseLocked) => ErrorKind::DatabaseLocked,
            Some(C::DatabaseCorrupt | C::NotADatabase) => ErrorKind::Corrupt,
            _ => ErrorKind::Internal,
        };
        Error::new(kind, "local database error").with_source(e)
    }
}

fn io_error(what: &'static str, e: std::io::Error) -> Error {
    Error::new(ErrorKind::Internal, what).with_source(e)
}

fn hlc_to_sql(h: Hlc) -> i64 {
    h.as_u64() as i64
}

fn hlc_from_sql(v: i64) -> Hlc {
    Hlc::from_u64(v as u64)
}

fn device_from_sql(b: Vec<u8>) -> Result<DeviceId> {
    let b: [u8; 16] = b
        .try_into()
        .map_err(|_| Error::corrupt("stored device id is not 16 bytes"))?;
    Ok(DeviceId::from_bytes(b))
}

/// The local database of one device.
pub struct Store {
    conn: Connection,
    _lock: File,
    device: DeviceId,
    hlc: HlcClock,
    clock: Arc<dyn Clock>,
    id_seed: Option<u64>,
    /// `None`: conflicts are not logged.
    conflict_limit: Option<u32>,
}

/// A 16-byte id derived from a test seed.
fn derive_id(seed: u64, purpose: &[u8], extra: &[u8]) -> [u8; 16] {
    let mut h = blake3::Hasher::new();
    h.update(&seed.to_be_bytes());
    h.update(purpose);
    h.update(extra);
    h.finalize().as_bytes()[..16].try_into().expect("16 bytes")
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("device", &self.device)
            .field("hlc", &self.hlc.last())
            .finish_non_exhaustive()
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

impl Store {
    /// Opens or creates the database at `path`. A second open of the same path, from this or
    /// another process, fails with `DatabaseLocked` while the first is alive.
    pub fn open(path: impl AsRef<Path>, clock: Arc<dyn Clock>) -> Result<Store> {
        Self::open_with(path, clock, None)
    }

    /// Like [`Store::open`], but with `id_seed` every new id (device, database, fork) is derived
    /// from the seed instead of the system random generator. Only for deterministic tests.
    pub fn open_with(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
        id_seed: Option<u64>,
    ) -> Result<Store> {
        let path = path.as_ref();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(path))
            .map_err(|e| io_error("cannot open lock file", e))?;
        match fs4::FileExt::try_lock(&lock) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock) => {
                return Err(Error::new(
                    ErrorKind::DatabaseLocked,
                    format!("{} is open in another process", path.display()),
                ));
            }
            Err(fs4::TryLockError::Error(e)) => return Err(io_error("cannot lock database", e)),
        }

        let mut conn = Connection::open(path)?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::new(ErrorKind::Internal, "SQLite refused WAL mode"));
        }
        conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        schema::migrate(&mut conn)?;

        let device = match get_meta(&conn, "device_id")? {
            Some(b) => device_from_sql(b)?,
            None => {
                let d = match id_seed {
                    Some(seed) => DeviceId::from_bytes(derive_id(seed, b"device", &[])),
                    None => DeviceId::random()?,
                };
                set_meta(&conn, "device_id", d.as_bytes().as_slice())?;
                d
            }
        };
        let last: Option<i64> = conn
            .query_row("SELECT value FROM meta WHERE key = 'hlc_last'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let hlc = HlcClock::resume(last.map_or(Hlc::ZERO, hlc_from_sql));
        Ok(Store {
            conn,
            _lock: lock,
            device,
            hlc,
            clock,
            id_seed,
            conflict_limit: Some(DEFAULT_CONFLICT_LOG_LIMIT),
        })
    }

    /// A new database id (from the test seed if the store has one).
    pub fn new_db_id(&self) -> Result<crate::model::DbId> {
        match self.id_seed {
            Some(seed) => Ok(crate::model::DbId::from_bytes(derive_id(seed, b"db", &[]))),
            None => crate::model::DbId::random(),
        }
    }

    pub(crate) fn new_fork_id(&self) -> Result<DeviceId> {
        match self.id_seed {
            Some(seed) => Ok(DeviceId::from_bytes(derive_id(
                seed,
                b"fork",
                self.device.as_bytes(),
            ))),
            None => DeviceId::random(),
        }
    }

    /// This device's id.
    pub fn device_id(&self) -> DeviceId {
        self.device
    }

    /// Latest HLC produced or observed.
    pub fn last_hlc(&self) -> Hlc {
        self.hlc.last()
    }

    /// Replaces a document. Fields missing from `doc` are unset. Returns whether anything changed.
    pub fn put(&mut self, collection: &CollectionName, id: &DocId, doc: Document) -> Result<bool> {
        check_user_names(collection, doc.keys())?;
        self.write_local(collection, id, |state| {
            let visible: BTreeMap<_, _> = state.visible().collect();
            let set: Document = doc
                .iter()
                .filter(|(k, v)| visible.get(k) != Some(v))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let unset: BTreeSet<FieldName> = visible
                .keys()
                .filter(|k| !doc.contains_key(*k))
                .map(|k| (*k).clone())
                .collect();
            changes(set, unset)
        })
    }

    /// Sets and unsets some fields of a document, creating it if needed.
    /// Returns whether anything changed.
    pub fn patch(
        &mut self,
        collection: &CollectionName,
        id: &DocId,
        set: Document,
        unset: BTreeSet<FieldName>,
    ) -> Result<bool> {
        check_user_names(collection, set.keys().chain(unset.iter()))?;
        if let Some(both) = set.keys().find(|k| unset.contains(*k)) {
            return Err(Error::invalid_input(format!(
                "field {both} is both set and unset"
            )));
        }
        self.write_local(collection, id, |state| {
            let visible: BTreeMap<_, _> = state.visible().collect();
            let set: Document = set
                .into_iter()
                .filter(|(k, v)| visible.get(k) != Some(&v))
                .collect();
            let unset = unset
                .into_iter()
                .filter(|k| visible.contains_key(k))
                .collect();
            changes(set, unset)
        })
    }

    /// Deletes a document. Returns `false` if it did not exist.
    pub fn delete(&mut self, collection: &CollectionName, id: &DocId) -> Result<bool> {
        check_user_names(collection, std::iter::empty())?;
        self.write_local(collection, id, |state| {
            Ok(if state.exists() {
                vec![OpKind::DeleteDoc]
            } else {
                Vec::new()
            })
        })
    }

    fn write_local(
        &mut self,
        collection: &CollectionName,
        id: &DocId,
        plan: impl FnOnce(&DocState) -> Result<Vec<OpKind>>,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let before = load_doc(&tx, collection, id)?;
        let kinds = plan(&before)?;
        if kinds.is_empty() {
            return Ok(false);
        }
        let mut after = before.clone();
        let wall = self.clock.now_ms();
        let mut encoded = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let hlc = self.hlc.now(wall);
            after.apply(&kind, Stamp::new(hlc, self.device));
            let op = Op {
                hlc,
                collection: collection.clone(),
                doc_id: id.clone(),
                kind,
            };
            let mut e = Encoder::new();
            encode_op(&mut e, &op)?;
            encoded.push((hlc, e.into_bytes()));
        }
        for (hlc, bytes) in encoded {
            tx.execute(
                "INSERT INTO outbox (hlc, op) VALUES (?1, ?2)",
                params![hlc_to_sql(hlc), bytes],
            )?;
        }
        save_doc(&tx, collection, id, &before, &after)?;
        set_meta(&tx, "hlc_last", hlc_to_sql(self.hlc.last()))?;
        tx.commit()?;
        Ok(true)
    }

    /// Visible fields of a document, or `None` if it does not exist.
    pub fn get(&self, collection: &CollectionName, id: &DocId) -> Result<Option<Document>> {
        let state = load_doc(&self.conn, collection, id)?;
        Ok(state.exists().then(|| visible_doc(&state)))
    }

    /// Full merge state of a document (stamps, unset fields, delete stamp).
    pub fn doc_state(&self, collection: &CollectionName, id: &DocId) -> Result<DocState> {
        load_doc(&self.conn, collection, id)
    }

    /// Sets how many conflict records are kept; `None` stops logging (existing records stay
    /// until [`Store::clear_conflicts`]).
    pub fn set_conflict_log(&mut self, limit: Option<u32>) {
        self.conflict_limit = limit;
    }

    /// Recorded conflicts for a document, newest first.
    pub fn conflicts(&self, collection: &CollectionName, id: &DocId) -> Result<Vec<Conflict>> {
        self.query_conflicts(
            "WHERE collection = ?1 AND doc_id = ?2",
            params![collection.as_str(), id.as_str()],
        )
    }

    /// Recorded conflicts across the database, newest first; with `since_ms`, only those
    /// recorded at or after that time.
    pub fn recent_conflicts(&self, since_ms: Option<u64>) -> Result<Vec<Conflict>> {
        let since = since_ms.unwrap_or(0).min(i64::MAX as u64) as i64;
        self.query_conflicts("WHERE at >= ?1", params![since])
    }

    /// Deletes every conflict record. Returns how many there were.
    pub fn clear_conflicts(&mut self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM conflict_log", [])?)
    }

    fn query_conflicts(
        &self,
        filter: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<Conflict>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT collection, doc_id, at, field, winner_hlc, winner_device, loser_hlc,
               loser_device, loser_value
             FROM conflict_log {filter} ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map(args, |r| {
            Ok((
                (
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ),
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Vec<u8>>(7)?,
                r.get::<_, Option<Vec<u8>>>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let ((c, d, at), field, wh, wd, lh, ld, lv) = row?;
            let bad = |what| move |_| Error::corrupt(format!("bad stored {what}"));
            out.push(Conflict {
                collection: CollectionName::new(&c).map_err(bad("collection name"))?,
                doc_id: DocId::new(&d).map_err(bad("document id"))?,
                at_ms: at.max(0) as u64,
                field: FieldName::new(&field).map_err(bad("field name"))?,
                winner: Stamp::new(hlc_from_sql(wh), device_from_sql(wd)?),
                loser: Stamp::new(hlc_from_sql(lh), device_from_sql(ld)?),
                loser_value: lv.map(|b| decode_value(&b)).transpose()?,
            });
        }
        Ok(out)
    }

    /// Visible documents of a collection in id order, starting after `after`.
    pub fn list(
        &self,
        collection: &CollectionName,
        limit: usize,
        after: Option<&DocId>,
    ) -> Result<Vec<(DocId, Document)>> {
        let mut stmt = self.conn.prepare(
            "SELECT doc_id FROM docs WHERE collection = ?1 AND doc_id > ?2
             ORDER BY doc_id LIMIT ?3",
        )?;
        let ids: Vec<String> = stmt
            .query_map(
                params![
                    collection.as_str(),
                    after.map_or("", DocId::as_str),
                    limit.min(i64::MAX as usize) as i64
                ],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| {
                let id = DocId::new(&id).map_err(|_| Error::corrupt("bad stored document id"))?;
                let state = load_doc(&self.conn, collection, &id)?;
                Ok((id, visible_doc(&state)))
            })
            .collect()
    }

    /// Operations not yet packed into a segment, oldest first, with their outbox keys.
    pub fn unpacked_ops(&self) -> Result<Vec<(i64, Op)>> {
        let mut stmt = self.conn.prepare(
            "SELECT local_seq, op FROM outbox WHERE segment_seq IS NULL ORDER BY local_seq",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, bytes) = row?;
            let mut d = crate::codec::cbor::Decoder::new(&bytes);
            let op = decode_op(&mut d)?;
            d.finish()?;
            out.push((seq, op));
        }
        Ok(out)
    }

    /// Applies the operations of segment `seq` from another device in one transaction.
    ///
    /// Returns `None` if the segment was already applied, otherwise the documents whose
    /// visible content changed. Writes that lose to another device's write are logged.
    pub fn apply_remote(
        &mut self,
        device: DeviceId,
        seq: u64,
        ops: &[Op],
    ) -> Result<Option<Vec<(CollectionName, DocId)>>> {
        if device == self.device {
            return Err(Error::new(
                ErrorKind::RemoteInconsistent,
                "refusing to apply a segment that carries this device's own id",
            ));
        }
        let seq_sql = i64::try_from(seq).map_err(|_| Error::corrupt("segment seq out of range"))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let done: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM applied_segments WHERE device_id = ?1 AND seq = ?2)
                OR EXISTS (SELECT 1 FROM devices WHERE device_id = ?1 AND watermark >= ?2)",
            params![device.as_bytes().as_slice(), seq_sql],
            |r| r.get(0),
        )?;
        if done {
            return Ok(None);
        }

        let wall = self.clock.now_ms();
        let log_limit = self.conflict_limit;
        let mut docs: BTreeMap<(CollectionName, DocId), (DocState, DocState)> = BTreeMap::new();
        let mut max_hlc = Hlc::ZERO;
        for op in ops {
            max_hlc = max_hlc.max(op.hlc);
            let key = (op.collection.clone(), op.doc_id.clone());
            if !docs.contains_key(&key) {
                let state = load_doc(&tx, &key.0, &key.1)?;
                docs.insert(key.clone(), (state.clone(), state));
            }
            let (_, after) = docs.get_mut(&key).expect("inserted above");
            let lost = after.apply(&op.kind, op.stamp(device));
            log_conflicts(&tx, &key.0, &key.1, &lost, wall, log_limit)?;
        }

        let mut changed = Vec::new();
        for ((collection, id), (before, after)) in &docs {
            if visible_doc(before) != visible_doc(after) || before.exists() != after.exists() {
                changed.push((collection.clone(), id.clone()));
            }
            save_doc(&tx, collection, id, before, after)?;
        }

        tx.execute(
            "INSERT INTO applied_segments (device_id, seq) VALUES (?1, ?2)",
            params![device.as_bytes().as_slice(), seq_sql],
        )?;
        advance_watermark(&tx, device, max_hlc)?;
        trim_conflicts(&tx, log_limit)?;
        if max_hlc > Hlc::ZERO {
            self.hlc.observe(max_hlc, wall);
        }
        set_meta(&tx, "hlc_last", hlc_to_sql(self.hlc.last()))?;
        tx.commit()?;
        Ok(Some(changed))
    }

    /// Highest `seq` of `device` such that every segment `1..=seq` has been applied.
    pub fn watermark(&self, device: DeviceId) -> Result<u64> {
        let w: Option<i64> = self
            .conn
            .query_row(
                "SELECT watermark FROM devices WHERE device_id = ?1",
                [device.as_bytes().as_slice()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(w.unwrap_or(0) as u64)
    }

    /// Canonical dump of the whole merge state, tombstones included. Two devices that applied
    /// the same operations produce identical bytes.
    pub fn canonical_dump(&self) -> Result<Vec<u8>> {
        let mut stmt = self.conn.prepare(
            "SELECT collection, doc_id FROM fields
             UNION SELECT collection, doc_id FROM doc_tombstones
             ORDER BY 1, 2",
        )?;
        let keys: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut e = Encoder::new();
        e.array(keys.len());
        for (c, d) in keys {
            let c = CollectionName::new(&c).map_err(|_| Error::corrupt("bad stored collection"))?;
            let d = DocId::new(&d).map_err(|_| Error::corrupt("bad stored document id"))?;
            let state = load_doc(&self.conn, &c, &d)?;
            encode_entry(&mut e, &c, &d, &state)?;
        }
        Ok(e.into_bytes())
    }
}

fn changes(set: Document, unset: BTreeSet<FieldName>) -> Result<Vec<OpKind>> {
    let mut kinds = Vec::new();
    if !set.is_empty() {
        kinds.push(OpKind::set_fields(set)?);
    }
    if !unset.is_empty() {
        kinds.push(OpKind::unset_fields(unset)?);
    }
    Ok(kinds)
}

fn check_user_names<'a>(
    collection: &CollectionName,
    mut fields: impl Iterator<Item = &'a FieldName>,
) -> Result<()> {
    if collection.is_reserved() {
        return Err(Error::invalid_input(format!(
            "collection {collection} is reserved for Bothy"
        )));
    }
    if let Some(f) = fields.find(|f| f.is_reserved()) {
        return Err(Error::invalid_input(format!(
            "field {f} is reserved: names starting with '_' belong to Bothy"
        )));
    }
    Ok(())
}

fn visible_doc(state: &DocState) -> Document {
    state
        .visible()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

fn set_meta(conn: &Connection, key: &str, value: impl rusqlite::ToSql) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

fn load_doc(conn: &Connection, collection: &CollectionName, id: &DocId) -> Result<DocState> {
    let mut state = DocState::default();
    let tomb: Option<(i64, Vec<u8>)> = conn
        .query_row(
            "SELECT hlc, device FROM doc_tombstones WHERE collection = ?1 AND doc_id = ?2",
            params![collection.as_str(), id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((h, d)) = tomb {
        state.deleted_at = Some(Stamp::new(hlc_from_sql(h), device_from_sql(d)?));
    }
    let mut stmt = conn.prepare_cached(
        "SELECT field, hlc, device, value FROM fields WHERE collection = ?1 AND doc_id = ?2",
    )?;
    let rows = stmt.query_map(params![collection.as_str(), id.as_str()], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Vec<u8>>(2)?,
            r.get::<_, Option<Vec<u8>>>(3)?,
        ))
    })?;
    for row in rows {
        let (field, h, d, v) = row?;
        let field = FieldName::new(&field).map_err(|_| Error::corrupt("bad stored field name"))?;
        state.fields.insert(
            field,
            FieldState {
                stamp: Stamp::new(hlc_from_sql(h), device_from_sql(d)?),
                value: v.map(|b| decode_value(&b)).transpose()?,
            },
        );
    }
    Ok(state)
}

fn save_doc(
    tx: &Transaction<'_>,
    collection: &CollectionName,
    id: &DocId,
    before: &DocState,
    after: &DocState,
) -> Result<()> {
    let (c, d) = (collection.as_str(), id.as_str());
    for (name, f) in &after.fields {
        if before.fields.get(name) == Some(f) {
            continue;
        }
        let value = f.value.as_ref().map(encode_value).transpose()?;
        tx.execute(
            "INSERT OR REPLACE INTO fields (collection, doc_id, field, hlc, device, value)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                c,
                d,
                name.as_str(),
                hlc_to_sql(f.stamp.hlc),
                f.stamp.device.as_bytes().as_slice(),
                value
            ],
        )?;
    }
    if after.deleted_at != before.deleted_at
        && let Some(s) = after.deleted_at
    {
        tx.execute(
            "INSERT OR REPLACE INTO doc_tombstones (collection, doc_id, hlc, device)
             VALUES (?1, ?2, ?3, ?4)",
            params![c, d, hlc_to_sql(s.hlc), s.device.as_bytes().as_slice()],
        )?;
    }
    if after.exists() {
        let json = serde_json::Value::Object(
            after
                .visible()
                .map(|(k, v)| (k.to_string(), v.to_json()))
                .collect(),
        );
        let updated = after
            .fields
            .values()
            .map(|f| f.stamp.hlc)
            .chain(after.deleted_at.map(|s| s.hlc))
            .max()
            .unwrap_or(Hlc::ZERO);
        tx.execute(
            "INSERT OR REPLACE INTO docs (collection, doc_id, json, updated_hlc)
             VALUES (?1, ?2, ?3, ?4)",
            params![c, d, json.to_string(), hlc_to_sql(updated)],
        )?;
    } else {
        tx.execute(
            "DELETE FROM docs WHERE collection = ?1 AND doc_id = ?2",
            params![c, d],
        )?;
    }
    Ok(())
}

fn log_conflicts(
    tx: &Transaction<'_>,
    collection: &CollectionName,
    id: &DocId,
    lost: &[Superseded],
    wall: u64,
    limit: Option<u32>,
) -> Result<()> {
    if limit.is_none_or(|l| l == 0) {
        return Ok(());
    }
    for s in lost
        .iter()
        .filter(|s| s.winner.device != s.loser.stamp.device)
    {
        let value = s.loser.value.as_ref().map(encode_value).transpose()?;
        tx.execute(
            "INSERT INTO conflict_log (collection, doc_id, field, winner_hlc, winner_device,
               loser_hlc, loser_device, loser_value, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                collection.as_str(),
                id.as_str(),
                s.field.as_str(),
                hlc_to_sql(s.winner.hlc),
                s.winner.device.as_bytes().as_slice(),
                hlc_to_sql(s.loser.stamp.hlc),
                s.loser.stamp.device.as_bytes().as_slice(),
                value,
                wall.min(i64::MAX as u64) as i64
            ],
        )?;
    }
    Ok(())
}

/// Keeps only the newest `limit` conflict records.
fn trim_conflicts(tx: &Transaction<'_>, limit: Option<u32>) -> Result<()> {
    if let Some(limit) = limit {
        tx.execute(
            "DELETE FROM conflict_log WHERE id <= (SELECT MAX(id) FROM conflict_log) - ?1",
            [i64::from(limit)],
        )?;
    }
    Ok(())
}

fn advance_watermark(tx: &Transaction<'_>, device: DeviceId, seen: Hlc) -> Result<()> {
    let dev = device.as_bytes().as_slice();
    tx.execute(
        "INSERT INTO devices (device_id, watermark, last_seen_hlc) VALUES (?1, 0, ?2)
         ON CONFLICT (device_id) DO UPDATE SET
           last_seen_hlc = MAX(COALESCE(last_seen_hlc, 0), excluded.last_seen_hlc)",
        params![dev, hlc_to_sql(seen)],
    )?;
    let mut w: i64 = tx.query_row(
        "SELECT watermark FROM devices WHERE device_id = ?1",
        [dev],
        |r| r.get(0),
    )?;
    loop {
        let next: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM applied_segments WHERE device_id = ?1 AND seq = ?2)",
            params![dev, w + 1],
            |r| r.get(0),
        )?;
        if !next {
            break;
        }
        w += 1;
    }
    tx.execute(
        "UPDATE devices SET watermark = ?2 WHERE device_id = ?1",
        params![dev, w],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
