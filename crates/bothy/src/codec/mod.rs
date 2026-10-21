//! Wire encodings and remote file formats.

pub mod cbor;
pub mod keys;
pub mod manifest;
mod op;
pub mod segment;
pub mod snapshot;
pub mod state;

pub use op::{decode_op, encode_op};

#[cfg(feature = "crypto")]
pub use crate::crypto::DataKey;

/// Stand-in for the data key when the `crypto` feature is off: it has no values, so every
/// `Option<&DataKey>` is `None` and encrypted files are rejected as unsupported.
#[cfg(not(feature = "crypto"))]
#[derive(Clone, Debug)]
pub enum DataKey {}
