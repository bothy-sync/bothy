//! Parsing never panics, and accepted manifests survive a write/read round trip.
#![no_main]

use bothy::codec::manifest::Manifest;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = Manifest::from_json(data, None) {
        assert_eq!(Manifest::from_json(&m.to_json(), Some(m.db_id)).unwrap(), m);
    }
});
