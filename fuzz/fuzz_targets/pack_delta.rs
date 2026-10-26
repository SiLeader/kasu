#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    kasu_fuzz::pack_delta(data);
});
