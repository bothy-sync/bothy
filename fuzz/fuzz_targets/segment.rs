//! The reader never panics; accepted uncompressed segments re-encode to the same bytes.
#![no_main]

use bothy::codec::segment::{self, Expected};
use bothy::{DbId, DeviceId};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 48 {
        return;
    }
    // Take the expected ids from the header so the fuzzer reaches the later checks.
    let expected = Expected {
        db_id: DbId::from_bytes(data[8..24].try_into().unwrap()),
        device_id: DeviceId::from_bytes(data[24..40].try_into().unwrap()),
        seq: u64::from_be_bytes(data[40..48].try_into().unwrap()),
    };
    if let Ok(seg) = segment::decode(data, &expected)
        && data[7] & 1 == 0
    {
        assert_eq!(segment::encode(&seg, false).unwrap(), data);
    }
});
