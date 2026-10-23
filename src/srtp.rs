//! SRTP and SRTCP (RFC 3711) with the default suite,
//! `AES_CM_128_HMAC_SHA1_80`: AES-128 in counter mode for privacy and an
//! 80-bit HMAC-SHA1 tag for authentication. Needs the `srtp` feature.
//!
//! [`Outbound`] protects what one participant sends; [`Inbound`] checks and
//! decrypts what one participant receives. Each holds the keys for a single
//! stream (one SSRC, its RTP and its RTCP), so a call has one of each per
//! direction. Both work in place on the packet buffer:
//!
//! - RTP: [`Outbound::protect_rtp`] appends a [`TAG_LEN`]-byte tag;
//!   [`Inbound::unprotect_rtp`] checks it, rejects replays, decrypts, and
//!   returns the length of the plain RTP packet to hand to
//!   [`Packet::parse`](crate::packet::Packet::parse).
//! - RTCP: likewise, with [`RTCP_OVERHEAD`] bytes added (the SRTCP index and
//!   the tag).
//!
//! # Keys
//!
//! A [`MasterKey`] is a 16-byte key plus a 14-byte salt, the 30 bytes that
//! SDP's `a=crypto:... inline:` carries in base64. Where it comes from is up
//! to the application, but **use a fresh master key for every session**.
//! AES counter mode turns the key, the SSRC and the packet index into a
//! keystream, and reusing a keystream for different audio leaks the XOR of
//! the two. A device that keeps one key in flash and restarts its sequence
//! numbers after a reboot does exactly that. A long-term device secret should
//! only authenticate the device to whoever issues session keys; [`wrap`] and
//! [`unwrap`] carry a session key under such a secret without TLS.
//!
//! [`Outbound`] never uses the same packet index twice under one key.
//! [`Inbound::rekey`] and [`Outbound::rekey`] change keys mid-stream; the
//! receiver accepts the old key until the first packet under the new one.
//!
//! The packet index starts with a rollover counter of zero, so both ends must
//! start a fresh context together: a receiver that joins an established
//! stream after its sequence number has wrapped cannot authenticate it. Fresh
//! keys per session make this the normal case anyway.
//!
//! No master key indicator (MKI) and no key derivation rate: neither is used
//! by the default suite as SDP and ffmpeg set it up.

use aes::cipher::{BlockEncrypt, InnerIvInit, KeyInit, StreamCipher};
use aes::Aes128;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::be32;

/// Length of a master key.
pub const KEY_LEN: usize = 16;
/// Length of a master salt.
pub const SALT_LEN: usize = 14;
/// Length of the authentication tag added to every packet.
pub const TAG_LEN: usize = 10;
/// Bytes SRTP adds to an RTP packet: the tag.
pub const RTP_OVERHEAD: usize = TAG_LEN;
/// Bytes SRTCP adds to an RTCP packet: the E flag and SRTCP index, then the
/// tag.
pub const RTCP_OVERHEAD: usize = 4 + TAG_LEN;
/// Packets older than this many behind the newest are rejected as possible
/// replays (RFC 3711 §3.3.2 requires at least 64).
pub const REPLAY_WINDOW: u64 = 64;
/// Length of a master key wrapped by [`wrap`].
pub const WRAPPED_LEN: usize = 40;

/// Why a packet was not protected or not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Too short to hold the headers it claims, or the tag.
    Truncated,
    /// Not RTP version 2.
    BadVersion,
    /// The buffer has no room for the tag (and SRTCP index).
    BufferTooSmall,
    /// A different SSRC from the first packet this context saw. Each
    /// context belongs to one stream.
    WrongSsrc,
    /// The tag doesn't match: tampered with, or under another key.
    Authentication,
    /// Already received, or too old to tell.
    Replayed,
    /// The packet index has run out (2^48 RTP or 2^31 RTCP packets under one
    /// key). Rekey.
    Exhausted,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Truncated => "packet truncated",
            Self::BadVersion => "not RTP version 2",
            Self::BufferTooSmall => "no room for the SRTP trailer",
            Self::WrongSsrc => "SSRC doesn't match this context",
            Self::Authentication => "authentication failed",
            Self::Replayed => "replayed or too old",
            Self::Exhausted => "packet index exhausted",
        })
    }
}

impl core::error::Error for Error {}

