#![no_main]

use libfuzzer_sys::fuzz_target;

include!("../../tests/common/invariants.rs");

fuzz_target!(|data: &[u8]| {
    record(data);
});
