//! Key files, sealed JSON and encrypted segments: parsing and decryption never panic, and
//! accepted key files survive a write/read round trip.
#![no_main]

use bothy::codec::manifest::KeyFile;
use bothy::codec::segment::{self, Expected};
use bothy::crypto::{self, DataKey};
use bothy::{DbId, DeviceId};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(f) = KeyFile::from_json(data) {
        assert_eq!(KeyFile::from_json(&f.to_json()).unwrap(), f);
    }
    let key = DataKey::from_bytes([7; 32]);
    let _ = crypto::open_json(&key, "db/devices/x/device.json", data);
    if data.len() >= 48 {
        let expected = Expected {
            db_id: DbId::from_bytes(data[8..24].try_into().unwrap()),
            device_id: DeviceId::from_bytes(data[24..40].try_into().unwrap()),
            seq: u64::from_be_bytes(data[40..48].try_into().unwrap()),
        };
        let _ = segment::decode_with(data, &expected, Some(&key));
    }
});
