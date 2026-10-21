//! Operations and stamps.

use std::collections::{BTreeMap, BTreeSet};

use crate::Hlc;
use crate::error::{Error, Result};
use crate::model::{CollectionName, DeviceId, DocId, FieldName, Value};

/// Total order over operations: `hlc` first, then `device` byte by byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp {
    /// Timestamp of the operation.
    pub hlc: Hlc,
    /// Device that produced the operation.
    pub device: DeviceId,
}

impl Stamp {
    /// Builds a stamp.
    pub const fn new(hlc: Hlc, device: DeviceId) -> Self {
        Self { hlc, device }
    }
}

/// What an operation does to one document.
#[derive(Clone, Debug, PartialEq)]
pub enum OpKind {
    /// Sets fields to values (kind 0). Never empty.
    SetFields(BTreeMap<FieldName, Value>),
    /// Removes fields (kind 1). Never empty.
    UnsetFields(BTreeSet<FieldName>),
    /// Deletes the document (kind 2).
    DeleteDoc,
}

impl OpKind {
    /// Wire code of the kind.
    pub const fn code(&self) -> u64 {
        match self {
            OpKind::SetFields(_) => 0,
            OpKind::UnsetFields(_) => 1,
            OpKind::DeleteDoc => 2,
        }
    }

    /// A `SetFields` kind; fails if `fields` is empty or a value is invalid.
    pub fn set_fields(fields: BTreeMap<FieldName, Value>) -> Result<Self> {
        if fields.is_empty() {
            return Err(Error::invalid_input("SetFields needs at least one field"));
        }
        fields.values().try_for_each(Value::validate)?;
        Ok(OpKind::SetFields(fields))
    }

    /// An `UnsetFields` kind; fails if `fields` is empty.
    pub fn unset_fields(fields: BTreeSet<FieldName>) -> Result<Self> {
        if fields.is_empty() {
            return Err(Error::invalid_input("UnsetFields needs at least one field"));
        }
        Ok(OpKind::UnsetFields(fields))
    }
}

/// One change to one document. The device id comes from the segment that carries it.
#[derive(Clone, Debug, PartialEq)]
pub struct Op {
    /// Timestamp; strictly increasing per device.
    pub hlc: Hlc,
    /// Target collection.
    pub collection: CollectionName,
    /// Target document.
    pub doc_id: DocId,
    /// The change.
    pub kind: OpKind,
}

impl Op {
    /// The stamp of this operation when produced by `device`.
    pub fn stamp(&self, device: DeviceId) -> Stamp {
        Stamp::new(self.hlc, device)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(b: u8) -> DeviceId {
        DeviceId::from_bytes([b; 16])
    }

    #[test]
    fn stamps_order_by_hlc_then_device() {
        let h1 = Hlc::from_u64(1);
        let h2 = Hlc::from_u64(2);
        assert!(Stamp::new(h1, dev(9)) < Stamp::new(h2, dev(0)));
        assert!(Stamp::new(h1, dev(0)) < Stamp::new(h1, dev(1)));
    }

    #[test]
    fn empty_kinds_are_rejected() {
        assert!(OpKind::set_fields(BTreeMap::new()).is_err());
        assert!(OpKind::unset_fields(BTreeSet::new()).is_err());
    }

    #[test]
    fn set_fields_validates_values() {
        let mut m = BTreeMap::new();
        m.insert(FieldName::new("x").unwrap(), Value::Float(f64::NAN));
        assert!(OpKind::set_fields(m).is_err());
    }
}
