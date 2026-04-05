#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = starla_measurements::dns::resolver::parse_response(data);
    if let Ok(b64) = std::str::from_utf8(data) {
        let _ = starla_measurements::dns::resolver::decode_answers(b64);
    }
});
