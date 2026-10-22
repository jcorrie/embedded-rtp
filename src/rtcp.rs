//! RTCP (RFC 3550 §6): sender and receiver reports, source descriptions and
//! goodbyes, read and written as compound packets.
//!
//! Reading checks a whole compound packet up front, per the validity rules
//! of RFC 3550 appendix A.2, so iterating over a [`Compound`] cannot fail.
//! Writing appends packets into a caller's buffer with a [`Writer`]; each
//! append writes all of its packet or none of it.
//!
//! Times follow RFC 3550 §4: a full 64-bit NTP timestamp in sender reports,
//! and the middle 32 bits ("NTP short", units of 1/65536 s) for last-SR and
//! delay fields. Round trips need only one clock per side, so any consistent
//! epoch works; see [`ntp_from_micros`] and [`round_trip`].

use crate::stats::Report;
use crate::{be16, be32, Error};

/// Sender report packet type.
pub const SR: u8 = 200;
/// Receiver report packet type.
pub const RR: u8 = 201;
/// Source description packet type.
pub const SDES: u8 = 202;
/// Goodbye packet type.
pub const BYE: u8 = 203;
/// Application-defined packet type.
pub const APP: u8 = 204;

/// Most report blocks, sources or chunks one packet can count (5 bits).
pub const MAX_COUNT: usize = 31;

const HEADER_LEN: usize = 4;

/// SDES item types (RFC 3550 §6.5).
pub mod sdes {
    /// Canonical name: a persistent, unique identifier for the endpoint.
    pub const CNAME: u8 = 1;
    /// A human-readable name.
    pub const NAME: u8 = 2;
    /// Email address.
    pub const EMAIL: u8 = 3;
    /// Phone number.
    pub const PHONE: u8 = 4;
    /// Geographic location.
    pub const LOC: u8 = 5;
    /// Application or tool name.
    pub const TOOL: u8 = 6;
    /// Transient notice.
    pub const NOTE: u8 = 7;
    /// Private extension.
    pub const PRIV: u8 = 8;
}

/// The sender information in a sender report (RFC 3550 §6.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SenderInfo {
    /// Wallclock time the report was sent, 64-bit NTP format.
    pub ntp_timestamp: u64,
    /// The same instant on the RTP media clock.
    pub rtp_timestamp: u32,
    /// Packets sent since the stream started, wrapping.
    pub packet_count: u32,
    /// Payload octets sent since the stream started, wrapping.
    pub octet_count: u32,
}

impl SenderInfo {
    /// Encoded size.
    pub const LEN: usize = 20;

    fn read(bytes: &[u8]) -> Self {
        Self {
            ntp_timestamp: (u64::from(be32(bytes, 0)) << 32) | u64::from(be32(bytes, 4)),
            rtp_timestamp: be32(bytes, 8),
            packet_count: be32(bytes, 12),
            octet_count: be32(bytes, 16),
        }
    }

    fn write(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.ntp_timestamp.to_be_bytes());
        out[8..12].copy_from_slice(&self.rtp_timestamp.to_be_bytes());
        out[12..16].copy_from_slice(&self.packet_count.to_be_bytes());
        out[16..20].copy_from_slice(&self.octet_count.to_be_bytes());
    }
}

/// One report block: what a receiver says about one source (RFC 3550
/// §6.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReportBlock {
    /// The source this block is about.
    pub ssrc: u32,
    /// Fraction of packets lost since the previous report, out of 256.
    pub fraction_lost: u8,
    /// Packets lost since the start, 24-bit signed on the wire.
    pub cumulative_lost: i32,
    /// Highest sequence number received, with wraparounds in the upper 16
    /// bits.
    pub extended_highest: u32,
    /// Interarrival jitter, in RTP clock ticks.
    pub jitter: u32,
    /// Middle 32 bits of the NTP timestamp of the last sender report from
    /// this source, or 0 if none.
    pub last_sr: u32,
    /// Time since that sender report was received, in 1/65536 s, or 0.
    pub delay_since_last_sr: u32,
}

