//! The reader never panics on any input.
#![no_main]

use bothy::codec::snapshot::{Expected, SnapshotReader};
use bothy::{DbId, DeviceId, Hlc};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 48 {
        return;
    }
    let expected = Expected {
        db_id: DbId::from_bytes(data[8..24].try_into().unwrap()),
        device_id: DeviceId::from_bytes(data[24..40].try_into().unwrap()),
        hlc: Hlc::from_u64(u64::from_be_bytes(data[40..48].try_into().unwrap())),
    };
    if let Ok(r) = SnapshotReader::open(data, &expected) {
        let _ = r.for_each_entry(|_, _, _| Ok(()));
    }
});
