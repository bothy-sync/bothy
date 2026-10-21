//! Identifiers: collection names, document ids, field names, device and database ids.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

const MAX_COLLECTION_LEN: usize = 64;
const MAX_DOC_ID_LEN: usize = 512;
const MAX_FIELD_LEN: usize = 256;

fn check_text(s: &str, what: &str, max: usize) -> Result<()> {
    if s.is_empty() || s.len() > max {
        return Err(Error::invalid_input(format!(
            "{what} must be 1 to {max} bytes"
        )));
    }
    if s.contains('\0') {
        return Err(Error::invalid_input(format!(
            "{what} must not contain U+0000"
        )));
    }
    Ok(())
}

macro_rules! text_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// The identifier as text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(s: String) -> Result<Self> {
                Self::validate(&s)?;
                Ok(Self(s))
            }
        }

        impl TryFrom<&str> for $name {
            type Error = Error;
            fn try_from(s: &str) -> Result<Self> {
                Self::new(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

text_id!(
    /// Collection name: `[a-z0-9_]{1,64}`. Names starting with `_bothy` are reserved.
    CollectionName
);

impl CollectionName {
    /// Validates and wraps a collection name.
    pub fn new(s: &str) -> Result<Self> {
        Self::validate(s)?;
        Ok(Self(s.to_owned()))
    }

    fn validate(s: &str) -> Result<()> {
        let ok = !s.is_empty()
            && s.len() <= MAX_COLLECTION_LEN
            && s.bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'));
        if ok {
            Ok(())
        } else {
            Err(Error::invalid_input(
                "collection name must match [a-z0-9_]{1,64}",
            ))
        }
    }

    /// Whether the name is reserved for Bothy's own use.
    pub fn is_reserved(&self) -> bool {
        self.0.starts_with("_bothy")
    }
}

text_id!(
    /// Document id: 1 to 512 bytes of UTF-8 without U+0000. Compared byte by byte.
    DocId
);

impl DocId {
    /// Validates and wraps a document id.
    pub fn new(s: &str) -> Result<Self> {
        Self::validate(s)?;
        Ok(Self(s.to_owned()))
    }

    fn validate(s: &str) -> Result<()> {
        check_text(s, "document id", MAX_DOC_ID_LEN)
    }
}

text_id!(
    /// Field name: 1 to 256 bytes of UTF-8 without U+0000. Names starting with `_` are reserved.
    FieldName
);

impl FieldName {
    /// Validates and wraps a field name.
    pub fn new(s: &str) -> Result<Self> {
        Self::validate(s)?;
        Ok(Self(s.to_owned()))
    }

    fn validate(s: &str) -> Result<()> {
        check_text(s, "field name", MAX_FIELD_LEN)
    }

    /// Whether the name is reserved for Bothy's own use.
    pub fn is_reserved(&self) -> bool {
        self.0.starts_with('_')
    }
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf).map_err(|e| {
        Error::new(crate::ErrorKind::Internal, "system random generator failed")
            .with_source(e.to_string())
    })
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

fn from_hex_16(s: &str) -> Option<[u8; 16]> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    let s = s.as_bytes();
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in s.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

macro_rules! byte_id {
    ($(#[$doc:meta])* $name:ident, $what:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 16]);

        impl $name {
            /// A new random id from the system CSPRNG.
            pub fn random() -> Result<Self> {
                let mut b = [0u8; 16];
                fill_random(&mut b)?;
                Ok(Self(b))
            }

            /// Wraps raw bytes.
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }

            /// The raw bytes.
            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }

            /// Lowercase hex form (32 characters).
            pub fn to_hex(&self) -> String {
                to_hex(&self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                from_hex_16(s).map(Self).ok_or_else(|| {
                    Error::invalid_input(concat!($what, " must be 32 lowercase hex characters"))
                })
            }
        }
    };
}

byte_id!(
    /// Device id: 16 random bytes, written as 32 lowercase hex characters.
    DeviceId,
    "device id"
);

byte_id!(
    /// Database id: 16 random bytes, written as 32 lowercase hex characters.
    DbId,
    "database id"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_names() {
        for ok in ["notes", "a", "todo_items_2", &"x".repeat(64), "_bothy_meta"] {
            assert!(CollectionName::new(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Notes", "a-b", "a b", "ü", &"x".repeat(65)] {
            assert!(CollectionName::new(bad).is_err(), "{bad}");
        }
        assert!(CollectionName::new("_bothy_meta").unwrap().is_reserved());
        assert!(!CollectionName::new("bothy").unwrap().is_reserved());
    }

    #[test]
    fn doc_ids_and_fields_are_length_checked_in_bytes() {
        assert!(DocId::new(&"é".repeat(256)).is_ok()); // 512 bytes
        assert!(DocId::new(&"é".repeat(257)).is_err());
        assert!(DocId::new("").is_err());
        assert!(DocId::new("a\0b").is_err());
        assert!(FieldName::new(&"x".repeat(256)).is_ok());
        assert!(FieldName::new(&"x".repeat(257)).is_err());
        assert!(FieldName::new("_meta").unwrap().is_reserved());
    }

    #[test]
    fn byte_ids_round_trip_through_hex() {
        let d = DeviceId::random().unwrap();
        assert_eq!(d.to_hex().len(), 32);
        assert_eq!(d.to_hex().parse::<DeviceId>().unwrap(), d);
        assert_ne!(DeviceId::random().unwrap(), d);
        assert!("0123".parse::<DbId>().is_err());
        assert!("0123456789ABCDEF0123456789abcdef".parse::<DbId>().is_err());
    }

    #[test]
    fn byte_ids_order_bytewise() {
        let a = DeviceId::from_bytes([0; 16]);
        let mut b = [0; 16];
        b[15] = 1;
        assert!(a < DeviceId::from_bytes(b));
    }
}
