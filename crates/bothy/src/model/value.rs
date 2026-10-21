//! Document values.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// Maximum array/map nesting depth of a value.
pub const MAX_DEPTH: usize = 32;

/// A JSON-like document value.
///
/// `Int` and `Float` are distinct: `1` and `1.0` never compare equal and are never converted.
/// Floats must be finite; `-0.0` is written as `0.0`.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Null.
    Null,
    /// Boolean.
    Bool(bool),
    /// Signed 64-bit integer.
    Int(i64),
    /// Finite 64-bit float.
    Float(f64),
    /// UTF-8 text.
    Text(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
    /// Ordered list.
    Array(Vec<Value>),
    /// Map with text keys.
    Map(BTreeMap<String, Value>),
}

impl Value {
    /// A float value; fails for NaN and infinities and turns `-0.0` into `0.0`.
    pub fn float(f: f64) -> Result<Value> {
        if !f.is_finite() {
            return Err(Error::invalid_input("float must be finite"));
        }
        Ok(Value::Float(if f == 0.0 { 0.0 } else { f }))
    }

    /// Checks the rules a stored value must follow: finite floats and nesting depth.
    pub fn validate(&self) -> Result<()> {
        self.validate_at(0)
    }

    fn validate_at(&self, depth: usize) -> Result<()> {
        match self {
            Value::Float(f) if !f.is_finite() => Err(Error::invalid_input("float must be finite")),
            Value::Array(items) => {
                check_depth(depth)?;
                items.iter().try_for_each(|v| v.validate_at(depth + 1))
            }
            Value::Map(entries) => {
                check_depth(depth)?;
                entries.values().try_for_each(|v| v.validate_at(depth + 1))
            }
            _ => Ok(()),
        }
    }
}

fn check_depth(depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        Err(Error::invalid_input("value nesting is deeper than 32"))
    } else {
        Ok(())
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Bytes(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested(depth: usize) -> Value {
        (0..depth).fold(Value::Null, |v, _| Value::Array(vec![v]))
    }

    #[test]
    fn float_rejects_non_finite_and_normalises_negative_zero() {
        assert!(Value::float(f64::NAN).is_err());
        assert!(Value::float(f64::INFINITY).is_err());
        let Value::Float(z) = Value::float(-0.0).unwrap() else {
            unreachable!()
        };
        assert!(z.is_sign_positive());
    }

    #[test]
    fn int_and_float_are_distinct() {
        assert_ne!(Value::Int(1), Value::Float(1.0));
    }

    #[test]
    fn depth_limit() {
        assert!(nested(MAX_DEPTH).validate().is_ok());
        assert!(nested(MAX_DEPTH + 1).validate().is_err());
    }

    #[test]
    fn validate_finds_nan_inside_containers() {
        let mut m = BTreeMap::new();
        m.insert("x".to_owned(), Value::Array(vec![Value::Float(f64::NAN)]));
        assert!(Value::Map(m).validate().is_err());
    }
}
