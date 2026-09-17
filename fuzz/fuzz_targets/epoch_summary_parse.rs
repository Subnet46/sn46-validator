#![no_main]

use libfuzzer_sys::fuzz_target;

// The fixture's binding. Seed from the fixtures directory; libFuzzer only writes to the first
// corpus directory: cargo +nightly fuzz run epoch_summary_parse fuzz/corpus/epoch_summary_parse tests/fixtures
fuzz_target!(|data: &[u8]| {
    let _ = sn46_shared::epoch_summary::parse_epoch_summary(
        data,
        "local",
        46,
        "5DAAnrj7VHTznn2AWBemMuyBwZWs6FNFjdyVXUeYum3PTXFy",
    );
});
