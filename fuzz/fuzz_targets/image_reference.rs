#![no_main]

use libfuzzer_sys::fuzz_target;
use moraebox_image::ImageReference;

fuzz_target!(|data: &[u8]| {
    if let Ok(reference) = std::str::from_utf8(data) {
        let _ = reference.parse::<ImageReference>();
    }
});
