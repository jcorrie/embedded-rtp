//! An intercom built on embedded-rtp: a mixing server and a device, shared by
//! `examples/mixing_server` and `examples/intercom_client`.
//!
//! Both are sans-IO like the crate: they take datagrams and the time in
//! milliseconds, and hand datagrams to send to a callback. The examples wrap
//! them in `std::net::UdpSocket`; the tests drive them directly.
//!
//! # The room
//!
//! Every participant hears everyone else, never themselves ("mix minus").
//! Each 20 ms tick the server plays every participant's jitter buffer through
//! concealment, sums them, and sends each participant the sum minus its own
//! audio. Nobody else talking means no packet, as RTP intends for silence.
//!
//! # Joining (this example's own signalling, not part of RTP)
//!
//! The server knows each device by an id and a 16-byte key-encryption key
//! (KEK), provisioned out of band. A device sends `JOIN <id>`; the server
//! picks fresh SRTP master keys, one per direction, and replies
//! `KEYS <up> <down>` with both wrapped under the device's KEK
//! (`srtp::wrap`, RFC 5649). `up` protects what the device sends, `down` what
//! it receives.
//!
//! A participant is *pending* until a packet arrives that authenticates under
//! its new keys. Only then is it mixed and sent audio, so a stranger who
//! sends someone else's id gets a reply it can't unwrap, and nothing else.
//! Joining again (after a reboot, or from a new port) gets fresh keys.
//!
//! RTCP every 5 s in each direction doubles as the keep-alive: a participant
//! the server hasn't heard from for 15 s is dropped, and a device that hasn't
//! heard the server for 15 s joins again. A BYE leaves at once.
//!
//! In insecure mode (`Mixer::new(.., true, ..)`) there are no keys: any
//! address that sends a valid RTP or RTCP packet joins, in the clear. That
//! is for testing with tools such as ffmpeg on a trusted network only.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;

use embedded_rtp::jitter::{Config, Counters};
use embedded_rtp::l16::{self, Concealer};
use embedded_rtp::packet::{self, Packet};
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PAYLOAD_TYPE};
use embedded_rtp::rtcp::{self, sdes, Compound, Writer};
use embedded_rtp::srtp::{self, Inbound, MasterKey, Outbound, RTCP_OVERHEAD, WRAPPED_LEN};
use embedded_rtp::{Receiver, Sender};

/// One 20 ms frame of samples.
pub type Frame = [i16; FRAME_SAMPLES];
/// A device's key-encryption key.
pub type Kek = [u8; 16];

pub const TICK_MS: u64 = 20;
pub const RTCP_INTERVAL_MS: u64 = 5_000;
pub const TIMEOUT_MS: u64 = 15_000;
pub const JOIN_RETRY_MS: u64 = 1_000;
pub const PENDING_TIMEOUT_MS: u64 = 5_000;
pub const MAX_PARTICIPANTS: usize = 32;
pub const MAX_PENDING: usize = 32;
/// Big enough for any datagram either side sends or accepts.
pub const DATAGRAM_MAX: usize = 1500;

const JOIN: &[u8; 4] = b"JOIN";
const KEYS: &[u8; 4] = b"KEYS";
const KEYS_LEN: usize = 4 + 2 * WRAPPED_LEN;
const FRAME_TICKS: u32 = FRAME_SAMPLES as u32;

/// Fills a buffer with random bytes: the OS generator in the examples, a
/// seeded one in tests.
pub type Random = Box<dyn FnMut(&mut [u8]) + Send>;

/// Local time in RTP clock ticks (16 kHz). Wraps; only differences matter.
fn rtp_time(now_ms: u64) -> u32 {
    (now_ms * 16) as u32
}

fn ntp_time(now_ms: u64) -> u64 {
    rtcp::ntp_from_micros(now_ms * 1_000)
}

fn ntp_short(now_ms: u64) -> u32 {
    rtcp::ntp_short(ntp_time(now_ms))
}

fn random_key(random: &mut Random) -> MasterKey {
    let mut bytes = [0u8; 30];
    random(&mut bytes);
    MasterKey::from_bytes(&bytes)
}

