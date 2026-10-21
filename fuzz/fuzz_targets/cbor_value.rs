//! Accepted input is canonical: re-encoding gives the same bytes.
#![no_main]

use bothy::codec::cbor::{decode_value, encode_value};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(v) = decode_value(data) {
        assert_eq!(encode_value(&v).unwrap(), data);
    }
});
