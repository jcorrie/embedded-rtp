//! `rtcp::Compound::parse` on any bytes: it never panics, everything it
//! returns lies inside the input, and every packet it accepts, written
//! again with `rtcp::Writer`, parses to the same contents.

#![no_main]

use embedded_rtp::rtcp::{Compound, Packet, ReportBlock, Writer};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(compound) = Compound::parse(data) else {
        return;
    };
    let mut first = true;
    for packet in compound.packets() {
        if first {
            assert!(matches!(
                packet,
                Packet::SenderReport { .. } | Packet::ReceiverReport { .. }
            ));
            first = false;
        }
        check(data, &packet);

        let mut buf = vec![0u8; 2 * data.len() + 64];
        let rewritten = rewrite(&packet, &mut buf);
        let mut copies = Compound::parse(rewritten).unwrap().packets();
        match packet {
            Packet::SenderReport { .. } | Packet::ReceiverReport { .. } => {
                assert_eq!(copies.next(), Some(packet));
            }
            Packet::SourceDescription(chunks) => {
                copies.next();
                // One SDES packet a chunk.
                for chunk in chunks {
                    let Some(Packet::SourceDescription(mut one)) = copies.next() else {
                        panic!("a rewritten SDES chunk is missing");
                    };
                    let copy = one.next().unwrap();
                    assert_eq!(copy.ssrc, chunk.ssrc);
                    assert!(copy.items.eq(chunk.items));
                    assert!(one.next().is_none());
                }
            }
            Packet::Goodbye { .. } => {
                copies.next();
                assert_eq!(copies.next(), Some(packet));
            }
            Packet::Other { .. } => {
                copies.next();
            }
        }
        assert!(copies.next().is_none());
    }
    assert!(!first, "an accepted compound packet has packets");
});

/// Every slice the packet hands out lies inside the input.
fn check(data: &[u8], packet: &Packet<'_>) {
    match *packet {
        Packet::SenderReport { reports, .. } | Packet::ReceiverReport { reports, .. } => {
            assert_eq!(reports.len(), reports.count());
        }
        Packet::SourceDescription(chunks) => {
            for chunk in chunks {
                for item in chunk.items {
                    assert_ne!(item.kind, 0);
                    assert_inside(data, item.value);
                }
            }
        }
        Packet::Goodbye { sources, reason } => {
            assert_eq!(sources.len(), sources.count());
            if let Some(reason) = reason {
                assert_inside(data, reason);
            }
        }
        Packet::Other { body, count, .. } => {
            assert!(count <= 31);
            assert_inside(data, body);
        }
    }
}

/// `packet` written again with `Writer`, after an empty RR if it can't lead
/// a compound packet itself. Packets `Writer` has no method for come out
/// as just the RR.
fn rewrite<'b>(packet: &Packet<'_>, buf: &'b mut [u8]) -> &'b [u8] {
    let mut writer = Writer::new(buf);
    match *packet {
        Packet::SenderReport {
            ssrc,
            info,
            reports,
        } => {
            let blocks: Vec<ReportBlock> = reports.collect();
            writer.sender_report(ssrc, &info, &blocks).unwrap();
        }
        Packet::ReceiverReport { ssrc, reports } => {
            let blocks: Vec<ReportBlock> = reports.collect();
            writer.receiver_report(ssrc, &blocks).unwrap();
        }
        Packet::SourceDescription(chunks) => {
            writer.receiver_report(0, &[]).unwrap();
            for chunk in chunks {
                let items: Vec<(u8, &[u8])> =
                    chunk.items.map(|item| (item.kind, item.value)).collect();
                writer.source_description(chunk.ssrc, &items).unwrap();
            }
        }
        Packet::Goodbye { sources, reason } => {
            writer.receiver_report(0, &[]).unwrap();
            let sources: Vec<u32> = sources.collect();
            writer.goodbye(&sources, reason).unwrap();
        }
        Packet::Other { .. } => writer.receiver_report(0, &[]).unwrap(),
    }
    writer.finish()
}

/// `part` is a subslice of `whole`.
fn assert_inside(whole: &[u8], part: &[u8]) {
    let whole = whole.as_ptr_range();
    let part = part.as_ptr_range();
    assert!(whole.start <= part.start && part.end <= whole.end);
}