impl ReportBlock {
    /// Encoded size.
    pub const LEN: usize = 24;

    /// A block about `ssrc` from reception statistics and the last sender
    /// report received from it.
    #[must_use]
    pub fn new(ssrc: u32, report: &Report, last_sr: u32, delay_since_last_sr: u32) -> Self {
        Self {
            ssrc,
            fraction_lost: report.fraction_lost,
            cumulative_lost: report.cumulative_lost,
            extended_highest: report.extended_highest,
            jitter: report.jitter,
            last_sr,
            delay_since_last_sr,
        }
    }

    fn read(bytes: &[u8]) -> Self {
        let lost = be32(bytes, 4);
        Self {
            ssrc: be32(bytes, 0),
            fraction_lost: (lost >> 24) as u8,
            // Sign-extend the 24-bit field.
            cumulative_lost: ((lost << 8) as i32) >> 8,
            extended_highest: be32(bytes, 8),
            jitter: be32(bytes, 12),
            last_sr: be32(bytes, 16),
            delay_since_last_sr: be32(bytes, 20),
        }
    }

    fn write(&self, out: &mut [u8]) {
        let cumulative = self.cumulative_lost.clamp(-0x80_0000, 0x7f_ffff) as u32 & 0x00ff_ffff;
        out[0..4].copy_from_slice(&self.ssrc.to_be_bytes());
        out[4..8]
            .copy_from_slice(&((u32::from(self.fraction_lost) << 24) | cumulative).to_be_bytes());
        out[8..12].copy_from_slice(&self.extended_highest.to_be_bytes());
        out[12..16].copy_from_slice(&self.jitter.to_be_bytes());
        out[16..20].copy_from_slice(&self.last_sr.to_be_bytes());
        out[20..24].copy_from_slice(&self.delay_since_last_sr.to_be_bytes());
    }
}

/// One packet of a compound RTCP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet<'a> {
    /// SR: an active sender's report.
    SenderReport {
        /// The sender.
        ssrc: u32,
        /// Its sending statistics.
        info: SenderInfo,
        /// What it has received from others.
        reports: ReportBlocks<'a>,
    },
    /// RR: a receiver's report.
    ReceiverReport {
        /// The reporter.
        ssrc: u32,
        /// What it has received.
        reports: ReportBlocks<'a>,
    },
    /// SDES: names for sources.
    SourceDescription(Chunks<'a>),
    /// BYE: sources leaving.
    Goodbye {
        /// The sources leaving.
        sources: Sources<'a>,
        /// Why, if given.
        reason: Option<&'a [u8]>,
    },
    /// Anything else (APP, feedback, ...), uninterpreted.
    Other {
        /// The packet type.
        packet_type: u8,
        /// The 5-bit count field.
        count: u8,
        /// The body after the 4-byte header, padding removed.
        body: &'a [u8],
    },
}

/// A validated compound RTCP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compound<'a> {
    bytes: &'a [u8],
}

impl<'a> Compound<'a> {
    /// Check a datagram against RFC 3550 A.2: every packet is version 2,
    /// the first is SR or RR, only the last may be padded, and the lengths
    /// add up to exactly the datagram. Each packet's own structure is
    /// checked too.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let mut offset = 0;
        let mut first = true;
        if bytes.len() < HEADER_LEN {
            return Err(Error::Truncated);
        }
        while offset < bytes.len() {
            let (packet, len) = split(&bytes[offset..])?;
            let last = offset + len == bytes.len();
            if bytes[offset] & 0x20 != 0 && !last {
                return Err(Error::BadPadding);
            }
            if first && !matches!(bytes[offset + 1], SR | RR) {
                return Err(Error::BadCompound);
            }
            decode(packet)?;
            first = false;
            offset += len;
        }
        Ok(Self { bytes })
    }

    /// The packets, in order.
    #[must_use]
    pub fn packets(&self) -> Packets<'a> {
        Packets { bytes: self.bytes }
    }
}

