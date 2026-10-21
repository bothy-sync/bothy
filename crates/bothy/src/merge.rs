//! Field-level last-writer-wins merge with delete stamps. Pure: no I/O.
//!
//! Applying the same set of operations in any order, any number of times, gives the same state.

use std::collections::BTreeMap;

use crate::model::{FieldName, OpKind, Stamp, Value};

/// Latest write to one field. `value == None` means the field was unset.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldState {
    /// Stamp of the winning write.
    pub stamp: Stamp,
    /// The value, or `None` for an unset field.
    pub value: Option<Value>,
}

/// Merge state of one document, tombstones included.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocState {
    /// Largest delete stamp seen, if any.
    pub deleted_at: Option<Stamp>,
    /// Latest write per field.
    pub fields: BTreeMap<FieldName, FieldState>,
}

/// A write that lost to another write on the same field.
#[derive(Clone, Debug, PartialEq)]
pub struct Superseded {
    /// The field.
    pub field: FieldName,
    /// Stamp of the write that won.
    pub winner: Stamp,
    /// The losing write.
    pub loser: FieldState,
}

impl DocState {
    /// Applies one operation with stamp `stamp`. Returns the writes that lost, whether the
    /// stored one or the incoming one.
    pub fn apply(&mut self, kind: &OpKind, stamp: Stamp) -> Vec<Superseded> {
        let mut lost = Vec::new();
        match kind {
            OpKind::SetFields(fields) => {
                for (name, value) in fields {
                    self.write(name, stamp, Some(value), &mut lost);
                }
            }
            OpKind::UnsetFields(fields) => {
                for name in fields {
                    self.write(name, stamp, None, &mut lost);
                }
            }
            OpKind::DeleteDoc => {
                if self.deleted_at.is_none_or(|d| stamp > d) {
                    self.deleted_at = Some(stamp);
                }
            }
        }
        lost
    }

    fn write(
        &mut self,
        name: &FieldName,
        stamp: Stamp,
        value: Option<&Value>,
        lost: &mut Vec<Superseded>,
    ) {
        let incoming = FieldState {
            stamp,
            value: value.cloned(),
        };
        match self.fields.get_mut(name) {
            None => {
                self.fields.insert(name.clone(), incoming);
            }
            Some(current) if stamp > current.stamp => {
                let old = std::mem::replace(current, incoming);
                lost.push(Superseded {
                    field: name.clone(),
                    winner: stamp,
                    loser: old,
                });
            }
            Some(current) if stamp < current.stamp => lost.push(Superseded {
                field: name.clone(),
                winner: current.stamp,
                loser: incoming,
            }),
            // Equal stamps carry equal values (stamps are unique per device); keep the state.
            Some(_) => {}
        }
    }

    /// Whether a field write is visible: newer than the delete stamp and not unset.
    fn is_visible(&self, f: &FieldState) -> bool {
        f.value.is_some() && self.deleted_at.is_none_or(|d| f.stamp > d)
    }

    /// Visible fields and their values.
    pub fn visible(&self) -> impl Iterator<Item = (&FieldName, &Value)> {
        self.fields
            .iter()
            .filter(|(_, f)| self.is_visible(f))
            .map(|(k, f)| (k, f.value.as_ref().expect("visible fields have a value")))
    }

    /// Whether the document exists: it has at least one visible field.
    pub fn exists(&self) -> bool {
        self.fields.values().any(|f| self.is_visible(f))
    }

    /// Largest stamp in the state, if any.
    pub fn max_stamp(&self) -> Option<Stamp> {
        self.fields
            .values()
            .map(|f| f.stamp)
            .chain(self.deleted_at)
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hlc;
    use crate::model::DeviceId;
    use proptest::prelude::*;

    fn s(hlc: u64, dev: u8) -> Stamp {
        Stamp::new(Hlc::from_u64(hlc), DeviceId::from_bytes([dev; 16]))
    }

    fn f(name: &str) -> FieldName {
        FieldName::new(name).unwrap()
    }

    fn set(pairs: &[(&str, i64)]) -> OpKind {
        OpKind::SetFields(pairs.iter().map(|(k, v)| (f(k), Value::Int(*v))).collect())
    }

    fn unset(names: &[&str]) -> OpKind {
        OpKind::UnsetFields(names.iter().map(|n| f(n)).collect())
    }

    fn visible(d: &DocState) -> Vec<(String, Value)> {
        d.visible()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn concurrent_edits_to_different_fields_both_survive() {
        let mut d = DocState::default();
        d.apply(&set(&[("title", 1)]), s(10, 1));
        d.apply(&set(&[("tags", 2)]), s(11, 2));
        assert_eq!(
            visible(&d),
            vec![
                ("tags".into(), Value::Int(2)),
                ("title".into(), Value::Int(1))
            ]
        );
    }

    #[test]
    fn same_field_higher_stamp_wins_and_loser_is_reported() {
        let mut d = DocState::default();
        assert!(d.apply(&set(&[("x", 1)]), s(20, 1)).is_empty());
        let lost = d.apply(&set(&[("x", 2)]), s(10, 2));
        assert_eq!(visible(&d), vec![("x".into(), Value::Int(1))]);
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].winner, s(20, 1));
        assert_eq!(lost[0].loser.value, Some(Value::Int(2)));
    }

