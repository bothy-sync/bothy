//! Parsed keys print back to exactly the input.
#![no_main]

use bothy::codec::keys::LogicalKey;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data)
        && let Some(k) = LogicalKey::parse(s)
    {
        assert_eq!(k.to_string(), s);
    }
});
