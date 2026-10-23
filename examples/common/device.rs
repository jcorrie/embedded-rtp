//! The device side of the example intercom, and what it shares with the
//! server: `Device` joins a room, sends what it captures and plays what it
//! hears. No allocator and no `std`, so the same file runs in
//! `examples/intercom_client` on a desktop and in `examples/pico-w` on a
//! Pico W. See `intercom.rs` for the room and the join handshake.

#![allow(dead_code)]

use embedded_rtp::jitter::{Config, Counters};
use embedded_rtp::l16::{self, Concealer};
use embedded_rtp::packet::{self, Packet};
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PAYLOAD_TYPE};
use embedded_rtp::rtcp::{self, sdes, Compound, Writer};
use embedded_rtp::srtp::{self, Inbound, Outbound, RTCP_OVERHEAD, WRAPPED_LEN};
use embedded_rtp::{Receiver, Sender};

/// One 20 ms frame of samples.
pub type Frame = [i16; FRAME_SAMPLES];
/// A device's key-encryption key.
pub type Kek = [u8; 16];

pub const TICK_MS: u64 = 20;
pub const RTCP_INTERVAL_MS: u64 = 5_000;
pub const TIMEOUT_MS: u64 = 15_000;
pub const JOIN_RETRY_MS: u64 = 1_000;
/// Big enough for any datagram either side sends or accepts.
pub const DATAGRAM_MAX: usize = 1500;

pub const JOIN: &[u8; 4] = b"JOIN";
pub const KEYS: &[u8; 4] = b"KEYS";
pub const KEYS_LEN: usize = 4 + 2 * WRAPPED_LEN;
pub const FRAME_TICKS: u32 = FRAME_SAMPLES as u32;

/// Local time in RTP clock ticks (16 kHz). Wraps; only differences matter.
pub fn rtp_time(now_ms: u64) -> u32 {
    (now_ms * 16) as u32
}

pub fn ntp_time(now_ms: u64) -> u64 {
    rtcp::ntp_from_micros(now_ms * 1_000)
}

pub fn ntp_short(now_ms: u64) -> u32 {
    rtcp::ntp_short(ntp_time(now_ms))
}

/// A sender with a random SSRC, first sequence number and first timestamp,
/// from 10 random bytes.
pub fn random_sender(bytes: [u8; 10]) -> Sender {
    let ssrc = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let sequence = u16::from_be_bytes([bytes[4], bytes[5]]);
    let timestamp = u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
    Sender::new(ssrc, PAYLOAD_TYPE, FRAME_TICKS, sequence, timestamp)
}

/// Write `samples` into `buf` as an RTP packet from `sender`, protect it if
/// there are keys, and return its length.
pub fn audio_packet(
    sender: &mut Sender,
    outbound: Option<&mut Outbound>,
    samples: &Frame,
    buf: &mut [u8],
) -> Option<usize> {
    l16::encode(samples, &mut buf[packet::HEADER_LEN..]);
    let len = sender.send_in_place(buf, FRAME_BYTES).ok()?;
    match outbound {
        Some(outbound) => outbound.protect_rtp(buf, len).ok(),
        None => Some(len),
    }
}

/// A compound report: SR if `sender` has sent anything, else RR, with a
/// report block about what `receiver` has heard, plus SDES. Protected if
/// there are keys.
pub fn report<const SLOTS: usize>(
    sender: &Sender,
    receiver: &mut Receiver<SLOTS, FRAME_BYTES>,
    outbound: Option<&mut Outbound>,
    items: &[(u8, &[u8])],
    now_ms: u64,
    buf: &mut [u8],
) -> Option<usize> {
    let block = receiver.report_block(ntp_short(now_ms));
    let blocks = block.as_slice();
    let room = buf.len() - RTCP_OVERHEAD;
    let mut writer = Writer::new(&mut buf[..room]);
    if sender.packet_count() > 0 {
        let info = sender.sender_info(ntp_time(now_ms));
        writer.sender_report(sender.ssrc(), &info, blocks).ok()?;
    } else {
        writer.receiver_report(sender.ssrc(), blocks).ok()?;
    }
    writer.source_description(sender.ssrc(), items).ok()?;
    let len = writer.finish().len();
    match outbound {
        Some(outbound) => outbound.protect_rtcp(buf, len).ok(),
        None => Some(len),
    }
}

