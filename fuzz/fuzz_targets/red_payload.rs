//! `red::Payload::parse` on any bytes: it never panics, everything it
//! returns lies inside the input, and written back with `red::write` it is
//! the input byte for byte (every bit of the format is a field).

#![no_main]

use embedded_rtp::red::{self, Block, Payload, BLOCK_HEADER_LEN};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(parsed) = Payload::parse(data) else {
        return;
    };
    assert_inside(data, parsed.primary);
    let blocks: Vec<Block> = parsed.blocks().collect();
    let mut len = 1 + parsed.primary.len();
    for block in &blocks {
        assert_inside(data, block.data);
        len += BLOCK_HEADER_LEN + block.data.len();
    }
    assert_eq!(len, data.len());

    let mut rebuilt = vec![0u8; data.len()];
    assert_eq!(
        red::write(&blocks, parsed.payload_type, parsed.primary, &mut rebuilt),
        Ok(data.len())
    );
    assert_eq!(rebuilt, data);
});

/// `part` is a subslice of `whole`.
fn assert_inside(whole: &[u8], part: &[u8]) {
    let whole = whole.as_ptr_range();
    let part = part.as_ptr_range();
    assert!(whole.start <= part.start && part.end <= whole.end);
}
