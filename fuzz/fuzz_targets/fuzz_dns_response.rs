#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = starla_measurements::dns::resolver::parse_response(data);
    let _ = starla_measurements::dns::resolver::format_result(data);
});
