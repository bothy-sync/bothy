//! Local encryption state: whether the database is encrypted, its key check, and a copy of the
//! newest key file so a password unlocks offline. The data key itself is never stored here.

use rusqlite::OptionalExtension;

use super::{Store, get_meta, hlc_from_sql, hlc_to_sql, set_meta};
use crate::Hlc;
use crate::error::Result;

impl Store {
    /// The manifest's key check if the database is encrypted.
    pub fn key_check(&self) -> Result<Option<Vec<u8>>> {
        get_meta(&self.conn, "key_check")
    }

    /// Marks the database as encrypted with this key check.
    pub fn set_key_check(&mut self, check: &[u8]) -> Result<()> {
        set_meta(&self.conn, "key_check", check)
    }

    /// The newest key file seen: `(its HLC, its logical key, its JSON)`.
    pub fn cached_key_file(&self) -> Result<Option<(Hlc, String, Vec<u8>)>> {
        let hlc: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'key_file_hlc'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let name = get_meta(&self.conn, "key_file_name")?;
        let body = get_meta(&self.conn, "key_file")?;
        Ok(match (hlc, name, body) {
            (Some(h), Some(n), Some(b)) => {
                Some((hlc_from_sql(h), String::from_utf8_lossy(&n).into_owned(), b))
            }
            _ => None,
        })
    }

    /// Remembers a key file if it is newer than the cached one. Returns whether it replaced it.
    pub fn cache_key_file(&mut self, hlc: Hlc, name: &str, json: &[u8]) -> Result<bool> {
        if self.cached_key_file()?.is_some_and(|(h, _, _)| h >= hlc) {
            return Ok(false);
        }
        let tx = self.conn.transaction()?;
        set_meta(&tx, "key_file_hlc", hlc_to_sql(hlc))?;
        set_meta(&tx, "key_file_name", name.as_bytes())?;
        set_meta(&tx, "key_file", json)?;
        tx.commit()?;
        Ok(true)
    }
}
