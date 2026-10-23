//! `packet::Packet::parse` on any bytes: it never panics, everything it
//! returns lies inside the input, and a parsed header writes back as the
//! input's first 12 bytes.

#![no_main]

use embedded_rtp::packet::{self, Packet, HEADER_LEN};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(parsed) = Packet::parse(data) else {
        return;
    };
    assert_inside(data, parsed.payload);
    if let Some(extension) = parsed.extension {
        assert_inside(data, extension.data);
        assert_eq!(extension.data.len() % 4, 0);
    }
    assert!(parsed.csrcs().count() <= 15);

    // The header writes back as it came in: V, M, PT, sequence, timestamp
    // and SSRC. A bare header has no P, X or CSRC count.
    let mut header = [0u8; HEADER_LEN];
    assert_eq!(parsed.header.write(&mut header), Ok(HEADER_LEN));
    assert_eq!(header[0], data[0] & 0xc0);
    assert_eq!(header[1..], data[1..HEADER_LEN]);

    // Rebuilt without padding, extension or CSRCs, it parses to the same
    // header and payload.
    let mut rebuilt = vec![0u8; HEADER_LEN + parsed.payload.len()];
    let len = packet::write(&parsed.header, parsed.payload, &mut rebuilt).unwrap();
    assert_eq!(len, rebuilt.len());
    let again = Packet::parse(&rebuilt).unwrap();
    assert_eq!(again.header, parsed.header);
    assert_eq!(again.payload, parsed.payload);
    assert_eq!(again.extension, None);
    assert_eq!(again.csrcs().count(), 0);
});

/// `part` is a subslice of `whole`.
fn assert_inside(whole: &[u8], part: &[u8]) {
    let whole = whole.as_ptr_range();
    let part = part.as_ptr_range();
    assert!(whole.start <= part.start && part.end <= whole.end);
}