/// Read a compound RTCP packet: note sender reports in `receiver`, pass any
/// SDES NAME to `on_name`, and return whether it says goodbye.
pub fn read_rtcp<const SLOTS: usize>(
    datagram: &[u8],
    receiver: &mut Receiver<SLOTS, FRAME_BYTES>,
    now_ms: u64,
    mut on_name: impl FnMut(&[u8]),
) -> bool {
    let mut goodbye = false;
    let Ok(compound) = Compound::parse(datagram) else {
        return goodbye;
    };
    for packet in compound.packets() {
        match packet {
            rtcp::Packet::SenderReport { ssrc, info, .. } => {
                receiver.sender_report(ssrc, &info, ntp_short(now_ms));
            }
            rtcp::Packet::SourceDescription(chunks) => {
                for chunk in chunks {
                    if let Some(name) = chunk.name() {
                        on_name(name);
                    }
                }
            }
            rtcp::Packet::Goodbye { .. } => goodbye = true,
            _ => {}
        }
    }
    goodbye
}

/// One intercom device: joins the room, sends what it captures, plays what it
/// hears.
pub struct Device<'a> {
    id: &'a str,
    name: &'a str,
    /// `None` in insecure mode.
    kek: Option<Kek>,
    /// Outbound for what it sends, inbound for what it hears.
    keys: Option<(Outbound, Inbound)>,
    /// The last `KEYS` reply acted on, so a duplicate doesn't reset state.
    keys_reply: Option<[u8; KEYS_LEN]>,
    joined: bool,
    last_join_ms: Option<u64>,
    last_heard_ms: u64,
    last_report_ms: u64,
    sender: Sender,
    receiver: Receiver<8, FRAME_BYTES>,
    concealer: Concealer<FRAME_SAMPLES>,
    buf: [u8; DATAGRAM_MAX],
}

impl<'a> Device<'a> {
    /// A device called `id` (and displayed as `name`). Without a KEK it
    /// talks plain RTP, for a server in insecure mode. `random` seeds its
    /// SSRC, first sequence number and first timestamp.
    pub fn new(id: &'a str, name: &'a str, kek: Option<Kek>, random: [u8; 10]) -> Self {
        Self {
            id,
            name,
            joined: kek.is_none(),
            kek,
            keys: None,
            keys_reply: None,
            last_join_ms: None,
            last_heard_ms: 0,
            last_report_ms: 0,
            sender: random_sender(random),
            receiver: Receiver::new(PAYLOAD_TYPE, FRAME_TICKS, Config::DEFAULT),
            concealer: Concealer::new(),
            buf: [0; DATAGRAM_MAX],
        }
    }

    /// Whether it has keys (or needs none) and can talk.
    pub fn is_joined(&self) -> bool {
        self.joined
    }

    /// How its jitter buffer has fared.
    pub fn counters(&self) -> Counters {
        self.receiver.counters()
    }

    /// One 20 ms tick: send `capture` (`None` when there's nothing to say),
    /// keep the session alive, and return the frame to play (`None` for
    /// silence).
    pub fn tick(
        &mut self,
        now_ms: u64,
        capture: Option<&Frame>,
        send: &mut dyn FnMut(&[u8]),
    ) -> Option<Frame> {
        if self.kek.is_some()
            && self.joined
            && now_ms.saturating_sub(self.last_heard_ms) >= TIMEOUT_MS
        {
            self.joined = false; // the server forgot us, or restarted
            self.last_join_ms = None;
        }
        if !self.joined {
            let due = match self.last_join_ms {
                None => true,
                Some(at) => now_ms.saturating_sub(at) >= JOIN_RETRY_MS,
            };
            if due {
                self.last_join_ms = Some(now_ms);
                let id = self.id.as_bytes();
                let len = (JOIN.len() + id.len()).min(self.buf.len());
                self.buf[..JOIN.len()].copy_from_slice(JOIN);
                self.buf[JOIN.len()..len].copy_from_slice(&id[..len - JOIN.len()]);
                send(&self.buf[..len]);
            }
            return None;
        }

        let outbound = self.keys.as_mut().map(|(outbound, _)| outbound);
        match capture {
            Some(samples) => {
                if let Some(len) = audio_packet(&mut self.sender, outbound, samples, &mut self.buf)
                {
                    send(&self.buf[..len]);
                }
            }
            None => self.sender.skip(),
        }

        // Report on joining and every few seconds after: the keep-alive.
        if self.last_report_ms == 0
            || now_ms.saturating_sub(self.last_report_ms) >= RTCP_INTERVAL_MS
        {
            self.last_report_ms = now_ms.max(1);
            let outbound = self.keys.as_mut().map(|(outbound, _)| outbound);
            let items: [(u8, &[u8]); 2] = [
                (sdes::CNAME, self.id.as_bytes()),
                (sdes::NAME, self.name.as_bytes()),
            ];
            if let Some(len) = report(
                &self.sender,
                &mut self.receiver,
                outbound,
                &items,
                now_ms,
                &mut self.buf,
            ) {
                send(&self.buf[..len]);
            }
        }

        self.concealer.play(self.receiver.play()).copied()
    }