/// Iterator over the packets of a [`Compound`].
#[derive(Debug, Clone)]
pub struct Packets<'a> {
    bytes: &'a [u8],
}

impl<'a> Iterator for Packets<'a> {
    type Item = Packet<'a>;

    fn next(&mut self) -> Option<Packet<'a>> {
        // Validated by `Compound::parse`; stop rather than panic regardless.
        let (packet, len) = split(self.bytes).ok()?;
        self.bytes = &self.bytes[len..];
        decode(packet).ok()
    }
}

/// One packet's bytes: header plus body with padding removed.
struct Raw<'a> {
    first: u8,
    packet_type: u8,
    body: &'a [u8],
}

/// Split the first packet off `bytes`, returning it and its length on the
/// wire.
fn split(bytes: &[u8]) -> Result<(Raw<'_>, usize), Error> {
    let header = bytes.get(..HEADER_LEN).ok_or(Error::Truncated)?;
    if header[0] >> 6 != 2 {
        return Err(Error::BadVersion);
    }
    // The length field counts 32-bit words, minus one.
    let len = (usize::from(be16(header, 2)) + 1) * 4;
    let packet = bytes.get(..len).ok_or(Error::Truncated)?;
    let mut end = len;
    if header[0] & 0x20 != 0 {
        let padding = usize::from(packet[len - 1]);
        if padding == 0 || padding > len - HEADER_LEN {
            return Err(Error::BadPadding);
        }
        end -= padding;
    }
    Ok((
        Raw {
            first: header[0],
            packet_type: header[1],
            body: &packet[HEADER_LEN..end],
        },
        len,
    ))
}

fn decode(raw: Raw<'_>) -> Result<Packet<'_>, Error> {
    let count = raw.first & 0x1f;
    let body = raw.body;
    match raw.packet_type {
        SR => {
            let blocks = 4 + SenderInfo::LEN;
            let reports = report_blocks(body, blocks, count)?;
            Ok(Packet::SenderReport {
                ssrc: be32(body, 0),
                info: SenderInfo::read(&body[4..blocks]),
                reports,
            })
        }
        RR => {
            let reports = report_blocks(body, 4, count)?;
            Ok(Packet::ReceiverReport {
                ssrc: be32(body, 0),
                reports,
            })
        }
        SDES => {
            let chunks = Chunks {
                bytes: body,
                remaining: count,
            };
            // Walk once to validate; the iterator then cannot fail.
            let mut walk = chunks;
            for _ in 0..count {
                walk.try_next()?;
            }
            Ok(Packet::SourceDescription(chunks))
        }
        BYE => {
            let ids = 4 * usize::from(count);
            let sources = body.get(..ids).ok_or(Error::Truncated)?;
            let rest = &body[ids..];
            // An optional reason: a length octet and text. Anything after it
            // is padding to the 32-bit boundary, which must be zero.
            let reason = match rest.first() {
                None => None,
                Some(&0) if rest.iter().all(|&b| b == 0) => None,
                Some(&len) => {
                    let len = usize::from(len);
                    Some(rest.get(1..1 + len).ok_or(Error::Truncated)?)
                }
            };
            Ok(Packet::Goodbye {
                sources: Sources { bytes: sources },
                reason,
            })
        }
        packet_type => Ok(Packet::Other {
            packet_type,
            count,
            body,
        }),
    }
}

/// The report blocks of an SR or RR, after `start` bytes of fixed fields.
/// Bytes after the blocks are profile-specific extensions, ignored.
fn report_blocks(body: &[u8], start: usize, count: u8) -> Result<ReportBlocks<'_>, Error> {
    let end = start + ReportBlock::LEN * usize::from(count);
    let bytes = body.get(start..end).ok_or(Error::Truncated)?;
    Ok(ReportBlocks { bytes })
}

/// Iterator over report blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportBlocks<'a> {
    bytes: &'a [u8],
}

