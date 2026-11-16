//! `$batch` bodies. The first byte picks the format, the rest is the body.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&format, body)) = data.split_first() else {
        return;
    };
    let content_type = match format % 3 {
        0 => "multipart/mixed; boundary=b",
        1 => "application/json",
        _ => "multipart/mixed; boundary=\"batch_36522ad7\"",
    };
    let _ = odatabase::parse_batch_for_fuzzing(content_type, body);
});
