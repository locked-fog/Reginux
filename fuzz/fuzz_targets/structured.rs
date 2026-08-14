#![no_main]

use libfuzzer_sys::fuzz_target;
use reginux_core::model::ValueType;
use reginux_core::structured;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    for format in ["kitty", "key_value", "toml", "ini", "kdl"] {
        let _ = structured::find_value(&text, "key", format);
        let _ = structured::replace_value(
            &text,
            "key",
            "value",
            format,
            &ValueType::String,
            Some("end"),
        );
        let _ = structured::remove_value(&text, "key", format);
    }
});