impl Iterator for ReportBlocks<'_> {
    type Item = ReportBlock;

    fn next(&mut self) -> Option<ReportBlock> {
        let block = self.bytes.get(..ReportBlock::LEN)?;
        self.bytes = &self.bytes[ReportBlock::LEN..];
        Some(ReportBlock::read(block))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.bytes.len() / ReportBlock::LEN;
        (n, Some(n))
    }
}

impl ExactSizeIterator for ReportBlocks<'_> {}

/// Iterator over the SSRCs in a BYE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sources<'a> {
    bytes: &'a [u8],
}

impl Iterator for Sources<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let id = self.bytes.get(..4)?;
        self.bytes = &self.bytes[4..];
        Some(be32(id, 0))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.bytes.len() / 4;
        (n, Some(n))
    }
}

impl ExactSizeIterator for Sources<'_> {}

/// Iterator over the chunks of an SDES packet: one per source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunks<'a> {
    bytes: &'a [u8],
    remaining: u8,
}

impl<'a> Chunks<'a> {
    fn try_next(&mut self) -> Result<Option<Chunk<'a>>, Error> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let ssrc = self.bytes.get(..4).ok_or(Error::Truncated)?;
        // Items run until a zero type octet, then zero octets pad the chunk
        // to a 32-bit boundary (RFC 3550 §6.5). Chunks start aligned.
        let mut at = 4;
        loop {
            match self.bytes.get(at) {
                None => return Err(Error::Truncated),
                Some(0) => break,
                Some(_) => {
                    let len = usize::from(*self.bytes.get(at + 1).ok_or(Error::Truncated)?);
                    at += 2 + len;
                }
            }
        }
        let end = (at + 1).next_multiple_of(4);
        if self
            .bytes
            .get(at..end)
            .ok_or(Error::Truncated)?
            .iter()
            .any(|&b| b != 0)
        {
            return Err(Error::Truncated);
        }
        let chunk = Chunk {
            ssrc: be32(ssrc, 0),
            items: Items {
                bytes: &self.bytes[4..at],
            },
        };
        self.bytes = &self.bytes[end..];
        Ok(Some(chunk))
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        self.try_next().ok().flatten()
    }
}

/// One source's SDES items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    /// The source described.
    pub ssrc: u32,
    /// Its items.
    pub items: Items<'a>,
}

impl<'a> Chunk<'a> {
    /// The value of the first item of type `kind`.
    #[must_use]
    pub fn item(&self, kind: u8) -> Option<&'a [u8]> {
        self.items
            .clone()
            .find(|item| item.kind == kind)
            .map(|item| item.value)
    }

    /// The CNAME item.
    #[must_use]
    pub fn cname(&self) -> Option<&'a [u8]> {
        self.item(sdes::CNAME)
    }

    /// The NAME item.
    #[must_use]
    pub fn name(&self) -> Option<&'a [u8]> {
        self.item(sdes::NAME)
    }
}

/// One SDES item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item<'a> {
    /// Item type: see [`sdes`].
    pub kind: u8,
    /// Its value, UTF-8 by RFC 3550 but not checked here.
    pub value: &'a [u8],
}

/// Iterator over a chunk's items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Items<'a> {
    bytes: &'a [u8],
}

impl<'a> Iterator for Items<'a> {
    type Item = Item<'a>;

    fn next(&mut self) -> Option<Item<'a>> {
        let kind = *self.bytes.first()?;
        let len = usize::from(*self.bytes.get(1)?);
        let value = self.bytes.get(2..2 + len)?;
        self.bytes = &self.bytes[2 + len..];
        Some(Item { kind, value })
    }
}

