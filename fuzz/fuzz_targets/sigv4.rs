// SPDX-License-Identifier: BUSL-1.1
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    deltaglider_proxy::fuzz_entry::sigv4(data);
});
