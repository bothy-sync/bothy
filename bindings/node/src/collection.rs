//! `Collection`: `put` and `get`.

use bothy::{ListOptions, Value};
use napi_derive::napi;
use std::collections::BTreeMap;

use crate::database::DbSlot;
use crate::error::{Result, from_engine};
use crate::value::{JsDoc, JsValue, Str};

/// A named group of documents. It holds the database's shared slot, not an engine handle, so
/// closing the database really releases it.
#[napi]
pub struct Collection {
    pub(crate) slot: DbSlot,
    pub(crate) name: String,
}

#[napi]
impl Collection {
    /// Writes a whole document. Returns whether anything changed.
    #[napi]
    pub async fn put(&self, id: Str, doc: JsDoc) -> Result<bool> {
        let open = self.slot.acquire().await?;
        let collection = open.db().collection(&self.name).map_err(from_engine)?;
        collection.put(&id.0, doc.0).await.map_err(from_engine)
    }

    /// The document, or `null`.
    #[napi]
    pub async fn get(&self, id: Str) -> Result<Option<JsValue>> {
        let open = self.slot.acquire().await?;
        let collection = open.db().collection(&self.name).map_err(from_engine)?;
        Ok(collection
            .get(&id.0)
            .await
            .map_err(from_engine)?
            .map(JsValue))
    }

    /// Sets the fields in `fields` and removes those in `unset`; creates the document if needed.
    /// Returns whether anything changed.
    #[napi]
    pub async fn patch(&self, id: Str, fields: JsDoc, unset: Option<Vec<Str>>) -> Result<bool> {
        let open = self.slot.acquire().await?;
        let collection = open.db().collection(&self.name).map_err(from_engine)?;
        let unset: Vec<String> = unset.unwrap_or_default().into_iter().map(|s| s.0).collect();
        let unset: Vec<&str> = unset.iter().map(String::as_str).collect();
        collection
            .patch(&id.0, fields.0, &unset)
            .await
            .map_err(from_engine)
    }

    /// Deletes a document. Returns whether it existed.
    #[napi]
    pub async fn delete(&self, id: Str) -> Result<bool> {
        let open = self.slot.acquire().await?;
        let collection = open.db().collection(&self.name).map_err(from_engine)?;
        collection.delete(&id.0).await.map_err(from_engine)
    }

    /// Documents in id order, as `[{ id, doc }]`. `limit` and `after` are checked by the facade.
    #[napi]
    pub async fn list(&self, limit: Option<u32>, after: Option<Str>) -> Result<JsValue> {
        let open = self.slot.acquire().await?;
        let collection = open.db().collection(&self.name).map_err(from_engine)?;
        let mut options = ListOptions::default();
        if let Some(limit) = limit {
            options = options.limit(limit as usize);
        }
        if let Some(after) = &after {
            options = options.after(&after.0);
        }
        let rows = collection.list(options).await.map_err(from_engine)?;
        Ok(JsValue(Value::Array(
            rows.into_iter()
                .map(|(id, doc)| {
                    let mut row = BTreeMap::new();
                    row.insert("id".to_owned(), Value::Text(id.as_str().to_owned()));
                    row.insert("doc".to_owned(), doc);
                    Value::Map(row)
                })
                .collect(),
        )))
    }
}