/// Builds a compound RTCP packet in a caller's buffer.
///
/// RFC 3550 §6.1 requires a compound packet to start with an SR or RR, even
/// an empty one, and to include an SDES CNAME. The writer doesn't enforce
/// order; [`Compound::parse`] on the result will.
pub struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Writer<'a> {
    /// An empty compound packet over `buf`.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The compound packet written so far.
    #[must_use]
    pub fn finish(self) -> &'a [u8] {
        let Self { buf, len } = self;
        &buf[..len]
    }

    /// Reserve a packet of `body` bytes (a multiple of 4) and write its
    /// header, returning the body to fill.
    fn packet(&mut self, packet_type: u8, count: usize, body: usize) -> Result<&mut [u8], Error> {
        debug_assert_eq!(body % 4, 0);
        if count > MAX_COUNT {
            return Err(Error::TooMany);
        }
        let len = HEADER_LEN + body;
        let start = self.len;
        let out = self
            .buf
            .get_mut(start..start + len)
            .ok_or(Error::BufferTooSmall)?;
        out[0] = 0x80 | count as u8;
        out[1] = packet_type;
        out[2..4].copy_from_slice(&((len / 4 - 1) as u16).to_be_bytes());
        self.len += len;
        Ok(&mut out[HEADER_LEN..])
    }

    /// Append a sender report.
    pub fn sender_report(
        &mut self,
        ssrc: u32,
        info: &SenderInfo,
        reports: &[ReportBlock],
    ) -> Result<(), Error> {
        let body = self.packet(
            SR,
            reports.len(),
            4 + SenderInfo::LEN + ReportBlock::LEN * reports.len(),
        )?;
        body[0..4].copy_from_slice(&ssrc.to_be_bytes());
        info.write(&mut body[4..24]);
        for (out, block) in body[24..].chunks_exact_mut(ReportBlock::LEN).zip(reports) {
            block.write(out);
        }
        Ok(())
    }

    /// Append a receiver report.
    pub fn receiver_report(&mut self, ssrc: u32, reports: &[ReportBlock]) -> Result<(), Error> {
        let body = self.packet(RR, reports.len(), 4 + ReportBlock::LEN * reports.len())?;
        body[0..4].copy_from_slice(&ssrc.to_be_bytes());
        for (out, block) in body[4..].chunks_exact_mut(ReportBlock::LEN).zip(reports) {
            block.write(out);
        }
        Ok(())
    }

    /// Append an SDES packet describing one source with `(type, value)`
    /// items, for example `&[(sdes::CNAME, b"kitchen@example")]`. Values are at
    /// most 255 bytes.
    pub fn source_description(&mut self, ssrc: u32, items: &[(u8, &[u8])]) -> Result<(), Error> {
        let mut chunk = 4;
        for &(kind, value) in items {
            if value.len() > 255 {
                return Err(Error::TooLong);
            }
            // Type 0 would read as the end of the items.
            if kind == 0 {
                return Err(Error::BadCompound);
            }
            chunk += 2 + value.len();
        }
        // At least one zero octet ends the items, then pad to 32 bits.
        let chunk_len = (chunk + 1).next_multiple_of(4);
        let body = self.packet(SDES, 1, chunk_len)?;
        body.fill(0);
        body[0..4].copy_from_slice(&ssrc.to_be_bytes());
        let mut at = 4;
        for &(kind, value) in items {
            body[at] = kind;
            body[at + 1] = value.len() as u8;
            body[at + 2..at + 2 + value.len()].copy_from_slice(value);
            at += 2 + value.len();
        }
        Ok(())
    }

    /// Append a BYE for `sources`, with an optional reason of at most 255
    /// bytes.
    pub fn goodbye(&mut self, sources: &[u32], reason: Option<&[u8]>) -> Result<(), Error> {
        let reason_len = match reason {
            Some(text) if text.len() > 255 => return Err(Error::TooLong),
            Some(text) => (1 + text.len()).next_multiple_of(4),
            None => 0,
        };
        let body = self.packet(BYE, sources.len(), 4 * sources.len() + reason_len)?;
        body.fill(0);
        for (out, id) in body.chunks_exact_mut(4).zip(sources) {
            out.copy_from_slice(&id.to_be_bytes());
        }
        if let Some(text) = reason {
            let at = 4 * sources.len();
            body[at] = text.len() as u8;
            body[at + 1..at + 1 + text.len()].copy_from_slice(text);
        }
        Ok(())
    }
}

