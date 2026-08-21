#![no_main]

use libfuzzer_sys::fuzz_target;
use moraebox_protocol::{decode_frame, validate_guest_path};

fuzz_target!(|data: &[u8]| {
    let _ = decode_frame(data);
    if let Ok(path) = std::str::from_utf8(data) {
        let _ = validate_guest_path(path);
    }
});
