#![no_main]
use libfuzzer_sys::fuzz_target;
use panda_providers::Stream;
fuzz_target!(|input: &[u8]| {
    fn decode(bytes: &[u8], width: usize) -> Option<(String, Vec<panda_core::Call>)> {
        let mut s = Stream::default();
        for chunk in bytes.chunks(width) {
            s.push(chunk, &mut |_| {}).ok()?
        }
        s.finish().ok()
    }
    let width = input.first().copied().unwrap_or(0) as usize + 1;
    assert_eq!(decode(input, width), decode(input, input.len().max(1)));
});
