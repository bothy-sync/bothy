//! Data model: values, identifiers, operations.

mod ids;
mod op;
mod value;

#[cfg(test)]
pub(crate) use ids::to_hex;
pub use ids::{CollectionName, DbId, DeviceId, DocId, FieldName};
pub use op::{Op, OpKind, Stamp};
pub use value::{MAX_DEPTH, Value};
