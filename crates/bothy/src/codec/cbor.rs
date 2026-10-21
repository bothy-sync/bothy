//! Canonical CBOR (RFC 8949 §4.2.1) encoder and strict decoder.
//!
//! The decoder rejects anything the encoder would not produce: non-shortest heads, indefinite
//! lengths, tags, unsorted or duplicate map keys, non-shortest floats, NaN, infinities, `-0.0`
//! and simple values other than false, true and null (`undefined` only where a caller asks).

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::model::{MAX_DEPTH, Value};

const MAJOR_UINT: u8 = 0;
const MAJOR_NINT: u8 = 1;
const MAJOR_BYTES: u8 = 2;
const MAJOR_TEXT: u8 = 3;
const MAJOR_ARRAY: u8 = 4;
const MAJOR_MAP: u8 = 5;
const MAJOR_TAG: u8 = 6;
const MAJOR_SIMPLE: u8 = 7;

const FALSE: u8 = 0xf4;
const TRUE: u8 = 0xf5;
const NULL: u8 = 0xf6;
const UNDEFINED: u8 = 0xf7;

fn corrupt(msg: &'static str) -> Error {
    Error::corrupt(format!("cbor: {msg}"))
}

/// Appends canonical CBOR to a byte buffer.
#[derive(Debug, Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    /// An empty encoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// The encoded bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    fn head(&mut self, major: u8, arg: u64) {
        let m = major << 5;
        if arg < 24 {
            self.buf.push(m | arg as u8);
        } else if arg <= u8::MAX as u64 {
            self.buf.extend_from_slice(&[m | 24, arg as u8]);
        } else if arg <= u16::MAX as u64 {
            self.buf.push(m | 25);
            self.buf.extend_from_slice(&(arg as u16).to_be_bytes());
        } else if arg <= u32::MAX as u64 {
            self.buf.push(m | 26);
            self.buf.extend_from_slice(&(arg as u32).to_be_bytes());
        } else {
            self.buf.push(m | 27);
            self.buf.extend_from_slice(&arg.to_be_bytes());
        }
    }

    /// Unsigned integer.
    pub fn uint(&mut self, v: u64) {
        self.head(MAJOR_UINT, v);
    }

    /// Signed integer.
    pub fn int(&mut self, v: i64) {
        if v >= 0 {
            self.head(MAJOR_UINT, v as u64);
        } else {
            self.head(MAJOR_NINT, !(v as u64));
        }
    }

    /// Byte string.
    pub fn bytes(&mut self, v: &[u8]) {
        self.head(MAJOR_BYTES, v.len() as u64);
        self.buf.extend_from_slice(v);
    }

    /// Text string.
    pub fn text(&mut self, v: &str) {
        self.head(MAJOR_TEXT, v.len() as u64);
        self.buf.extend_from_slice(v.as_bytes());
    }

    /// Array header; the caller writes `len` items next.
    pub fn array(&mut self, len: usize) {
        self.head(MAJOR_ARRAY, len as u64);
    }

    /// Map header; the caller writes `len` sorted key/value pairs next.
    pub fn map(&mut self, len: usize) {
        self.head(MAJOR_MAP, len as u64);
    }

    /// `null`.
    pub fn null(&mut self) {
        self.buf.push(NULL);
    }

    /// `undefined`; only valid where the format allows it.
    pub fn undefined(&mut self) {
        self.buf.push(UNDEFINED);
    }

    /// Boolean.
    pub fn bool(&mut self, v: bool) {
        self.buf.push(if v { TRUE } else { FALSE });
    }

    /// Float in its shortest lossless width. Fails for NaN and infinities; `-0.0` becomes `0.0`.
    pub fn float(&mut self, v: f64) -> Result<()> {
        if !v.is_finite() {
            return Err(Error::invalid_input("float must be finite"));
        }
        let v = if v == 0.0 { 0.0 } else { v };
        if let Some(h) = f16_exact(v) {
            self.buf.push(0xf9);
            self.buf.extend_from_slice(&h.to_be_bytes());
        } else if f32_exact(v) {
            self.buf.push(0xfa);
            self.buf
                .extend_from_slice(&(v as f32).to_bits().to_be_bytes());
        } else {
            self.buf.push(0xfb);
            self.buf.extend_from_slice(&v.to_bits().to_be_bytes());
        }
        Ok(())
    }

    /// Raw bytes that are already canonical CBOR.
    pub fn raw(&mut self, encoded: &[u8]) {
        self.buf.extend_from_slice(encoded);
    }

    /// A document value. Map keys are sorted by their encoded form.
    pub fn value(&mut self, v: &Value) -> Result<()> {
        self.value_at(v, 0)
    }

    fn value_at(&mut self, v: &Value, depth: usize) -> Result<()> {
        match v {
            Value::Null => self.null(),
            Value::Bool(b) => self.bool(*b),
            Value::Int(i) => self.int(*i),
            Value::Float(f) => self.float(*f)?,
            Value::Text(s) => self.text(s),
            Value::Bytes(b) => self.bytes(b),
            Value::Array(items) => {
                check_encode_depth(depth)?;
                self.array(items.len());
                for item in items {
                    self.value_at(item, depth + 1)?;
                }
            }
            Value::Map(entries) => {
                check_encode_depth(depth)?;
                self.map(entries.len());
                for (key, item) in sorted_text_keys(entries.iter()) {
                    self.raw(&key);
                    self.value_at(item, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

/// Pairs `(encoded key, value)` in canonical order: by encoded key bytes.
pub(crate) fn sorted_text_keys<'v, K: AsRef<str> + 'v, V: 'v>(
    entries: impl Iterator<Item = (&'v K, &'v V)>,
) -> Vec<(Vec<u8>, &'v V)> {
    let mut out: Vec<(Vec<u8>, &V)> = entries
        .map(|(k, v)| {
            let mut e = Encoder::new();
            e.text(k.as_ref());
            (e.into_bytes(), v)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn check_encode_depth(depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        Err(Error::invalid_input("value nesting is deeper than 32"))
    } else {
        Ok(())
    }
}

fn check_depth(depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        Err(corrupt("value nesting is deeper than 32"))
    } else {
        Ok(())
    }
}

/// Encodes one value as a standalone canonical CBOR item.
pub fn encode_value(v: &Value) -> Result<Vec<u8>> {
    let mut e = Encoder::new();
    e.value(v)?;
    Ok(e.into_bytes())
}

/// Decodes one standalone canonical CBOR value; trailing bytes are an error.
pub fn decode_value(data: &[u8]) -> Result<Value> {
    let mut d = Decoder::new(data);
    let v = d.value()?;
    d.finish()?;
    Ok(v)
}

/// Reads canonical CBOR from a byte slice, rejecting non-canonical input.
#[derive(Debug, Clone)]
pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    /// A decoder over `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Current read offset.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Fails unless every byte was consumed.
    pub fn finish(&self) -> Result<()> {
        if self.pos == self.data.len() {
            Ok(())
        } else {
            Err(corrupt("trailing bytes"))
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.data.len())
            .ok_or_else(|| corrupt("unexpected end of input"))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn peek(&self) -> Result<u8> {
        self.data
            .get(self.pos)
            .copied()
            .ok_or_else(|| corrupt("unexpected end of input"))
    }

    /// Major type of the next item.
    pub fn peek_major(&self) -> Result<u8> {
        Ok(self.peek()? >> 5)
    }

    /// Reads a head of major types 0 to 6 with a shortest-form argument.
    fn head(&mut self) -> Result<(u8, u64)> {
        let b = self.take(1)?[0];
        let (major, ai) = (b >> 5, b & 0x1f);
        if major == MAJOR_SIMPLE {
            return Err(corrupt("unexpected simple value or float"));
        }
        let arg = match ai {
            0..=23 => ai as u64,
            24 => {
                let v = self.take(1)?[0] as u64;
                if v < 24 {
                    return Err(corrupt("non-shortest integer or length"));
                }
                v
            }
            25 => {
                let v = u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes")) as u64;
                if v <= u8::MAX as u64 {
                    return Err(corrupt("non-shortest integer or length"));
                }
                v
            }
            26 => {
                let v = u32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")) as u64;
                if v <= u16::MAX as u64 {
                    return Err(corrupt("non-shortest integer or length"));
                }
                v
            }
            27 => {
                let v = u64::from_be_bytes(self.take(8)?.try_into().expect("8 bytes"));
                if v <= u32::MAX as u64 {
                    return Err(corrupt("non-shortest integer or length"));
                }
                v
            }
            31 => return Err(corrupt("indefinite length is not allowed")),
            _ => return Err(corrupt("reserved additional information")),
        };
        if major == MAJOR_TAG {
            return Err(corrupt("tags are not allowed"));
        }
        Ok((major, arg))
    }

    fn expect_head(&mut self, major: u8, what: &'static str) -> Result<u64> {
        let (m, arg) = self.head()?;
        if m != major {
            return Err(corrupt(what));
        }
        Ok(arg)
    }

    /// Unsigned integer.
    pub fn uint(&mut self) -> Result<u64> {
        self.expect_head(MAJOR_UINT, "expected unsigned integer")
    }

    /// Signed integer in the 64-bit range.
    pub fn int(&mut self) -> Result<i64> {
        match self.head()? {
            (MAJOR_UINT, v) => i64::try_from(v).map_err(|_| corrupt("integer out of range")),
            (MAJOR_NINT, v) => i64::try_from(v)
                .map(|v| -1 - v)
                .map_err(|_| corrupt("integer out of range")),
            _ => Err(corrupt("expected integer")),
        }
    }

    fn len(&mut self, major: u8, what: &'static str) -> Result<usize> {
        let n = self.expect_head(major, what)?;
        usize::try_from(n).map_err(|_| corrupt("length out of range"))
    }

    /// Byte string.
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.len(MAJOR_BYTES, "expected byte string")?;
        self.take(n)
    }

    /// Text string; must be valid UTF-8.
    pub fn text(&mut self) -> Result<&'a str> {
        let n = self.len(MAJOR_TEXT, "expected text string")?;
        std::str::from_utf8(self.take(n)?).map_err(|_| corrupt("invalid UTF-8"))
    }

    /// Array header; returns the item count.
    pub fn array(&mut self) -> Result<usize> {
        let n = self.len(MAJOR_ARRAY, "expected array")?;
        if n > self.remaining() {
            return Err(corrupt("array longer than input"));
        }
        Ok(n)
    }

    /// Map header; returns the pair count.
    pub fn map(&mut self) -> Result<usize> {
        let n = self.len(MAJOR_MAP, "expected map")?;
        if n > self.remaining() / 2 {
            return Err(corrupt("map longer than input"));
        }
        Ok(n)
    }

    /// A map key that must be text and sort strictly after `prev` (the previous key's
    /// encoding). Returns the key and its encoding for the next call.
    pub fn map_text_key(&mut self, prev: Option<&[u8]>) -> Result<(&'a str, &'a [u8])> {
        let start = self.pos;
        let key = self.text()?;
        let encoded = &self.data[start..self.pos];
        if prev.is_some_and(|p| p >= encoded) {
            return Err(corrupt("map keys are not in canonical order or repeat"));
        }
        Ok((key, encoded))
    }

    /// A map key that must be a byte string and sort strictly after `prev` (the previous
    /// key's encoding). Returns the key and its encoding for the next call.
    pub fn map_bytes_key(&mut self, prev: Option<&[u8]>) -> Result<(&'a [u8], &'a [u8])> {
        let start = self.pos;
        let key = self.bytes()?;
        let encoded = &self.data[start..self.pos];
        if prev.is_some_and(|p| p >= encoded) {
            return Err(corrupt("map keys are not in canonical order or repeat"));
        }
        Ok((key, encoded))
    }

    /// Consumes `null` if it is next.
    pub fn null_if_present(&mut self) -> Result<bool> {
        if self.peek()? == NULL {
            self.pos += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// `null`.
    pub fn null(&mut self) -> Result<()> {
        if self.null_if_present()? {
            Ok(())
        } else {
            Err(corrupt("expected null"))
        }
    }

    /// Consumes `undefined` if it is next.
    pub fn undefined_if_present(&mut self) -> Result<bool> {
        if self.peek()? == UNDEFINED {
            self.pos += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// A document value.
    pub fn value(&mut self) -> Result<Value> {
        self.value_at(0)
    }

    fn value_at(&mut self, depth: usize) -> Result<Value> {
        let b = self.peek()?;
        if b >> 5 == MAJOR_SIMPLE {
            return self.simple();
        }
        Ok(match b >> 5 {
            MAJOR_UINT | MAJOR_NINT => Value::Int(self.int()?),
            MAJOR_BYTES => Value::Bytes(self.bytes()?.to_vec()),
            MAJOR_TEXT => Value::Text(self.text()?.to_owned()),
            MAJOR_ARRAY => {
                check_depth(depth)?;
                let n = self.array()?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.value_at(depth + 1)?);
                }
                Value::Array(items)
            }
            MAJOR_MAP => {
                check_depth(depth)?;
                let n = self.map()?;
                let mut entries = BTreeMap::new();
                let mut prev = None;
                for _ in 0..n {
                    let (key, encoded) = self.map_text_key(prev)?;
                    prev = Some(encoded);
                    entries.insert(key.to_owned(), self.value_at(depth + 1)?);
                }
                Value::Map(entries)
            }
            _ => return Err(corrupt("tags are not allowed")),
        })
    }

    fn simple(&mut self) -> Result<Value> {
        let b = self.take(1)?[0];
        let f = match b {
            FALSE => return Ok(Value::Bool(false)),
            TRUE => return Ok(Value::Bool(true)),
            NULL => return Ok(Value::Null),
            UNDEFINED => return Err(corrupt("undefined is not a value")),
            0xf9 => {
                let h = u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes"));
                f16_to_f64(h)
            }
            0xfa => {
                let f = f32::from_bits(u32::from_be_bytes(
                    self.take(4)?.try_into().expect("4 bytes"),
                )) as f64;
                if f.is_finite() && f16_exact(f).is_some() {
                    return Err(corrupt("float is not in its shortest form"));
                }
                f
            }
            0xfb => {
                let f = f64::from_bits(u64::from_be_bytes(
                    self.take(8)?.try_into().expect("8 bytes"),
                ));
                if f.is_finite() && f32_exact(f) {
                    return Err(corrupt("float is not in its shortest form"));
                }
                f
            }
            _ => return Err(corrupt("simple value is not allowed")),
        };
        if !f.is_finite() {
            return Err(corrupt("NaN and infinity are not allowed"));
        }
        if f == 0.0 && f.is_sign_negative() {
            return Err(corrupt("-0.0 is not allowed"));
        }
        Ok(Value::Float(f))
    }
}

fn f32_exact(v: f64) -> bool {
    (v as f32) as f64 == v
}

/// The IEEE half-precision bits of `v` if `v` is exactly representable.
fn f16_exact(v: f64) -> Option<u16> {
    if !f32_exact(v) {
        return None;
    }
    let bits = (v as f32).to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x007f_ffff;
    if exp == 0 {
        // Zero, or an f32 subnormal (far below the smallest f16 subnormal).
        return (mant == 0).then_some(sign);
    }
    if exp == 0xff {
        return (mant == 0).then_some(sign | 0x7c00);
    }
    let e = exp - 127;
    if (-14..=15).contains(&e) {
        // f16 normal: the low 13 mantissa bits must be zero.
        if mant & 0x1fff != 0 {
            return None;
        }
        return Some(sign | (((e + 15) as u16) << 10) | (mant >> 13) as u16);
    }
    if (-24..=-15).contains(&e) {
        // f16 subnormal: value = k * 2^-24 with k = (2^23 + mant) >> -(e + 1).
        let full = 0x0080_0000 | mant;
        let shift = (-(e + 1)) as u32;
        if full & ((1 << shift) - 1) != 0 {
            return None;
        }
        return Some(sign | (full >> shift) as u16);
    }
    None
}

fn f16_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let mant = (h & 0x03ff) as f64;
    let mag = match exp {
        0 => mant * 2f64.powi(-24),
        31 if mant == 0.0 => f64::INFINITY,
        31 => f64::NAN,
        _ => (1024.0 + mant) * 2f64.powi(exp - 25),
    };
    sign * mag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn enc(v: &Value) -> String {
        crate::model::to_hex(&encode_value(v).unwrap())
    }

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    // RFC 8949 Appendix A examples that fit the value model.
    #[test]
    fn rfc_8949_examples_round_trip() {
        let cases: Vec<(Value, &str)> = vec![
            (Value::Int(0), "00"),
            (Value::Int(1), "01"),
            (Value::Int(10), "0a"),
            (Value::Int(23), "17"),
            (Value::Int(24), "1818"),
            (Value::Int(25), "1819"),
            (Value::Int(100), "1864"),
            (Value::Int(1000), "1903e8"),
            (Value::Int(1_000_000), "1a000f4240"),
            (Value::Int(1_000_000_000_000), "1b000000e8d4a51000"),
            (Value::Int(i64::MAX), "1b7fffffffffffffff"),
            (Value::Int(i64::MIN), "3b7fffffffffffffff"),
            (Value::Int(-1), "20"),
            (Value::Int(-10), "29"),
            (Value::Int(-100), "3863"),
            (Value::Int(-1000), "3903e7"),
            (Value::Float(0.0), "f90000"),
            (Value::Float(1.0), "f93c00"),
            (Value::Float(1.1), "fb3ff199999999999a"),
            (Value::Float(1.5), "f93e00"),
            (Value::Float(65504.0), "f97bff"),
            (Value::Float(100000.0), "fa47c35000"),
            (Value::Float(3.4028234663852886e38), "fa7f7fffff"),
            (Value::Float(1.0e300), "fb7e37e43c8800759c"),
            (Value::Float(5.960464477539063e-8), "f90001"),
            (Value::Float(0.00006103515625), "f90400"),
            (Value::Float(-4.0), "f9c400"),
            (Value::Float(-4.1), "fbc010666666666666"),
            (Value::Bool(false), "f4"),
            (Value::Bool(true), "f5"),
            (Value::Null, "f6"),
            (Value::Bytes(vec![]), "40"),
            (Value::Bytes(vec![1, 2, 3, 4]), "4401020304"),
            (Value::Text(String::new()), "60"),
            (Value::from("a"), "6161"),
            (Value::from("IETF"), "6449455446"),
            (Value::from("\"\\"), "62225c"),
            (Value::from("ü"), "62c3bc"),
            (Value::from("水"), "63e6b0b4"),
            (Value::Array(vec![]), "80"),
            (
                Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
                "83010203",
            ),
            (
                Value::Array(vec![
                    Value::Int(1),
                    Value::Array(vec![Value::Int(2), Value::Int(3)]),
                    Value::Array(vec![Value::Int(4), Value::Int(5)]),
                ]),
                "8301820203820405",
            ),
            (map(&[]), "a0"),
            (
                map(&[
                    ("a", Value::Int(1)),
                    ("b", Value::Array(vec![Value::Int(2), Value::Int(3)])),
                ]),
                "a26161016162820203",
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(enc(&value), expected, "{value:?}");
            assert_eq!(decode_value(&hex(expected)).unwrap(), value, "{expected}");
        }
    }

    #[test]
    fn map_keys_sort_by_encoded_form_shorter_first() {
        let v = map(&[
            ("bb", Value::Int(1)),
            ("c", Value::Int(2)),
            ("aaa", Value::Int(3)),
        ]);
        // "c" (1 byte) < "bb" (2 bytes) < "aaa" (3 bytes), not alphabetical.
        assert_eq!(enc(&v), "a3616302626262016361616103");
    }

    #[test]
    fn negative_zero_is_written_as_zero() {
        assert_eq!(enc(&Value::Float(-0.0)), "f90000");
    }

    #[test]
    fn non_finite_floats_are_not_encoded() {
        assert!(encode_value(&Value::Float(f64::NAN)).is_err());
        assert!(encode_value(&Value::Float(f64::NEG_INFINITY)).is_err());
    }

    #[test]
    fn f16_subnormals_and_limits() {
        for (f, h) in [
            (2f64.powi(-24), 0x0001u16),
            (2f64.powi(-15), 0x0200),
            (1023.0 * 2f64.powi(-24), 0x03ff),
            (2f64.powi(-14), 0x0400),
            (-65504.0, 0xfbff),
        ] {
            assert_eq!(f16_exact(f), Some(h), "{f}");
            assert_eq!(f16_to_f64(h), f);
        }
        assert_eq!(f16_exact(2f64.powi(-25)), None);
        assert_eq!(f16_exact(65520.0), None);
        assert_eq!(f16_exact(1.0 + 2f64.powi(-11)), None);
    }

    #[test]
    fn rejects_non_canonical_and_disallowed_input() {
        for (bad, why) in [
            ("1817", "non-shortest uint8"),
            ("190017", "non-shortest uint16"),
            ("1a0000ffff", "non-shortest uint32"),
            ("1b00000000ffffffff", "non-shortest uint64"),
            ("3817", "non-shortest negative"),
            ("5801ff", "non-shortest length"),
            ("1c", "reserved additional info"),
            ("9fff", "indefinite array"),
            ("7f6161ff", "indefinite text"),
            ("c000", "tag"),
            ("f7", "undefined"),
            ("f0", "simple value 16"),
            ("f818", "simple value 24"),
            ("f97e00", "NaN"),
            ("f97c00", "infinity"),
            ("f98000", "negative zero"),
            ("fa3f800000", "1.0 as f32"),
            ("fb3ff0000000000000", "1.0 as f64"),
            ("fb47efffffe0000000", "f32 max as f64"),
            ("a2616201616102", "unsorted keys"),
            ("a2616101616102", "duplicate keys"),
            ("a1010102", "integer key"),
            ("1bffffffffffffffff", "uint above i64::MAX"),
            ("3b8000000000000000", "negative below i64::MIN"),
            ("62c328", "invalid UTF-8"),
            ("1901", "truncated"),
            ("9a7fffffff", "array longer than input"),
            ("0000", "trailing bytes"),
        ] {
            let err = decode_value(&hex(bad)).expect_err(why);
            assert_eq!(err.kind(), crate::ErrorKind::Corrupt, "{why}");
        }
    }

    #[test]
    fn depth_limit_applies_on_both_sides() {
        let deep = |n: usize| (0..n).fold(Value::Null, |v, _| Value::Array(vec![v]));
        let ok = encode_value(&deep(MAX_DEPTH)).unwrap();
        assert_eq!(decode_value(&ok).unwrap(), deep(MAX_DEPTH));
        assert!(encode_value(&deep(MAX_DEPTH + 1)).is_err());
        let mut too_deep = vec![0x81; MAX_DEPTH + 1];
        too_deep.push(0xf6);
        assert!(decode_value(&too_deep).is_err());
    }
}