    #[test]
    fn device_id_breaks_hlc_ties() {
        let mut d = DocState::default();
        d.apply(&set(&[("x", 2)]), s(10, 2));
        d.apply(&set(&[("x", 1)]), s(10, 1));
        assert_eq!(visible(&d), vec![("x".into(), Value::Int(2))]);
    }

    #[test]
    fn delete_hides_older_writes_even_if_they_arrive_late() {
        let mut d = DocState::default();
        d.apply(&OpKind::DeleteDoc, s(30, 1));
        d.apply(&set(&[("x", 1)]), s(20, 2));
        assert!(!d.exists());
    }

    #[test]
    fn write_after_delete_revives_only_new_fields() {
        let mut d = DocState::default();
        d.apply(&set(&[("a", 1), ("b", 2)]), s(10, 1));
        d.apply(&OpKind::DeleteDoc, s(20, 1));
        d.apply(&set(&[("b", 3)]), s(30, 2));
        assert_eq!(visible(&d), vec![("b".into(), Value::Int(3))]);
    }

    #[test]
    fn unset_removes_field_and_document_without_fields_does_not_exist() {
        let mut d = DocState::default();
        d.apply(&set(&[("a", 1)]), s(10, 1));
        d.apply(&unset(&["a"]), s(11, 1));
        assert!(!d.exists());
        assert_eq!(d.fields[&f("a")].value, None);
    }

    #[test]
    fn delete_keeps_the_largest_stamp() {
        let mut d = DocState::default();
        d.apply(&OpKind::DeleteDoc, s(30, 1));
        d.apply(&OpKind::DeleteDoc, s(20, 2));
        assert_eq!(d.deleted_at, Some(s(30, 1)));
        assert_eq!(d.max_stamp(), Some(s(30, 1)));
    }

    // Random ops from three devices. Each device's stamps are unique; values derive from the
    // stamp so that equal stamps always carry equal content, as the format guarantees.
    fn arb_ops() -> impl Strategy<Value = Vec<(OpKind, Stamp)>> {
        let fields = prop::sample::subsequence(vec!["a", "b", "c", "d"], 1..=3);
        let op = (0u8..3, fields, 0u8..3, 1u64..40);
        prop::collection::vec(op, 0..40).prop_map(|raw| {
            let mut seen = std::collections::BTreeSet::new();
            raw.into_iter()
                .filter(|(_, _, dev, hlc)| seen.insert((*hlc, *dev)))
                .map(|(kind, names, dev, hlc)| {
                    let stamp = s(hlc, dev);
                    let kind = match kind {
                        0 => OpKind::SetFields(
                            names
                                .iter()
                                .map(|n| (f(n), Value::Int((hlc * 10 + dev as u64) as i64)))
                                .collect(),
                        ),
                        1 => unset(&names),
                        _ => OpKind::DeleteDoc,
                    };
                    (kind, stamp)
                })
                .collect()
        })
    }

    fn apply_all<'a>(ops: impl IntoIterator<Item = &'a (OpKind, Stamp)>) -> DocState {
        let mut d = DocState::default();
        for (k, st) in ops {
            d.apply(k, *st);
        }
        d
    }

    proptest! {
        #[test]
        fn order_does_not_matter(ops in arb_ops(), seed in any::<u64>()) {
            let mut shuffled = ops.clone();
            // Deterministic Fisher-Yates driven by the seed.
            let mut x = seed | 1;
            for i in (1..shuffled.len()).rev() {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                shuffled.swap(i, (x % (i as u64 + 1)) as usize);
            }
            prop_assert_eq!(apply_all(&ops), apply_all(&shuffled));
        }

        #[test]
        fn applying_twice_changes_nothing(ops in arb_ops()) {
            let once = apply_all(&ops);
            let twice = apply_all(ops.iter().chain(ops.iter()));
            prop_assert_eq!(once, twice);
        }
    }
}
