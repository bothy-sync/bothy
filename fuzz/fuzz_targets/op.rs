//! Accepted ops re-encode to the same bytes.
#![no_main]

use bothy::codec::cbor::{Decoder, Encoder};
use bothy::codec::{decode_op, encode_op};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut d = Decoder::new(data);
    if let Ok(op) = decode_op(&mut d)
        && d.finish().is_ok()
    {
        let mut e = Encoder::new();
        encode_op(&mut e, &op).unwrap();
        assert_eq!(e.into_bytes(), data);
    }
});