fn random_sender(random: &mut Random) -> Sender {
    let mut bytes = [0u8; 10];
    random(&mut bytes);
    let ssrc = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let sequence = u16::from_be_bytes([bytes[4], bytes[5]]);
    let timestamp = u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
    Sender::new(ssrc, PAYLOAD_TYPE, FRAME_TICKS, sequence, timestamp)
}

/// Write `samples` into `buf` as an RTP packet from `sender`, protect it if
/// there are keys, and return its length.
fn audio_packet(
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
fn report<const SLOTS: usize>(
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

/// What a compound RTCP packet asks of whoever receives it.
#[derive(Default)]
struct RtcpNews {
    name: Option<String>,
    goodbye: bool,
}

fn read_rtcp<const SLOTS: usize>(
    datagram: &[u8],
    receiver: &mut Receiver<SLOTS, FRAME_BYTES>,
    now_ms: u64,
) -> RtcpNews {
    let mut news = RtcpNews::default();
    let Ok(compound) = Compound::parse(datagram) else {
        return news;
    };
    for packet in compound.packets() {
        match packet {
            rtcp::Packet::SenderReport { ssrc, info, .. } => {
                receiver.sender_report(ssrc, &info, ntp_short(now_ms));
            }
            rtcp::Packet::SourceDescription(chunks) => {
                for chunk in chunks {
                    if let Some(name) = chunk.name() {
                        news.name = Some(String::from_utf8_lossy(name).into_owned());
                    }
                }
            }
            rtcp::Packet::Goodbye { .. } => news.goodbye = true,
            _ => {}
        }
    }
    news
}

struct Participant {
    addr: SocketAddr,
    /// The device id it joined as; `None` in insecure mode.
    device: Option<String>,
    /// Inbound for what it sends, outbound for what it hears. `None` in
    /// insecure mode.
    keys: Option<(Inbound, Outbound)>,
    /// The `KEYS` reply, resent if its `JOIN` is repeated while pending.
    keys_reply: Vec<u8>,
    /// Authenticated at least once: mixed and sent audio.
    active: bool,
    joined_ms: u64,
    last_heard_ms: u64,
    last_report_ms: u64,
    name: String,
    receiver: Receiver<16, FRAME_BYTES>,
    concealer: Concealer<FRAME_SAMPLES>,
    sender: Sender,
    /// What it contributed to this tick's mix.
    frame: Option<Frame>,
}

/// A participant as the mixer sees it, for logs and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub addr: SocketAddr,
    pub device: Option<String>,
    pub name: String,
    pub active: bool,
    /// How its jitter buffer has fared: frames played, concealed, dropped.
    pub counters: Counters,
}

/// The mixing server's state.
pub struct Mixer {
    devices: HashMap<String, Kek>,
    insecure: bool,
    random: Random,
    participants: Vec<Box<Participant>>,
    sum: [i32; FRAME_SAMPLES],
    buf: [u8; DATAGRAM_MAX],
}

impl Mixer {
    /// A room for the `devices` (id to KEK). With `insecure`, anyone who
    /// sends plain RTP joins as well.
    pub fn new(devices: HashMap<String, Kek>, insecure: bool, random: Random) -> Self {
        Self {
            devices,
            insecure,
            random,
            participants: Vec::new(),
            sum: [0; FRAME_SAMPLES],
            buf: [0; DATAGRAM_MAX],
        }
    }

    pub fn members(&self) -> Vec<Member> {
        self.participants
            .iter()
            .map(|p| Member {
                addr: p.addr,
                device: p.device.clone(),
                name: p.name.clone(),
                active: p.active,
                counters: p.receiver.counters(),
            })
            .collect()
    }

    /// Handle one datagram from `from`. It may be changed in place.
    pub fn datagram(
        &mut self,
        from: SocketAddr,
        data: &mut [u8],
        now_ms: u64,
        send: &mut dyn FnMut(SocketAddr, &[u8]),
    ) {
        if let Some(id) = data.strip_prefix(JOIN) {
            let id = String::from_utf8_lossy(id).trim().to_string();
            self.join(from, &id, now_ms, send);
            return;
        }
        let rtcp = packet::is_rtcp(data);
        let index = match self.participants.iter().position(|p| p.addr == from) {
            Some(index) => index,
            None if self.insecure && self.can_admit(true) && plausible(data, rtcp) => {
                let participant = self.participant(from, None, None, now_ms);
                self.participants.push(participant);
                self.participants.len() - 1
            }
            None => return,
        };

        let participant = &mut self.participants[index];
        let len = match &mut participant.keys {
            Some((inbound, _)) if rtcp => inbound.unprotect_rtcp(data),
            Some((inbound, _)) => inbound.unprotect_rtp(data),
            None => Ok(data.len()),
        };
        let Ok(len) = len else {
            return; // forged, replayed or under old keys
        };
        let data = &data[..len];
        participant.last_heard_ms = now_ms;
        let newly_active = !participant.active;
        participant.active = true;

        let mut goodbye = false;
        if rtcp {
            let news = read_rtcp(data, &mut participant.receiver, now_ms);
            if let Some(name) = news.name {
                participant.name = name;
            }
            goodbye = news.goodbye;
        } else if let Ok(packet) = Packet::parse(data) {
            participant.receiver.receive(&packet, rtp_time(now_ms));
        }

        if newly_active {
            // A device that joined again replaces its old session.
            if let Some(device) = participant.device.clone() {
                self.participants
                    .retain(|p| p.addr == from || p.device.as_ref() != Some(&device));
            }
        }
        if goodbye {
            self.participants.retain(|p| p.addr != from);
        }
    }

    fn join(
        &mut self,
        from: SocketAddr,
        id: &str,
        now_ms: u64,
        send: &mut dyn FnMut(SocketAddr, &[u8]),
    ) {
        let Some(&kek) = self.devices.get(id) else {
            return;
        };
        if let Some(existing) = self.participants.iter().position(|p| p.addr == from) {
            let p = &self.participants[existing];
            if !p.active && p.device.as_deref() == Some(id) {
                // Our reply was lost or is still on its way: same keys again.
                send(from, &p.keys_reply.clone());
                return;
            }
            self.participants.remove(existing);
        }
        if !self.can_admit(false) {
            return;
        }
        let up = random_key(&mut self.random);
        let down = random_key(&mut self.random);
        let mut reply = KEYS.to_vec();
        reply.extend_from_slice(&srtp::wrap(&kek, &up));
        reply.extend_from_slice(&srtp::wrap(&kek, &down));
        let keys = (Inbound::new(&up), Outbound::new(&down));
        let mut participant = self.participant(from, Some(id.to_string()), Some(keys), now_ms);
        participant.keys_reply = reply;
        send(from, &participant.keys_reply);
        self.participants.push(participant);
    }

    fn can_admit(&self, active: bool) -> bool {
        let pending = self.participants.iter().filter(|p| !p.active).count();
        let total = self.participants.len() - pending;
        if active {
            total < MAX_PARTICIPANTS
        } else {
            pending < MAX_PENDING
        }
    }

    fn participant(
        &mut self,
        addr: SocketAddr,
        device: Option<String>,
        keys: Option<(Inbound, Outbound)>,
        now_ms: u64,
    ) -> Box<Participant> {
        Box::new(Participant {
            addr,
            name: device.clone().unwrap_or_else(|| addr.to_string()),
            device,
            keys,
            keys_reply: Vec::new(),
            active: false,
            joined_ms: now_ms,
            last_heard_ms: now_ms,
            last_report_ms: now_ms,
            receiver: Receiver::new(PAYLOAD_TYPE, FRAME_TICKS, Config::DEFAULT),
            concealer: Concealer::new(),
            sender: random_sender(&mut self.random),
            frame: None,
        })
    }

    /// One 20 ms tick: drop the silent, mix, and send everyone their mix
    /// and, every few seconds, a report.
    pub fn tick(&mut self, now_ms: u64, send: &mut dyn FnMut(SocketAddr, &[u8])) {
        self.participants.retain(|p| {
            if p.active {
                now_ms.saturating_sub(p.last_heard_ms) < TIMEOUT_MS
            } else {
                now_ms.saturating_sub(p.joined_ms) < PENDING_TIMEOUT_MS
            }
        });

        // Everyone's next frame, and the sum of them all.
        self.sum = [0; FRAME_SAMPLES];
        let mut talking = 0;
        for p in self.participants.iter_mut().filter(|p| p.active) {
            p.frame = p.concealer.play(p.receiver.play()).copied();
            if let Some(frame) = &p.frame {
                talking += 1;
                for (sum, &sample) in self.sum.iter_mut().zip(frame) {
                    *sum += i32::from(sample);
                }
            }
        }

        for p in self.participants.iter_mut().filter(|p| p.active) {
            let others = talking - usize::from(p.frame.is_some());
            if others == 0 {
                p.sender.skip();
            } else {
                // Mix minus: everyone but this participant.
                let mut mix = [0i16; FRAME_SAMPLES];
                let own = p.frame.as_ref();
                for (k, out) in mix.iter_mut().enumerate() {
                    let sample = self.sum[k] - own.map_or(0, |f| i32::from(f[k]));
                    *out = sample.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
                }
                let outbound = p.keys.as_mut().map(|(_, outbound)| outbound);
                if let Some(len) = audio_packet(&mut p.sender, outbound, &mix, &mut self.buf) {
                    send(p.addr, &self.buf[..len]);
                }
            }

            if now_ms.saturating_sub(p.last_report_ms) >= RTCP_INTERVAL_MS {
                p.last_report_ms = now_ms;
                let outbound = p.keys.as_mut().map(|(_, outbound)| outbound);
                let items: [(u8, &[u8]); 1] = [(sdes::CNAME, b"mixer")];
                let report = report(
                    &p.sender,
                    &mut p.receiver,
                    outbound,
                    &items,
                    now_ms,
                    &mut self.buf,
                );
                if let Some(len) = report {
                    send(p.addr, &self.buf[..len]);
                }
            }
        }
    }
}

/// Whether a datagram from a stranger looks worth admitting in insecure
/// mode: a valid RTP packet of our payload type, or valid RTCP.
fn plausible(data: &[u8], rtcp: bool) -> bool {
    if rtcp {
        Compound::parse(data).is_ok()
    } else {
        Packet::parse(data).is_ok_and(|p| p.header.payload_type == PAYLOAD_TYPE)
    }
}

/// One intercom device: joins the room, sends what it captures, plays what it
/// hears.
pub struct Device {
    id: String,
    name: String,
    /// `None` in insecure mode.
    kek: Option<Kek>,
    /// Outbound for what it sends, inbound for what it hears.
    keys: Option<(Outbound, Inbound)>,
    /// The last `KEYS` reply acted on, so a duplicate doesn't reset state.
    keys_reply: Vec<u8>,
    joined: bool,
    last_join_ms: Option<u64>,
    last_heard_ms: u64,
    last_report_ms: u64,
    sender: Sender,
    receiver: Receiver<8, FRAME_BYTES>,
    concealer: Concealer<FRAME_SAMPLES>,
    buf: [u8; DATAGRAM_MAX],
}

impl Device {
    /// A device called `id` (and displayed as `name`). Without a KEK it
    /// talks plain RTP, for a server in insecure mode.
    pub fn new(id: &str, name: &str, kek: Option<Kek>, mut random: Random) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            joined: kek.is_none(),
            kek,
            keys: None,
            keys_reply: Vec::new(),
            last_join_ms: None,
            last_heard_ms: 0,
            last_report_ms: 0,
            sender: random_sender(&mut random),
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
            if self
                .last_join_ms
                .map_or(true, |at| now_ms.saturating_sub(at) >= JOIN_RETRY_MS)
            {
                self.last_join_ms = Some(now_ms);
                let mut join = JOIN.to_vec();
                join.extend_from_slice(self.id.as_bytes());
                send(&join);
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
            read_rtcp(data, &mut self.receiver, now_ms);
        } else if let Ok(packet) = Packet::parse(data) {
            self.receiver.receive(&packet, rtp_time(now_ms));
        }
    }

    fn keys_received(&mut self, data: &[u8], now_ms: u64) {
        let Some(kek) = self.kek else {
            return;
        };
        if data.len() != KEYS_LEN || data == self.keys_reply {
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
        self.keys_reply = data.to_vec();
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

/// Parse a devices file: one `<id> <hex KEK>` per line; `#` starts a
/// comment.
pub fn parse_devices(text: &str) -> Result<HashMap<String, Kek>, String> {
    let mut devices = HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(id), Some(kek), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(format!("line {}: expected `<id> <hex KEK>`", n + 1));
        };
        let kek = parse_kek(kek).ok_or(format!("line {}: KEK must be 32 hex digits", n + 1))?;
        devices.insert(id.to_string(), kek);
    }
    Ok(devices)
}