/// The middle 32 bits of a 64-bit NTP timestamp: the "NTP short" format of
/// last-SR fields, in units of 1/65536 s.
#[must_use]
pub fn ntp_short(ntp: u64) -> u32 {
    (ntp >> 16) as u32
}

/// A 64-bit NTP timestamp (seconds in the high 32 bits, binary fraction in
/// the low 32) from microseconds since any epoch. Round trips only need the
/// same epoch on each side; for wallclock time, count from 1900.
#[must_use]
pub fn ntp_from_micros(micros: u64) -> u64 {
    let seconds = micros / 1_000_000;
    let fraction = ((micros % 1_000_000) << 32) / 1_000_000;
    (seconds << 32) | fraction
}

/// Round-trip time from a report block about our stream (RFC 3550 §6.4.1):
/// `arrival - last_sr - delay_since_last_sr`, all in 1/65536 s, where
/// `arrival` is when the report arrived, as NTP short on the clock our sender
/// reports used. `None` if the receiver has had no sender report from us yet,
/// or if the figures are inconsistent.
#[must_use]
pub fn round_trip(arrival: u32, last_sr: u32, delay_since_last_sr: u32) -> Option<u32> {
    if last_sr == 0 {
        return None;
    }
    arrival
        .wrapping_sub(last_sr)
        .checked_sub(delay_since_last_sr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_round_trip() {
        let mut buf = [0u8; 256];
        let mut writer = Writer::new(&mut buf);
        let block = ReportBlock {
            ssrc: 9,
            fraction_lost: 25,
            cumulative_lost: -3,
            extended_highest: 70_000,
            jitter: 12,
            last_sr: 0x1234_5678,
            delay_since_last_sr: 65_536,
        };
        let info = SenderInfo {
            ntp_timestamp: 0x0102_0304_0506_0708,
            rtp_timestamp: 5,
            packet_count: 6,
            octet_count: 7,
        };
        writer.sender_report(1, &info, &[block]).unwrap();
        writer
            .source_description(1, &[(sdes::CNAME, b"a@b"), (sdes::NAME, b"Kitchen")])
            .unwrap();
        writer.goodbye(&[1], Some(b"bye")).unwrap();
        let bytes = writer.finish();

        let mut packets = Compound::parse(bytes).unwrap().packets();
        match packets.next() {
            Some(Packet::SenderReport {
                ssrc: 1,
                info: got,
                mut reports,
            }) => {
                assert_eq!(got, info);
                assert_eq!(reports.next(), Some(block));
                assert_eq!(reports.next(), None);
            }
            other => panic!("{other:?}"),
        }
        match packets.next() {
            Some(Packet::SourceDescription(mut chunks)) => {
                let chunk = chunks.next().unwrap();
                assert_eq!(
                    (chunk.ssrc, chunk.cname(), chunk.name()),
                    (1, Some(&b"a@b"[..]), Some(&b"Kitchen"[..]))
                );
                assert_eq!(chunks.next(), None);
            }
            other => panic!("{other:?}"),
        }
        match packets.next() {
            Some(Packet::Goodbye {
                mut sources,
                reason,
            }) => {
                assert_eq!(sources.next(), Some(1));
                assert_eq!(reason, Some(&b"bye"[..]));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(packets.next(), None);
    }

    #[test]
    fn writer_errors_write_nothing() {
        let mut buf = [0u8; 40];
        let mut writer = Writer::new(&mut buf);
        assert_eq!(
            writer.receiver_report(1, &[ReportBlock::default(); 32]),
            Err(Error::TooMany)
        );
        assert_eq!(
            writer.receiver_report(1, &[ReportBlock::default(); 2]),
            Err(Error::BufferTooSmall)
        );
        assert_eq!(
            writer.source_description(1, &[(sdes::NOTE, &[0; 256])]),
            Err(Error::TooLong)
        );
        assert!(writer.is_empty());
    }

    #[test]
    fn ntp_conversions() {
        assert_eq!(ntp_from_micros(1_500_000), 0x0000_0001_8000_0000);
        assert_eq!(ntp_short(0x0000_0001_8000_0000), 0x0001_8000);
    }
}
