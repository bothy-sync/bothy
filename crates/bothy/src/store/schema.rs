//! Local schema and forward-only migrations.

use rusqlite::{Connection, OptionalExtension};

use crate::error::{Error, ErrorKind, Result};

/// Each entry migrates from version `index` to `index + 1`.
const MIGRATIONS: &[&str] = &[
    // v1
    "
    CREATE TABLE meta (
      key TEXT PRIMARY KEY,
      value BLOB NOT NULL
    );

    -- Merge state: the source of truth. value = canonical CBOR, NULL = unset field.
    CREATE TABLE fields (
      collection TEXT NOT NULL,
      doc_id TEXT NOT NULL,
      field TEXT NOT NULL,
      hlc INTEGER NOT NULL,
      device BLOB NOT NULL,
      value BLOB,
      PRIMARY KEY (collection, doc_id, field)
    ) WITHOUT ROWID;

    CREATE TABLE doc_tombstones (
      collection TEXT NOT NULL,
      doc_id TEXT NOT NULL,
      hlc INTEGER NOT NULL,
      device BLOB NOT NULL,
      PRIMARY KEY (collection, doc_id)
    ) WITHOUT ROWID;

    -- Read view of visible documents, updated in the same transaction as the merge state.
    CREATE TABLE docs (
      collection TEXT NOT NULL,
      doc_id TEXT NOT NULL,
      json TEXT NOT NULL,
      updated_hlc INTEGER NOT NULL,
      PRIMARY KEY (collection, doc_id)
    ) WITHOUT ROWID;

    CREATE TABLE outbox (
      local_seq INTEGER PRIMARY KEY AUTOINCREMENT,
      hlc INTEGER NOT NULL,
      op BLOB NOT NULL,
      segment_seq INTEGER
    );
    CREATE INDEX outbox_unpacked ON outbox (local_seq) WHERE segment_seq IS NULL;

    CREATE TABLE pending_uploads (
      seq INTEGER PRIMARY KEY,
      file_id TEXT,
      bytes BLOB NOT NULL,
      state TEXT NOT NULL CHECK (state IN ('pending', 'uploaded')),
      attempts INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE devices (
      device_id BLOB PRIMARY KEY,
      name TEXT,
      platform TEXT,
      watermark INTEGER NOT NULL DEFAULT 0,
      last_seen_hlc INTEGER
    );

    CREATE TABLE applied_segments (
      device_id BLOB NOT NULL,
      seq INTEGER NOT NULL,
      PRIMARY KEY (device_id, seq)
    ) WITHOUT ROWID;

    CREATE TABLE remote_files (
      key TEXT PRIMARY KEY,
      file_id TEXT,
      size INTEGER,
      hash TEXT
    );

    CREATE TABLE conflict_log (
      id INTEGER PRIMARY KEY,
      collection TEXT NOT NULL,
      doc_id TEXT NOT NULL,
      field TEXT NOT NULL,
      winner_hlc INTEGER NOT NULL,
      winner_device BLOB NOT NULL,
      loser_hlc INTEGER NOT NULL,
      loser_device BLOB NOT NULL,
      loser_value BLOB,
      at INTEGER NOT NULL
    );
    CREATE INDEX conflict_log_doc ON conflict_log (collection, doc_id);
    ",
    // v2: snapshots
    "
    -- Remote snapshots already merged into the local state, by logical key.
    CREATE TABLE applied_snapshots (
      key TEXT PRIMARY KEY
    ) WITHOUT ROWID;

    -- Snapshots of this device; vv = canonical CBOR map device_id -> seq.
    -- state: 0 = upload started, 1 = confirmed on the remote, 2 = deleted by compaction.
    CREATE TABLE own_snapshots (
      hlc INTEGER PRIMARY KEY,
      vv BLOB NOT NULL,
      state INTEGER NOT NULL DEFAULT 0
    );
    ",
];

/// Schema version this build writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

fn current_version(conn: &Connection) -> Result<i64> {
    let has_meta: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
        [],
        |r| r.get(0),
    )?;
    if !has_meta {
        return Ok(0);
    }
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Brings the schema up to [`SCHEMA_VERSION`]. Fails for databases from a newer build.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let version = current_version(conn)?;
    if version > SCHEMA_VERSION {
        return Err(Error::new(
            ErrorKind::SchemaTooNew,
            format!(
                "local database has schema version {version}; this build supports {SCHEMA_VERSION}"
            ),
        ));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            [i as i64 + 1],
        )?;
        tx.commit()?;
    }
    Ok(())
}