/// A master key and salt, from which each direction's session keys are
/// derived (RFC 3711 §4.3).
#[derive(Clone)]
pub struct MasterKey {
    /// The AES-128 master key.
    pub key: [u8; KEY_LEN],
    /// The master salt.
    pub salt: [u8; SALT_LEN],
}

impl MasterKey {
    /// From key then salt, as SDP's `inline:` parameter carries them (after
    /// base64 decoding).
    pub fn from_bytes(bytes: &[u8; KEY_LEN + SALT_LEN]) -> Self {
        let mut key = [0; KEY_LEN];
        let mut salt = [0; SALT_LEN];
        key.copy_from_slice(&bytes[..KEY_LEN]);
        salt.copy_from_slice(&bytes[KEY_LEN..]);
        Self { key, salt }
    }

    /// Key then salt, for SDP's `inline:` parameter (before base64).
    pub fn to_bytes(&self) -> [u8; KEY_LEN + SALT_LEN] {
        let mut bytes = [0; KEY_LEN + SALT_LEN];
        bytes[..KEY_LEN].copy_from_slice(&self.key);
        bytes[KEY_LEN..].copy_from_slice(&self.salt);
        bytes
    }
}

/// Keeps keys out of logs.
impl core::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

/// Wrap `key` under `kek` with AES key wrap with padding (RFC 5649), so a
/// server can send a session key to a device that shares `kek` with it,
/// without TLS. Use a key encryption key for nothing else: derive it from
/// the device secret rather than using the secret itself.
pub fn wrap(kek: &[u8; 16], key: &MasterKey) -> [u8; WRAPPED_LEN] {
    let mut out = [0; WRAPPED_LEN];
    aes_kw::KekAes128::from(*kek)
        .wrap_with_padding(&key.to_bytes(), &mut out)
        .expect("sizes are fixed");
    out
}

/// Recover a key from [`wrap`]. Fails with [`Error::Authentication`] if
/// `wrapped` was altered or wrapped under another key.
pub fn unwrap(kek: &[u8; 16], wrapped: &[u8; WRAPPED_LEN]) -> Result<MasterKey, Error> {
    let mut out = [0; WRAPPED_LEN - 8];
    let result = aes_kw::KekAes128::from(*kek)
        .unwrap_with_padding(wrapped, &mut out)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .map(MasterKey::from_bytes);
    out.fill(0);
    result.ok_or(Error::Authentication)
}

type Ctr = ctr::Ctr128BE<Aes128>;
type HmacSha1 = Hmac<Sha1>;

/// Session keys for one of RTP or RTCP (RFC 3711 §4.3.1), with the cipher's
/// key schedule and the HMAC's keyed state precomputed.
#[derive(Clone)]
struct SessionKeys {
    cipher: Aes128,
    salt: [u8; SALT_LEN],
    auth: HmacSha1,
}

impl SessionKeys {
    /// Labels 0, 1, 2 give RTP's keys; 3, 4, 5 RTCP's.
    fn derive(master: &MasterKey, first_label: u8) -> Self {
        let kdf = Aes128::new(&master.key.into());
        let mut key = [0; KEY_LEN];
        let mut auth = [0; 20];
        let mut salt = [0; SALT_LEN];
        derive_key(&kdf, &master.salt, first_label, &mut key);
        derive_key(&kdf, &master.salt, first_label + 1, &mut auth);
        derive_key(&kdf, &master.salt, first_label + 2, &mut salt);
        let keys = Self {
            cipher: Aes128::new(&key.into()),
            salt,
            auth: <HmacSha1 as Mac>::new_from_slice(&auth).expect("HMAC takes any key length"),
        };
        key.fill(0);
        auth.fill(0);
        keys
    }

    /// AES-CM (§4.1.1) over `data` for the packet `index` of `ssrc`.
    fn apply_keystream(&self, ssrc: u32, index: u64, data: &mut [u8]) {
        let mut iv = [0u8; 16];
        iv[..SALT_LEN].copy_from_slice(&self.salt);
        for (byte, x) in iv[4..8].iter_mut().zip(ssrc.to_be_bytes()) {
            *byte ^= x;
        }
        for (byte, x) in iv[8..14].iter_mut().zip(&index.to_be_bytes()[2..]) {
            *byte ^= x;
        }
        let mut ctr = Ctr::from_core(ctr::CtrCore::inner_iv_init(self.cipher.clone(), &iv.into()));
        ctr.apply_keystream(data);
    }

