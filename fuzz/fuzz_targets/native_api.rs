#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    kasu_fuzz::api::native_api(data);
});
