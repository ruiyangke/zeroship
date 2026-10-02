#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The crate's frame decoder over a socket-free byte slice. Decoding errors
    // are ordinary; a panic is the finding.
    compio_postgres::test_utils::decode_backend_frames(data);
});