    /// The HMAC over `data` then `trailer`, ready to finalize or verify.
    fn mac(&self, data: &[u8], trailer: &[u8]) -> HmacSha1 {
        let mut mac = self.auth.clone();
        mac.update(data);
        mac.update(trailer);
        mac
    }

    fn tag(&self, data: &[u8], trailer: &[u8]) -> [u8; TAG_LEN] {
        let full = self.mac(data, trailer).finalize().into_bytes();
        let mut tag = [0; TAG_LEN];
        tag.copy_from_slice(&full[..TAG_LEN]);
        tag
    }

    /// Constant-time check of an 80-bit tag.
    fn verify(&self, data: &[u8], trailer: &[u8], tag: &[u8]) -> bool {
        self.mac(data, trailer).verify_truncated_left(tag).is_ok()
    }
}

/// The key derivation PRF (§4.3.1, key derivation rate zero): AES-CM under
/// the master key, starting from the master salt with the label in byte 7.
fn derive_key(kdf: &Aes128, salt: &[u8; SALT_LEN], label: u8, out: &mut [u8]) {
    let mut iv = [0u8; 16];
    iv[..SALT_LEN].copy_from_slice(salt);
    iv[7] ^= label;
    for (i, chunk) in out.chunks_mut(16).enumerate() {
        let mut block = iv;
        block[14..].copy_from_slice(&(i as u16).to_be_bytes());
        let mut block = block.into();
        kdf.encrypt_block(&mut block);
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
}

/// Both halves of a stream's keys.
#[derive(Clone)]
struct Keys {
    rtp: SessionKeys,
    rtcp: SessionKeys,
}

impl Keys {
    fn derive(master: &MasterKey) -> Self {
        Self {
            rtp: SessionKeys::derive(master, 0),
            rtcp: SessionKeys::derive(master, 3),
        }
    }

    fn session(&self, rtcp: bool) -> &SessionKeys {
        if rtcp {
            &self.rtcp
        } else {
            &self.rtp
        }
    }
}

/// Which keys authenticated a packet.
#[derive(Clone, Copy)]
enum Generation {
    Current,
    Previous,
}

/// The length of an RTP header including CSRCs and any extension: what SRTP
/// leaves unencrypted.
fn rtp_header_len(packet: &[u8]) -> Result<usize, Error> {
    if packet.len() < 12 {
        return Err(Error::Truncated);
    }
    if packet[0] >> 6 != 2 {
        return Err(Error::BadVersion);
    }
    let mut len = 12 + 4 * usize::from(packet[0] & 0x0f);
    if packet[0] & 0x10 != 0 {
        if packet.len() < len + 4 {
            return Err(Error::Truncated);
        }
        len += 4 + 4 * usize::from(u16::from_be_bytes([packet[len + 2], packet[len + 3]]));
    }
    if packet.len() < len {
        return Err(Error::Truncated);
    }
    Ok(len)
}

/// The first SSRC this context saw, which it then sticks to.
fn check_ssrc(bound: Option<u32>, ssrc: u32) -> Result<(), Error> {
    match bound {
        Some(expected) if expected != ssrc => Err(Error::WrongSsrc),
        _ => Ok(()),
    }
}

const MAX_RTP_INDEX: u64 = (1 << 48) - 1;
const MAX_RTCP_INDEX: u32 = (1 << 31) - 1;
const E_FLAG: u32 = 1 << 31;

/// Protects one outgoing stream: its RTP and its RTCP.
///
/// Deliberately not `Clone`: two copies would hand out the same packet
/// indexes under the same key, reusing keystream.
pub struct Outbound {
    keys: Keys,
    ssrc: Option<u32>,
    /// The last RTP packet index used, once there is one.
    rtp_index: Option<u64>,
    /// The next SRTCP index.
    rtcp_index: u32,
}

impl Outbound {
    /// Derive the session keys for a stream from its master key.
    pub fn new(master: &MasterKey) -> Self {
        Self {
            keys: Keys::derive(master),
            ssrc: None,
            rtp_index: None,
            rtcp_index: 0,
        }
    }

    /// Switch to a new master key. Packet indexes carry on.
    pub fn rekey(&mut self, master: &MasterKey) {
        self.keys = Keys::derive(master);
    }

    /// Encrypt the RTP packet in `buf[..len]` and append its tag. Returns the
    /// SRTP packet's length, `len + RTP_OVERHEAD`.
    ///
    /// Packet indexes only move forward: a sequence number that isn't after
    /// the last one is taken to have wrapped, so a keystream is never used
    /// twice.
    pub fn protect_rtp(&mut self, buf: &mut [u8], len: usize) -> Result<usize, Error> {
        let packet = buf.get(..len).ok_or(Error::Truncated)?;
        let header = rtp_header_len(packet)?;
        if buf.len() < len + TAG_LEN {
            return Err(Error::BufferTooSmall);
        }
        let ssrc = be32(buf, 8);
        check_ssrc(self.ssrc, ssrc)?;
        let sequence = u64::from(u16::from_be_bytes([buf[2], buf[3]]));
        let index = match self.rtp_index {
            None => sequence,
            Some(last) => {
                let index = (last & !0xffff) | sequence;
                if index > last {
                    index
                } else {
                    index + 0x1_0000
                }
            }
        };
        if index > MAX_RTP_INDEX {
            return Err(Error::Exhausted);
        }
        self.ssrc = Some(ssrc);
        self.rtp_index = Some(index);

        let rtp = &self.keys.rtp;
        rtp.apply_keystream(ssrc, index, &mut buf[header..len]);
        let roc = ((index >> 16) as u32).to_be_bytes();
        let tag = rtp.tag(&buf[..len], &roc);
        buf[len..len + TAG_LEN].copy_from_slice(&tag);
        Ok(len + TAG_LEN)
    }

    /// Encrypt the (compound) RTCP packet in `buf[..len]` and append the
    /// SRTCP index and tag. Returns the SRTCP packet's length,
    /// `len + RTCP_OVERHEAD`.
    pub fn protect_rtcp(&mut self, buf: &mut [u8], len: usize) -> Result<usize, Error> {
        if len < 8 || buf.len() < len {
            return Err(Error::Truncated);
        }
        if buf[0] >> 6 != 2 {
            return Err(Error::BadVersion);
        }
        if buf.len() < len + RTCP_OVERHEAD {
            return Err(Error::BufferTooSmall);
        }
        let index = self.rtcp_index;
        if index > MAX_RTCP_INDEX {
            return Err(Error::Exhausted);
        }
        self.rtcp_index += 1;

        let rtcp = &self.keys.rtcp;
        let ssrc = be32(buf, 4);
        rtcp.apply_keystream(ssrc, u64::from(index), &mut buf[8..len]);
        buf[len..len + 4].copy_from_slice(&(E_FLAG | index).to_be_bytes());
        let tag = rtcp.tag(&buf[..len + 4], &[]);
        buf[len + 4..len + RTCP_OVERHEAD].copy_from_slice(&tag);
        Ok(len + RTCP_OVERHEAD)
    }
}

/// The newest index accepted and which of the [`REPLAY_WINDOW`] before it
/// have been seen (bit `n` is `newest - n`).
#[derive(Clone, Copy)]
struct ReplayWindow {
    newest: u64,
    seen: u64,
}

impl ReplayWindow {
    fn check(window: Option<Self>, index: u64) -> Result<(), Error> {
        let Some(window) = window else {
            return Ok(());
        };
        if index > window.newest {
            return Ok(());
        }
        let age = window.newest - index;
        if age >= REPLAY_WINDOW || window.seen & (1 << age) != 0 {
            return Err(Error::Replayed);
        }
        Ok(())
    }

    fn accept(window: &mut Option<Self>, index: u64) {
        *window = Some(match *window {
            None => Self {
                newest: index,
                seen: 1,
            },
            Some(Self { newest, seen }) if index > newest => {
                let shift = index - newest;
                Self {
                    newest: index,
                    seen: if shift >= REPLAY_WINDOW {
                        1
                    } else {
                        (seen << shift) | 1
                    },
                }
            }
            Some(Self { newest, seen }) => Self {
                newest,
                seen: seen | 1 << (newest - index),
            },
        });
    }
}

/// Checks and decrypts one incoming stream: its RTP and its RTCP.
///
/// Nothing is accepted until its tag checks out, and a rejected packet
/// changes no state, so forged packets can't disturb the stream.
///
/// Not `Clone`, so each packet can be accepted only once.
pub struct Inbound {
    keys: Keys,
    /// The keys before the last [`Inbound::rekey`], until a packet arrives
    /// under the new ones.
    previous: Option<Keys>,
    ssrc: Option<u32>,
    rtp: Option<ReplayWindow>,
    rtcp: Option<ReplayWindow>,
}

impl Inbound {
    /// Derive the session keys for a stream from its master key.
    pub fn new(master: &MasterKey) -> Self {
        Self {
            keys: Keys::derive(master),
            previous: None,
            ssrc: None,
            rtp: None,
            rtcp: None,
        }
    }

    /// Switch to a new master key. Packets under the old key are still
    /// accepted until the first one under the new key arrives, so the sender
    /// can switch whenever it likes.
    pub fn rekey(&mut self, master: &MasterKey) {
        self.previous = Some(core::mem::replace(&mut self.keys, Keys::derive(master)));
    }

    /// Which keys authenticate a packet: the current ones, else the
    /// previous ones. Settles a pending rekey once the current ones work.
    fn authenticate(
        &mut self,
        rtcp: bool,
        data: &[u8],
        trailer: &[u8],
        tag: &[u8],
    ) -> Result<Generation, Error> {
        if self.keys.session(rtcp).verify(data, trailer, tag) {
            self.previous = None;
            return Ok(Generation::Current);
        }
        match &self.previous {
            Some(previous) if previous.session(rtcp).verify(data, trailer, tag) => {
                Ok(Generation::Previous)
            }
            _ => Err(Error::Authentication),
        }
    }

    fn keys(&self, generation: Generation, rtcp: bool) -> &SessionKeys {
        match (generation, &self.previous) {
            (Generation::Previous, Some(previous)) => previous.session(rtcp),
            _ => self.keys.session(rtcp),
        }
    }

    /// Check and decrypt the SRTP packet in `buf` in place. Returns the
    /// length of the RTP packet now at the start of `buf`.
    pub fn unprotect_rtp(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let len = buf.len().checked_sub(TAG_LEN).ok_or(Error::Truncated)?;
        let header = rtp_header_len(&buf[..len])?;
        let ssrc = be32(buf, 8);
        check_ssrc(self.ssrc, ssrc)?;
        let sequence = u16::from_be_bytes([buf[2], buf[3]]);
        let index = estimate_index(self.rtp.map(|w| w.newest), sequence)?;
        ReplayWindow::check(self.rtp, index)?;

        let roc = ((index >> 16) as u32).to_be_bytes();
        let (packet, tag) = buf.split_at(len);
        let generation = self.authenticate(false, packet, &roc, tag)?;
        self.ssrc = Some(ssrc);
        ReplayWindow::accept(&mut self.rtp, index);
        self.keys(generation, false)
            .apply_keystream(ssrc, index, &mut buf[header..len]);
        Ok(len)
    }

    /// Check and decrypt the SRTCP packet in `buf` in place. Returns the
    /// length of the (compound) RTCP packet now at the start of `buf`.
    ///
    /// Only encrypted SRTCP is accepted. Packets with the E flag clear
    /// (authentication only, which RFC 3711 allows but this crate never
    /// sends) fail with [`Error::Authentication`].
    pub fn unprotect_rtcp(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let len = buf
            .len()
            .checked_sub(RTCP_OVERHEAD)
            .filter(|&len| len >= 8)
            .ok_or(Error::Truncated)?;
        if buf[0] >> 6 != 2 {
            return Err(Error::BadVersion);
        }
        let ssrc = be32(buf, 4);
        check_ssrc(self.ssrc, ssrc)?;
        let word = be32(buf, len);
        if word & E_FLAG == 0 {
            return Err(Error::Authentication);
        }
        let index = u64::from(word & !E_FLAG);
        ReplayWindow::check(self.rtcp, index)?;

        let (packet, tag) = buf.split_at(len + 4);
        let generation = self.authenticate(true, packet, &[], tag)?;
        self.ssrc = Some(ssrc);
        ReplayWindow::accept(&mut self.rtcp, index);
        self.keys(generation, true)
            .apply_keystream(ssrc, index, &mut buf[8..len]);
        Ok(len)
    }
}

/// The packet index for `sequence`: the one closest to the newest index
/// accepted (RFC 3711 §3.3.1 and appendix A). Before any packet, the
/// rollover counter is zero.
fn estimate_index(newest: Option<u64>, sequence: u16) -> Result<u64, Error> {
    let Some(newest) = newest else {
        return Ok(u64::from(sequence));
    };
    let roc = newest >> 16;
    let s_l = newest as u16;
    let roc = if s_l < 0x8000 {
        if sequence > s_l && sequence - s_l > 0x8000 {
            roc.checked_sub(1).ok_or(Error::Replayed)?
        } else {
            roc
        }
    } else if sequence < s_l - 0x8000 {
        roc + 1
    } else {
        roc
    };
    let index = roc << 16 | u64::from(sequence);
    if index > MAX_RTP_INDEX {
        return Err(Error::Exhausted);
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 3711 appendix B.2: the AES-CM keystream for a given session key
    /// and salt, SSRC 0 and index 0. The first two blocks, as also carried
    /// in libsrtp's `aes_icm_128_test_case_0`.
    #[test]
    fn rfc3711_b2_keystream() {
        let keys = SessionKeys {
            cipher: Aes128::new(
                &[
                    0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09,
                    0xcf, 0x4f, 0x3c,
                ]
                .into(),
            ),
            salt: [
                0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd,
            ],
            auth: <HmacSha1 as Mac>::new_from_slice(&[]).unwrap(),
        };
        let mut keystream = [0u8; 32];
        keys.apply_keystream(0, 0, &mut keystream);
        assert_eq!(
            keystream,
            [
                0xe0, 0x3e, 0xad, 0x09, 0x35, 0xc9, 0x5e, 0x80, 0xe1, 0x66, 0xb1, 0x6d, 0xd9, 0x2b,
                0x4e, 0xb4, 0xd2, 0x35, 0x13, 0x16, 0x2b, 0x02, 0xd0, 0xf7, 0x2a, 0x43, 0xa2, 0xfe,
                0x4a, 0x5f, 0x97, 0xab,
            ]
        );
    }

    /// RFC 3711 appendix B.3: session keys derived from a master key.
    #[test]
    fn rfc3711_b3_key_derivation() {
        let key = [
            0xe1, 0xf9, 0x7a, 0x0d, 0x3e, 0x01, 0x8b, 0xe0, 0xd6, 0x4f, 0xa3, 0x2c, 0x06, 0xde,
            0x41, 0x39,
        ];
        let salt = [
            0x0e, 0xc6, 0x75, 0xad, 0x49, 0x8a, 0xfe, 0xeb, 0xb6, 0x96, 0x0b, 0x3a, 0xab, 0xe6,
        ];
        let kdf = Aes128::new(&key.into());

        let mut cipher_key = [0u8; 16];
        derive_key(&kdf, &salt, 0, &mut cipher_key);
        assert_eq!(
            cipher_key,
            [
                0xc6, 0x1e, 0x7a, 0x93, 0x74, 0x4f, 0x39, 0xee, 0x10, 0x73, 0x4a, 0xfe, 0x3f, 0xf7,
                0xa0, 0x87,
            ]
        );
        let mut session_salt = [0u8; 14];
        derive_key(&kdf, &salt, 2, &mut session_salt);
        assert_eq!(
            session_salt,
            [0x30, 0xcb, 0xbc, 0x08, 0x86, 0x3d, 0x8c, 0x85, 0xd4, 0x9d, 0xb3, 0x4a, 0x9a, 0xe1]
        );
        let mut auth_key = [0u8; 20];
        derive_key(&kdf, &salt, 1, &mut auth_key);
        assert_eq!(
            auth_key,
            [
                0xce, 0xbe, 0x32, 0x1f, 0x6f, 0xf7, 0x71, 0x6b, 0x6f, 0xd4, 0xab, 0x49, 0xaf, 0x25,
                0x6a, 0x15, 0x6d, 0x38, 0xba, 0xa4,
            ]
        );
    }

    /// Appendix A's estimate, around the wrap in both directions.
    #[test]
    fn index_estimate() {
        assert_eq!(estimate_index(None, 7), Ok(7));
        // Just past a wrap: the next rollover.
        assert_eq!(estimate_index(Some(0xfffe), 1), Ok(0x1_0001));
        // Late packet from before the wrap.
        assert_eq!(estimate_index(Some(0x1_0002), 0xfffd), Ok(0xfffd));
        // Before the first wrap there is nothing earlier.
        assert_eq!(estimate_index(Some(5), 0xfff0), Err(Error::Replayed));
        // Ordinary steps.
        assert_eq!(estimate_index(Some(0x3_1234), 0x1235), Ok(0x3_1235));
        assert_eq!(estimate_index(Some(0x3_9234), 0x9200), Ok(0x3_9200));
    }
}
