#![no_main]
use libfuzzer_sys::fuzz_target;
use panda_core::Call;
use panda_providers::wire::Responses;

fn decode(bytes: &[u8], width: usize) -> Option<(String, Vec<Call>)> {
    let mut response = Responses::default();
    for chunk in bytes.chunks(width) {
        response.push(chunk, &mut |_| {}).ok()?;
    }
    response.finish().ok()
}

fuzz_target!(|bytes: &[u8]| {
    let width = usize::from(bytes.last().copied().unwrap_or_default()) + 1;

    assert_eq!(decode(bytes, width), decode(bytes, bytes.len().max(1)));
});
