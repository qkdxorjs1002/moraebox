#![no_main]

use libfuzzer_sys::fuzz_target;
use moraebox_box::BoxStore;

fuzz_target!(|data: &[u8]| {
    let Ok(temporary) = tempfile::tempdir() else {
        return;
    };
    let bundle = temporary.path().join("input.tar");
    if std::fs::write(&bundle, data).is_err() {
        return;
    }
    let store = BoxStore::new(temporary.path().join("state"));
    let _ = store.import_bundle(&bundle);
});