    /// Handle one datagram from the server. It may be changed in place.
    pub fn datagram(&mut self, data: &mut [u8], now_ms: u64) {
        if data.starts_with(KEYS) {
            self.keys_received(data, now_ms);
            return;
        }
        let rtcp = packet::is_rtcp(data);
        let len = match &mut self.keys {
            Some((_, inbound)) if rtcp => inbound.unprotect_rtcp(data),
            Some((_, inbound)) => inbound.unprotect_rtp(data),
            None if self.kek.is_none() => Ok(data.len()),
            None => return,
        };
        let Ok(len) = len else {
            return;
        };
        self.last_heard_ms = now_ms;
        let data = &data[..len];
        if rtcp {
            read_rtcp(data, &mut self.receiver, now_ms, |_| {});
        } else if let Ok(packet) = Packet::parse(data) {
            self.receiver.receive(&packet, rtp_time(now_ms));
        }
    }

    fn keys_received(&mut self, data: &[u8], now_ms: u64) {
        let Some(kek) = self.kek else {
            return;
        };
        let Ok(reply) = <[u8; KEYS_LEN]>::try_from(data) else {
            return;
        };
        if self.keys_reply == Some(reply) {
            return;
        }
        let mut up = [0u8; WRAPPED_LEN];
        let mut down = [0u8; WRAPPED_LEN];
        up.copy_from_slice(&data[4..4 + WRAPPED_LEN]);
        down.copy_from_slice(&data[4 + WRAPPED_LEN..]);
        let (Ok(up), Ok(down)) = (srtp::unwrap(&kek, &up), srtp::unwrap(&kek, &down)) else {
            return; // not for us
        };
        self.keys = Some((Outbound::new(&up), Inbound::new(&down)));
        self.keys_reply = Some(reply);
        self.joined = true;
        self.last_heard_ms = now_ms;
        self.last_report_ms = 0;
        // A new session is a new stream from the server.
        self.receiver = Receiver::new(PAYLOAD_TYPE, FRAME_TICKS, Config::DEFAULT);
    }

    /// Leave the room: a BYE, so the server drops us at once.
    pub fn leave(&mut self, send: &mut dyn FnMut(&[u8])) {
        if !self.joined {
            return;
        }
        let room = self.buf.len() - RTCP_OVERHEAD;
        let mut writer = Writer::new(&mut self.buf[..room]);
        let ssrc = self.sender.ssrc();
        let ok = writer.receiver_report(ssrc, &[]).is_ok()
            && writer
                .source_description(ssrc, &[(sdes::CNAME, self.id.as_bytes())])
                .is_ok()
            && writer.goodbye(&[ssrc], None).is_ok();
        if !ok {
            return;
        }
        let len = writer.finish().len();
        let len = match &mut self.keys {
            Some((outbound, _)) => outbound.protect_rtcp(&mut self.buf, len).ok(),
            None => Some(len),
        };
        if let Some(len) = len {
            send(&self.buf[..len]);
        }
        self.joined = false;
    }
}

/// Parse a hex KEK.
pub fn parse_kek(hex: &str) -> Option<Kek> {
    let hex = hex.trim();
    if hex.len() != 32 {
        return None;
    }
    let mut kek = [0u8; 16];
    for (i, byte) in kek.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(kek)
}
