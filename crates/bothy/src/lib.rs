//! # Bothy
//!
//! Bothy is a local-first sync engine for applications that have no server of their own.
//! Each device keeps its data in a local SQLite database, and devices exchange immutable,
//! append-only files through the user's own cloud storage (Google Drive first).
//!
//! **Status: early development.** This crate currently contains the data model
//! ([`Value`], [`Op`]) and the
//! [`Hlc`] (hybrid logical clock) that orders every change. The local store, the sync engine,
//! the Drive backend and the CLI are being built at <https://github.com/bothy-sync/bothy>.
//!
//! ```
//! use bothy::{Hlc, HlcClock};
//!
//! let mut clock = HlcClock::new();
//! let a = clock.now(1_700_000_000_000);
//! let b = clock.now(1_700_000_000_000); // same millisecond
//! assert!(b > a);
//! assert_eq!(b.to_hex().parse::<Hlc>().unwrap(), b);
//! ```

#![forbid(unsafe_code)]

mod error;
mod hlc;
pub mod model;

pub use error::{Error, ErrorKind, Result};
pub use hlc::{Clock, DEFAULT_MAX_DRIFT_MS, Hlc, HlcClock, ParseHlcError, SystemClock};
pub use model::{CollectionName, DbId, DeviceId, DocId, FieldName, Op, OpKind, Stamp, Value};

/// Version of the on-remote file format written by this crate (`bothy_format` in
/// `manifest.json`). Draft: the format is not frozen until `SPEC.md` v1.
pub const FORMAT_VERSION: u16 = 1;
