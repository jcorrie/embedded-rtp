//! Interop with independent RTP implementations: ffmpeg and GStreamer.
//!
//! These need the tools installed, so they are `#[ignore]`d by default. Run
//! them with `cargo test --test interop -- --ignored`; CI does so in a job
//! that installs both. Unix only, since they stop ffmpeg with SIGINT.
//!
//! `UPDATE_FIXTURES=1` makes the ffmpeg capture test rewrite
//! `tests/fixtures/ffmpeg-l16-16000.bin`.

#![cfg(unix)]

mod common;

use std::fs;
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{l16_bytes, l16_packet, l16_samples, test_signal};
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::packet::{self, Packet};
use embedded_rtp::profile::{CLOCK_RATE, FRAME_SAMPLES, PAYLOAD_TYPE};
use embedded_rtp::stats::{SeqStatus, SourceStats, MIN_SEQUENTIAL};
use embedded_rtp::{rtcp, Receiver, Sender};

#[cfg(feature = "srtp")]
#[path = "../examples/common/intercom.rs"]
mod intercom;

/// Kills the child if a test fails before cleaning it up itself.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Running {
    /// Ask the child to finish cleanly (ffmpeg flushes its output on SIGINT)
    /// and wait for it.
    fn interrupt(mut self) {
        let _ = Command::new("kill")
            .args(["-INT", &self.0.id().to_string()])
            .status();
        let _ = self.0.wait();
    }
}

fn spawn(program: &str, args: &[&str]) -> Running {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("could not run {program} (is it installed?): {e}"));
    Running(child)
}

/// A scratch directory unique to this test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("embedded-rtp-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A port P with P+1 also free, for RTP plus RTCP. Racy in principle, but the
/// window between probing and the tool binding is tiny.
fn free_port_pair() -> u16 {
    for _ in 0..100 {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        if port < u16::MAX && UdpSocket::bind(("127.0.0.1", port + 1)).is_ok() {
            return port;
        }
    }
    panic!("no free UDP port pair");
}

/// Wait until some other process has bound `port`, i.e. is ready to receive.
fn wait_until_bound(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if is_bound(port) {
            return;
        }
        sleep(Duration::from_millis(50));
    }
    panic!("nothing bound UDP port {port}");
}

/// Whether a UDP port is bound. On Linux, read the socket table rather than
/// probing with a bind, which could briefly steal the port from the tool.
fn is_bound(port: u16) -> bool {
    let tables: Vec<String> = ["/proc/net/udp", "/proc/net/udp6"]
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .collect();
    if tables.is_empty() {
        return matches!(UdpSocket::bind(("0.0.0.0", port)), Err(e) if e.kind() == ErrorKind::AddrInUse);
    }
    let needle = format!(":{port:04X} ");
    tables
        .iter()
        .flat_map(|table| table.lines().skip(1))
        .any(|line| {
            line.split_whitespace()
                .nth(1)
                .is_some_and(|local| format!("{local} ").ends_with(&needle))
        })
}

/// Have ffmpeg packetize `input` as L16/16000/1 and capture the datagrams,
/// plus the SDP ffmpeg wrote to describe them.
fn ffmpeg_capture(name: &str, input: &[i16]) -> (Vec<Vec<u8>>, String) {
    let dir = scratch(name);
    let input_path = dir.join("in.raw");
    let sdp_path = dir.join("out.sdp");
    fs::write(&input_path, l16_bytes(input)).unwrap();

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let url = format!("rtp://127.0.0.1:{}", socket.local_addr().unwrap().port());

    let ffmpeg = spawn(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "s16be",
            "-ar",
            "16000",
            "-ac",
            "1",
            "-i",
            input_path.to_str().unwrap(),
            "-c:a",
            "pcm_s16be",
            "-payload_type",
            "96",
            "-f",
            "rtp",
            &url,
            "-sdp_file",
            sdp_path.to_str().unwrap(),
        ],
    );

    let mut datagrams = Vec::new();
    let mut total = 0;
    let mut buf = [0u8; 2048];
    while total < input.len() * 2 {
        let (len, _) = socket
            .recv_from(&mut buf)
            .expect("ffmpeg stopped sending early");
        let datagram = buf[..len].to_vec();
        total += Packet::parse(&datagram).unwrap().payload.len();
        datagrams.push(datagram);
    }
    drop(ffmpeg);

    let sdp = fs::read_to_string(&sdp_path).unwrap();
    (datagrams, sdp)
}

/// ffmpeg packetizes known audio; we must recover it exactly, with the
/// sequence and timestamp behaviour RFC 3550 §5.1 requires.
#[test]
#[ignore = "needs ffmpeg"]
fn ffmpeg_sends_l16_we_parse() {
    let input = test_signal(CLOCK_RATE as usize, 0); // one second
    let (datagrams, sdp) = ffmpeg_capture("ffmpeg-send", &input);

    // ffmpeg's own description of the stream is our profile, word for word.
    assert!(sdp.contains("a=rtpmap:96 L16/16000/1"), "{sdp}");
    assert!(sdp.contains("RTP/AVP 96"), "{sdp}");

    let first = Packet::parse(&datagrams[0]).unwrap().header;
    let mut received = Vec::new();
    let mut expected_seq = first.sequence;
    let mut expected_ts = first.timestamp;
    for datagram in &datagrams {
        assert!(!packet::is_rtcp(datagram));
        let packet = Packet::parse(datagram).unwrap();
        assert_eq!(packet.header.payload_type, PAYLOAD_TYPE);
        assert_eq!(packet.header.ssrc, first.ssrc);
        assert_eq!(
            packet.header.sequence, expected_seq,
            "sequence numbers step by one"
        );
        assert_eq!(
            packet.header.timestamp, expected_ts,
            "timestamps step by samples sent"
        );
        let samples = l16_samples(packet.payload);
        expected_seq = expected_seq.wrapping_add(1);
        expected_ts = expected_ts.wrapping_add(samples.len() as u32);
        received.extend(samples);
    }
    assert_eq!(received, input, "L16 is big endian, sample for sample");

    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        let mut fixture = Vec::new();
        for datagram in datagrams.iter().take(3) {
            fixture.extend_from_slice(&(datagram.len() as u16).to_be_bytes());
            fixture.extend_from_slice(datagram);
        }
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ffmpeg-l16-16000.bin");
        fs::write(path, fixture).unwrap();
    }
}

/// Our packets, described by an SDP file, must decode in ffmpeg back to the
/// exact audio: byte order, payload type, clock rate and sequence numbers all
/// have to be right for that. The stream crosses both the sequence number and
/// the timestamp wrap, loses frames and reorders two.
#[test]
#[ignore = "needs ffmpeg"]
fn ffmpeg_decodes_our_packets() {
    const FRAMES: usize = 200;
    let dropped = [50, 51, 120];
    let dir = scratch("ffmpeg-receive");
    let port = free_port_pair();
    let sdp_path = dir.join("in.sdp");
    let output_path = dir.join("out.raw");
    fs::write(
        &sdp_path,
        format!(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=embedded-rtp\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
             m=audio {port} RTP/AVP 96\r\na=rtpmap:96 L16/16000/1\r\na=ptime:20\r\n"
        ),
    )
    .unwrap();

    let ffmpeg = spawn(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-protocol_whitelist",
            "file,udp,rtp",
            "-i",
            sdp_path.to_str().unwrap(),
            "-f",
            "s16be",
            "-y",
            output_path.to_str().unwrap(),
        ],
    );
    wait_until_bound(port);

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let first_seq: u16 = 65_500;
    let first_ts: u32 = u32::MAX - 20 * FRAME_SAMPLES as u32;
    let ssrc = 0x5eed_cafe;
    let frames: Vec<Vec<i16>> = (0..FRAMES)
        .map(|i| test_signal(FRAME_SAMPLES, i * FRAME_SAMPLES))
        .collect();

    let mut order: Vec<usize> = (0..FRAMES).filter(|i| !dropped.contains(i)).collect();
    let swap_at = order.iter().position(|&i| i == 150).unwrap();
    order.swap(swap_at, swap_at + 1);

    for &i in &order {
        let seq = first_seq.wrapping_add(i as u16);
        let ts = first_ts.wrapping_add((i * FRAME_SAMPLES) as u32);
        socket
            .send_to(&l16_packet(seq, ts, ssrc, &frames[i]), ("127.0.0.1", port))
            .unwrap();
        sleep(Duration::from_millis(1));
    }
    sleep(Duration::from_millis(500));
    ffmpeg.interrupt();

    let decoded = l16_samples(&fs::read(&output_path).unwrap());
    let expected: Vec<i16> = (0..FRAMES)
        .filter(|i| !dropped.contains(i))
        .flat_map(|i| frames[i].iter().copied())
        .collect();
    assert_eq!(decoded.len(), expected.len(), "frame count");
    assert!(
        decoded == expected,
        "decoded audio differs from what was sent"
    );
}

/// What GStreamer's RTCP receiver report says about our stream must agree
/// with our own `SourceStats` fed the same packets.
///
/// GStreamer differs from the RFC 3550 A.1 reference code in one documented
/// way: when probation ends, `init_seq` in `rtpsource.c` also counts the
/// packets it held during probation as received, while the base sequence
/// number is the packet that ended probation. It therefore reports exactly
/// `MIN_SEQUENTIAL - 1` fewer packets lost than the reference code, and we
/// follow the reference code.
#[test]
#[ignore = "needs GStreamer (gst-launch-1.0 with rtpmanager)"]
fn gstreamer_receiver_reports_match_our_statistics() {
    const PACKETS: usize = 600;
    let dropped = [100, 101, 102, 350, 351];
    let duplicated = [200, 420];
    let rtp_port = free_port_pair();
    let reports = UdpSocket::bind("127.0.0.1:0").unwrap();
    reports
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let report_port = reports.local_addr().unwrap().port();

    let gst = spawn_gstreamer_receiver(rtp_port, report_port);
    wait_until_bound(rtp_port);

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let ssrc = 0x1122_3344;
    let first_seq: u16 = 65_000; // wraps partway through
    let mut order: Vec<usize> = Vec::new();
    for i in (0..PACKETS).filter(|i| !dropped.contains(i)) {
        order.push(i);
        if duplicated.contains(&i) {
            order.push(i);
        }
    }
    let swap_at = order.iter().position(|&i| i == 300).unwrap();
    order.swap(swap_at, swap_at + 1);

    let mut ours = SourceStats::new();
    let payload = test_signal(FRAME_SAMPLES, 0);
    for &i in &order {
        let seq = first_seq.wrapping_add(i as u16);
        let ts = (i * FRAME_SAMPLES) as u32;
        socket
            .send_to(
                &l16_packet(seq, ts, ssrc, &payload),
                ("127.0.0.1", rtp_port),
            )
            .unwrap();
        assert_ne!(ours.update(seq), SeqStatus::Invalid);
        sleep(Duration::from_millis(2));
    }
    let sent_at = Instant::now();
    let ours = ours.report();

    // The first report sent after the last packet has seen the whole stream.
    let deadline = sent_at + Duration::from_secs(20);
    let theirs = loop {
        assert!(
            Instant::now() < deadline,
            "no receiver report from GStreamer"
        );
        let mut buf = [0u8; 1500];
        let Ok((len, _)) = reports.recv_from(&mut buf) else {
            continue;
        };
        if sent_at.elapsed() < Duration::from_millis(300) {
            continue;
        }
        if let Some(block) = report_about(&buf[..len], ssrc) {
            break block;
        }
    };
    drop(gst);

    assert_eq!(theirs.extended_highest, ours.extended_highest);
    assert_eq!(
        theirs.cumulative_lost,
        ours.cumulative_lost - i32::from(MIN_SEQUENTIAL - 1),
        "ours {ours:?}, GStreamer {theirs:?}"
    );
}

/// GStreamer's `rtpbin` receiving L16/16000/1 RTP on `rtp_port` and RTCP on
/// `rtp_port + 1`, sending its own RTCP to `report_port`.
fn spawn_gstreamer_receiver(rtp_port: u16, report_port: u16) -> Running {
    let caps =
        "application/x-rtp,media=audio,clock-rate=16000,encoding-name=L16,channels=1,payload=96";
    spawn(
        "gst-launch-1.0",
        &[
            "-q",
            "rtpbin",
            "name=rb",
            "udpsrc",
            &format!("port={rtp_port}"),
            &format!("caps={caps}"),
            "!",
            "rb.recv_rtp_sink_0",
            // The depayloader's caps make gst-launch link the RTP pad here,
            // not the first rtpbin pad that appears.
            "rb.",
            "!",
            "rtpL16depay",
            "!",
            "fakesink",
            "sync=false",
            "udpsrc",
            &format!("port={}", rtp_port + 1),
            "!",
            "rb.recv_rtcp_sink_0",
            "rb.send_rtcp_src_0",
            "!",
            "udpsink",
            "host=127.0.0.1",
            &format!("port={report_port}"),
            "sync=false",
            "async=false",
        ],
    )
}

/// The report block about `ssrc` in GStreamer's compound RTCP, read with the
/// crate's parser. GStreamer's packets must pass its RFC 3550 A.2 checks and
/// carry an SDES CNAME, as §6.1 requires.
fn report_about(compound: &[u8], ssrc: u32) -> Option<rtcp::ReportBlock> {
    let compound = rtcp::Compound::parse(compound)
        .unwrap_or_else(|e| panic!("GStreamer's RTCP rejected: {e:?} {compound:02x?}"));
    let mut cname = false;
    let mut found = None;
    for packet in compound.packets() {
        match packet {
            rtcp::Packet::SenderReport { reports, .. }
            | rtcp::Packet::ReceiverReport { reports, .. } => {
                found = found.or(reports.into_iter().find(|b| b.ssrc == ssrc));
            }
            rtcp::Packet::SourceDescription(chunks) => {
                cname |= chunks.into_iter().any(|c| c.cname().is_some());
            }
            _ => {}
        }
    }
    assert!(cname, "compound RTCP without an SDES CNAME");
    found
}

/// GStreamer must parse our sender report: the receiver report it sends
/// next echoes our SR's NTP timestamp as "last SR" (RFC 3550 §6.4.1), and
/// the round trip worked out from it is small and positive on loopback.
#[test]
#[ignore = "needs GStreamer (gst-launch-1.0 with rtpmanager)"]
fn gstreamer_echoes_our_sender_reports() {
    let rtp_port = free_port_pair();
    let reports = UdpSocket::bind("127.0.0.1:0").unwrap();
    reports
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let gst = spawn_gstreamer_receiver(rtp_port, reports.local_addr().unwrap().port());
    wait_until_bound(rtp_port);

    // Our clock for NTP times: microseconds since the test began, offset so
    // the NTP short form is never zero (zero means "no SR").
    let epoch = Instant::now();
    let now = || rtcp::ntp_from_micros(10_000_000 + epoch.elapsed().as_micros() as u64);

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut sender = Sender::new(0x7357_0001, PAYLOAD_TYPE, FRAME_SAMPLES as u32, 1, 0);
    let mut sent_reports = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut frame = 0u32;
    let (block, arrival) = loop {
        assert!(
            Instant::now() < deadline,
            "no echo of our SR from GStreamer"
        );
        // One frame every 20 ms, and an SR + SDES every second.
        let mut packet = [0u8; 700];
        let len = sender
            .send(&l16_bytes(&test_signal(FRAME_SAMPLES, 0)), &mut packet)
            .unwrap();
        socket
            .send_to(&packet[..len], ("127.0.0.1", rtp_port))
            .unwrap();
        if frame % 50 == 10 {
            let ntp = now();
            let mut buf = [0u8; 200];
            let mut writer = rtcp::Writer::new(&mut buf);
            writer
                .sender_report(sender.ssrc(), &sender.sender_info(ntp), &[])
                .unwrap();
            writer
                .source_description(sender.ssrc(), &[(rtcp::sdes::CNAME, b"embedded-rtp@test")])
                .unwrap();
            socket
                .send_to(writer.finish(), ("127.0.0.1", rtp_port + 1))
                .unwrap();
            sent_reports.push(rtcp::ntp_short(ntp));
        }
        frame += 1;
        let mut buf = [0u8; 1500];
        if let Ok((len, _)) = reports.recv_from(&mut buf) {
            let arrival = rtcp::ntp_short(now());
            if let Some(block) = report_about(&buf[..len], sender.ssrc()) {
                if block.last_sr != 0 {
                    break (block, arrival);
                }
            }
        }
    };
    drop(gst);

    assert!(
        sent_reports.contains(&block.last_sr),
        "GStreamer echoed {:#x}, we sent {sent_reports:x?}",
        block.last_sr
    );
    let rtt = rtcp::round_trip(arrival, block.last_sr, block.delay_since_last_sr)
        .expect("a consistent round trip");
    // Loopback: well under 100 ms, allowing for the poll interval.
    assert!(rtt < 65_536 / 10, "round trip {rtt} / 65536 s");
}

/// ffmpeg's own RTCP must parse, and add up to the RTP stream it sent
/// alongside: the final sender report counts exactly the packets and payload
/// octets we received, under the same SSRC, which its SDES and BYE name too.
#[test]
#[ignore = "needs ffmpeg"]
fn ffmpeg_rtcp_matches_its_stream() {
    let port = free_port_pair();
    let rtp = UdpSocket::bind(("127.0.0.1", port)).unwrap();
    let rtcp_socket = UdpSocket::bind(("127.0.0.1", port + 1)).unwrap();
    rtp.set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    rtcp_socket
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let url = format!("rtp://127.0.0.1:{port}");
    let ffmpeg = spawn(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=16000:duration=1.5",
            "-ac",
            "1",
            "-c:a",
            "pcm_s16be",
            "-payload_type",
            "96",
            "-cname",
            "embedded-rtp-test",
            "-rtpflags",
            "send_bye",
            "-f",
            "rtp",
            &url,
        ],
    );

    let mut packets = 0u32;
    let mut octets = 0u32;
    let mut ssrc = None;
    let mut datagrams = Vec::new();
    let mut quiet_since = Instant::now();
    let mut buf = [0u8; 2048];
    while quiet_since.elapsed() < Duration::from_secs(2) {
        if let Ok((len, _)) = rtp.recv_from(&mut buf) {
            let packet = Packet::parse(&buf[..len]).unwrap();
            packets += 1;
            octets += packet.payload.len() as u32;
            ssrc = Some(packet.header.ssrc);
            quiet_since = Instant::now();
        }
        if let Ok((len, _)) = rtcp_socket.recv_from(&mut buf) {
            assert!(packet::is_rtcp(&buf[..len]));
            datagrams.push(buf[..len].to_vec());
            quiet_since = Instant::now();
        }
    }
    drop(ffmpeg);
    let ssrc = ssrc.expect("no RTP from ffmpeg");
    assert!(datagrams.len() >= 2, "expected an opening and a closing SR");

    let mut last_info = None;
    let mut bye = false;
    for datagram in &datagrams {
        let compound = rtcp::Compound::parse(datagram)
            .unwrap_or_else(|e| panic!("ffmpeg's RTCP rejected: {e:?} {datagram:02x?}"));
        for packet in compound.packets() {
            match packet {
                rtcp::Packet::SenderReport {
                    ssrc: from, info, ..
                } => {
                    assert_eq!(from, ssrc);
                    last_info = Some(info);
                }
                rtcp::Packet::SourceDescription(chunks) => {
                    for chunk in chunks {
                        assert_eq!(chunk.ssrc, ssrc);
                        assert_eq!(chunk.cname(), Some(&b"embedded-rtp-test"[..]));
                    }
                }
                rtcp::Packet::Goodbye { sources, .. } => {
                    assert_eq!(sources.collect::<Vec<_>>(), [ssrc]);
                    bye = true;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    let info = last_info.expect("no sender report");
    assert_eq!(
        info.packet_count, packets,
        "SR packet count vs RTP received"
    );
    assert_eq!(
        info.octet_count, octets,
        "SR octet count vs payload received"
    );
    assert!(bye, "no BYE despite send_bye");

    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        let mut fixture = Vec::new();
        for datagram in &datagrams {
            fixture.extend_from_slice(&(datagram.len() as u16).to_be_bytes());
            fixture.extend_from_slice(datagram);
        }
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ffmpeg-rtcp.bin");
        fs::write(path, fixture).unwrap();
    }
}

/// Write an SDP file describing an L16/16000/1 stream on `port`.
fn write_sdp(dir: &std::path::Path, port: u16) -> PathBuf {
    let path = dir.join("in.sdp");
    fs::write(
        &path,
        format!(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=embedded-rtp\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
             m=audio {port} RTP/AVP 96\r\na=rtpmap:96 L16/16000/1\r\na=ptime:20\r\n"
        ),
    )
    .unwrap();
    path
}

/// A `Sender` stream that suppresses silence must be understood by ffmpeg as
/// audio with gaps of exactly the right length: sequence numbers stay
/// contiguous while timestamps jump (RFC 3551 §4.1). `aresample=async=1`
/// makes ffmpeg fill timestamp gaps with silence, sample-accurately, so the
/// decoded output shows whether every timestamp was right.
#[test]
#[ignore = "needs ffmpeg"]
fn ffmpeg_hears_sender_silence_as_gaps_of_the_right_length() {
    const FRAMES: usize = 300;
    let talking =
        |i: usize| !(60..85).contains(&i) && !(200..201).contains(&i) && !(240..290).contains(&i);
    let dir = scratch("ffmpeg-sender");
    let port = free_port_pair();
    let sdp_path = write_sdp(&dir, port);
    let output_path = dir.join("out.raw");

    let ffmpeg = spawn(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-protocol_whitelist",
            "file,udp,rtp",
            "-i",
            sdp_path.to_str().unwrap(),
            "-af",
            "aresample=async=1:min_hard_comp=0",
            "-f",
            "s16be",
            "-y",
            output_path.to_str().unwrap(),
        ],
    );
    wait_until_bound(port);

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut sender = Sender::new(
        0x5e4d,
        PAYLOAD_TYPE,
        FRAME_SAMPLES as u32,
        65_520,
        u32::MAX - 3_000,
    );
    let mut expected = Vec::new();
    for i in 0..FRAMES {
        let frame = test_signal(FRAME_SAMPLES, i * FRAME_SAMPLES);
        if talking(i) {
            let mut buf = [0u8; 700];
            let len = sender.send(&l16_bytes(&frame), &mut buf).unwrap();
            socket.send_to(&buf[..len], ("127.0.0.1", port)).unwrap();
            expected.extend(frame);
        } else {
            sender.skip();
            expected.extend(std::iter::repeat(0).take(FRAME_SAMPLES));
        }
        sleep(Duration::from_millis(1));
    }
    sleep(Duration::from_millis(500));
    ffmpeg.interrupt();

    // Trailing silence has no packet after it to reveal it.
    let last_talk = (0..FRAMES).rev().find(|&i| talking(i)).unwrap();
    expected.truncate((last_talk + 1) * FRAME_SAMPLES);
    let decoded = l16_samples(&fs::read(&output_path).unwrap());
    assert_eq!(
        decoded.len(),
        expected.len(),
        "total duration, silences included"
    );
    assert!(
        decoded == expected,
        "decoded audio differs from what was sent"
    );
}

/// ffmpeg's stream played through our `Receiver` comes out unchanged.
/// ffmpeg sends variable-length packets (up to 1460 bytes), which the
/// receiver must carry through as they are.
#[test]
#[ignore = "needs ffmpeg"]
fn ffmpeg_stream_plays_through_receiver_unchanged() {
    let input = test_signal(CLOCK_RATE as usize, 0);
    let (datagrams, _) = ffmpeg_capture("ffmpeg-receiver", &input);

    let config = Config {
        initial_depth: 2,
        min_depth: 1,
        max_depth: 16,
    };
    let mut receiver: Receiver<16, 1500> =
        Receiver::new(PAYLOAD_TYPE, FRAME_SAMPLES as u32, config);
    let mut played = Vec::new();
    let mut take = |receiver: &mut Receiver<16, 1500>| match receiver.play() {
        Playout::Frame { payload, .. } => {
            played.extend(l16_samples(payload));
            true
        }
        Playout::Lost { sequence } => panic!("{sequence} lost on loopback"),
        Playout::Silence => false,
    };
    for (i, datagram) in datagrams.iter().enumerate() {
        let packet = Packet::parse(datagram).unwrap();
        let arrival = (i * FRAME_SAMPLES) as u32;
        assert_eq!(
            receiver.receive(&packet, arrival),
            embedded_rtp::Received::Accepted
        );
        take(&mut receiver);
    }
    while take(&mut receiver) {}
    assert_eq!(played, input);
}

/// The `red` payload type for the RFC 2198 tests.
const RED_PAYLOAD_TYPE: u8 = 97;

/// GStreamer packetizes known audio and adds redundancy (RFC 2198) with
/// `rtpredenc`, carrying the frame `distance` packets back. Every tenth
/// packet is dropped; we must recover each lost frame exactly from a later
/// packet, so the audio plays through unchanged.
#[test]
#[ignore = "needs GStreamer (gst-launch-1.0 with the rtp plugin)"]
fn gstreamer_redundancy_recovers_every_lost_frame() {
    let input = test_signal(CLOCK_RATE as usize, 0);
    for distance in [1u16, 2] {
        let dir = scratch(&format!("gst-redenc-{distance}"));
        let input_path = dir.join("in.raw");
        fs::write(&input_path, l16_bytes(&input)).unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let gst = spawn(
            "gst-launch-1.0",
            &[
                "-q",
                "filesrc",
                &format!("location={}", input_path.to_str().unwrap()),
                "!",
                "rawaudioparse",
                "format=pcm",
                "pcm-format=s16be",
                "sample-rate=16000",
                "num-channels=1",
                "!",
                "rtpL16pay",
                "pt=96",
                "min-ptime=20000000",
                "max-ptime=20000000",
                "!",
                "rtpredenc",
                &format!("pt={RED_PAYLOAD_TYPE}"),
                &format!("distance={distance}"),
                "!",
                "udpsink",
                "host=127.0.0.1",
                &format!("port={port}"),
                "sync=false",
            ],
        );
        let frames = input.len() / FRAME_SAMPLES;
        let mut datagrams = Vec::new();
        let mut buf = [0u8; 2048];
        while datagrams.len() < frames {
            let (len, _) = socket
                .recv_from(&mut buf)
                .expect("GStreamer stopped sending early");
            datagrams.push(buf[..len].to_vec());
        }
        drop(gst);

        let config = Config {
            initial_depth: 2,
            min_depth: 1,
            max_depth: 16,
        };
        let mut receiver: Receiver<16, 1500> =
            Receiver::new(PAYLOAD_TYPE, FRAME_SAMPLES as u32, config)
                .with_redundancy(RED_PAYLOAD_TYPE, distance as u8);
        let mut played = Vec::new();
        let mut take = |receiver: &mut Receiver<16, 1500>| match receiver.play() {
            Playout::Frame { payload, .. } => {
                played.extend(l16_samples(payload));
                true
            }
            Playout::Lost { sequence } => panic!("distance {distance}: {sequence} not recovered"),
            Playout::Silence => false,
        };
        let mut dropped = 0;
        for (i, datagram) in datagrams.iter().enumerate() {
            let packet = Packet::parse(datagram).unwrap();
            assert_eq!(packet.header.payload_type, RED_PAYLOAD_TYPE);
            if i % 10 == 5 {
                dropped += 1;
            } else {
                let arrival = (i * FRAME_SAMPLES) as u32;
                assert_eq!(
                    receiver.receive(&packet, arrival),
                    embedded_rtp::Received::Accepted
                );
            }
            take(&mut receiver);
        }
        // The last packets' frames, and their redundant copies, are still
        // to come: the final frame can only be recovered if a packet after
        // it arrives, so none of the last `distance` is dropped.
        while take(&mut receiver) {}
        assert_eq!(receiver.counters().recovered, dropped);
        assert_eq!(played, input, "distance {distance}");
    }
}

/// Our redundant packets (RFC 2198) decode in GStreamer's `rtpreddec`.
/// Isolated packets are dropped; GStreamer must recover each lost frame
/// from the next packet's redundant block and play the audio unchanged.
#[test]
#[ignore = "needs GStreamer (gst-launch-1.0 with the rtp plugin)"]
fn gstreamer_recovers_from_our_redundancy() {
    use embedded_rtp::packet::HEADER_LEN;
    use embedded_rtp::red::Encoder;

    const FRAMES: usize = 100;
    const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
    let dropped = [10, 31, 52, 77];
    let dir = scratch("gst-reddec");
    let port = free_port_pair();
    let output_path = dir.join("out.raw");
    let caps = format!(
        "application/x-rtp,media=audio,clock-rate=16000,encoding-name=L16,channels=1,payload={PAYLOAD_TYPE}"
    );
    let gst = spawn(
        "gst-launch-1.0",
        &[
            "-q",
            "-e",
            "udpsrc",
            &format!("port={port}"),
            &format!("caps={caps}"),
            "!",
            "rtpreddec",
            &format!("pt={RED_PAYLOAD_TYPE}"),
            "!",
            "rtpjitterbuffer",
            "latency=200",
            "!",
            "rtpL16depay",
            "!",
            "filesink",
            &format!("location={}", output_path.to_str().unwrap()),
        ],
    );
    wait_until_bound(port);

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut sender = Sender::new(
        0x5eed_cafe,
        RED_PAYLOAD_TYPE,
        FRAME_SAMPLES as u32,
        65_500,
        u32::MAX - 20 * FRAME_SAMPLES as u32,
    );
    let mut encoder: Encoder<1, FRAME_BYTES> = Encoder::new(PAYLOAD_TYPE, FRAME_SAMPLES as u32);
    let frames: Vec<Vec<i16>> = (0..FRAMES)
        .map(|i| test_signal(FRAME_SAMPLES, i * FRAME_SAMPLES))
        .collect();
    let mut buf = [0u8; HEADER_LEN + Encoder::<1, FRAME_BYTES>::MAX_PAYLOAD];
    for (i, frame) in frames.iter().enumerate() {
        let len = encoder
            .encode(
                sender.timestamp(),
                &l16_bytes(frame),
                &mut buf[HEADER_LEN..],
            )
            .unwrap();
        let len = sender.send_in_place(&mut buf, len).unwrap();
        if !dropped.contains(&i) {
            socket.send_to(&buf[..len], ("127.0.0.1", port)).unwrap();
        }
        sleep(Duration::from_millis(5));
    }
    sleep(Duration::from_millis(500));
    gst.interrupt();

    let decoded = l16_samples(&fs::read(&output_path).unwrap());
    let expected: Vec<i16> = frames.concat();
    assert_eq!(decoded.len(), expected.len(), "frame count");
    assert!(
        decoded == expected,
        "decoded audio differs from what was sent"
    );
}

/// SRTP (RFC 3711) against ffmpeg's implementation, in both directions, with
/// the default suite and one master key for RTP and RTCP as SDES sets up.
/// Both streams start just below the sequence number wrap, so the rollover
/// counter, which the tag covers, has to agree too.
#[cfg(feature = "srtp")]
mod srtp_interop {
    use super::*;
    use embedded_rtp::srtp::{Inbound, MasterKey, Outbound, RTP_OVERHEAD};

    fn master_key() -> MasterKey {
        let mut bytes = [0u8; 30];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        MasterKey::from_bytes(&bytes)
    }

    /// Standard base64, as SDP's `inline:` and ffmpeg's `srtp_*_params` take.
    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// ffmpeg encrypts known audio; we must authenticate and decrypt every
    /// packet back to it exactly, and its SRTCP sender reports too.
    #[test]
    #[ignore = "needs ffmpeg"]
    fn ffmpeg_sends_srtp_we_decrypt() {
        let input = test_signal(2 * CLOCK_RATE as usize, 0); // two seconds
        let dir = scratch("ffmpeg-srtp-send");
        let input_path = dir.join("in.raw");
        fs::write(&input_path, l16_bytes(&input)).unwrap();

        let port = free_port_pair();
        let rtp = UdpSocket::bind(("127.0.0.1", port)).unwrap();
        let rtcp_socket = UdpSocket::bind(("127.0.0.1", port + 1)).unwrap();
        rtp.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        rtcp_socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let params = base64(&master_key().to_bytes());
        let url = format!("srtp://127.0.0.1:{port}");
        let ffmpeg = spawn(
            "ffmpeg",
            &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "s16be",
                "-ar",
                "16000",
                "-ac",
                "1",
                "-i",
                input_path.to_str().unwrap(),
                "-c:a",
                "pcm_s16be",
                "-payload_type",
                "96",
                "-seq",
                "65520",
                "-rtpflags",
                "send_bye",
                "-srtp_out_suite",
                "AES_CM_128_HMAC_SHA1_80",
                "-srtp_out_params",
                &params,
                "-f",
                "rtp",
                &url,
            ],
        );

        let mut inbound = Inbound::new(&master_key());
        let mut received = Vec::new();
        let mut sequences = Vec::new();
        let mut reports = Vec::new();
        let mut quiet_since = Instant::now();
        let mut buf = [0u8; 2048];
        while quiet_since.elapsed() < Duration::from_secs(2) {
            if let Ok((len, _)) = rtp.recv_from(&mut buf) {
                let plain = inbound
                    .unprotect_rtp(&mut buf[..len])
                    .unwrap_or_else(|e| panic!("ffmpeg's SRTP rejected: {e:?}"));
                let packet = Packet::parse(&buf[..plain]).unwrap();
                sequences.push(packet.header.sequence);
                received.extend(l16_samples(packet.payload));
                quiet_since = Instant::now();
            }
            if let Ok((len, _)) = rtcp_socket.recv_from(&mut buf) {
                let plain = inbound
                    .unprotect_rtcp(&mut buf[..len])
                    .unwrap_or_else(|e| panic!("ffmpeg's SRTCP rejected: {e:?}"));
                reports.push(buf[..plain].to_vec());
                quiet_since = Instant::now();
            }
        }
        drop(ffmpeg);

        assert!(
            sequences.contains(&65535) && sequences.contains(&0),
            "stream should cross the sequence wrap: {sequences:?}"
        );
        assert_eq!(received, input, "decrypted audio differs");

        let mut last_count = None;
        for report in &reports {
            for packet in rtcp::Compound::parse(report).unwrap().packets() {
                if let rtcp::Packet::SenderReport { info, .. } = packet {
                    last_count = Some(info.packet_count);
                }
            }
        }
        assert_eq!(
            last_count,
            Some(sequences.len() as u32),
            "final SR counts every packet"
        );
    }

    /// We encrypt; ffmpeg, keyed from the SDP's `a=crypto` line (RFC 4568),
    /// must decrypt back to the exact audio, across losses, reordering and
    /// the sequence wrap.
    #[test]
    #[ignore = "needs ffmpeg"]
    fn ffmpeg_decrypts_our_srtp() {
        const FRAMES: usize = 150;
        let dropped = [40, 41, 100];
        let dir = scratch("ffmpeg-srtp-receive");
        let port = free_port_pair();
        let sdp_path = dir.join("in.sdp");
        let output_path = dir.join("out.raw");
        fs::write(
            &sdp_path,
            format!(
                "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=embedded-rtp\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                 m=audio {port} RTP/SAVP 96\r\na=rtpmap:96 L16/16000/1\r\na=ptime:20\r\n\
                 a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{}\r\n",
                base64(&master_key().to_bytes())
            ),
        )
        .unwrap();

        let ffmpeg = spawn(
            "ffmpeg",
            &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-protocol_whitelist",
                "file,udp,rtp,srtp",
                "-i",
                sdp_path.to_str().unwrap(),
                "-f",
                "s16be",
                "-y",
                output_path.to_str().unwrap(),
            ],
        );
        wait_until_bound(port);

        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut sender = Sender::new(0x5eed_cafe, PAYLOAD_TYPE, FRAME_SAMPLES as u32, 65_500, 0);
        let mut outbound = Outbound::new(&master_key());
        let frames: Vec<Vec<i16>> = (0..FRAMES)
            .map(|i| test_signal(FRAME_SAMPLES, i * FRAME_SAMPLES))
            .collect();
        let mut wire: Vec<Vec<u8>> = frames
            .iter()
            .map(|frame| {
                let mut buf = vec![0u8; packet::HEADER_LEN + 2 * FRAME_SAMPLES + RTP_OVERHEAD];
                let len = sender.send(&l16_bytes(frame), &mut buf).unwrap();
                let len = outbound.protect_rtp(&mut buf, len).unwrap();
                buf.truncate(len);
                buf
            })
            .collect();
        wire.swap(70, 71);
        let order: Vec<usize> = (0..FRAMES).filter(|i| !dropped.contains(i)).collect();
        for &i in &order {
            socket.send_to(&wire[i], ("127.0.0.1", port)).unwrap();
            sleep(Duration::from_millis(1));
        }
        sleep(Duration::from_millis(500));
        ffmpeg.interrupt();

        // ffmpeg's jitter buffer puts the swapped pair back in order.
        let decoded = l16_samples(&fs::read(&output_path).unwrap());
        let expected: Vec<i16> = (0..FRAMES)
            .filter(|i| !dropped.contains(i))
            .flat_map(|i| frames[i].iter().copied())
            .collect();
        assert_eq!(decoded.len(), expected.len(), "frame count");
        assert!(
            decoded == expected,
            "decrypted audio differs from what was sent"
        );
    }
}

/// The example mixing server (`examples/common/intercom.rs`) with ffmpeg as
/// a participant, in insecure mode (ffmpeg can't do the example's join
/// handshake). The server runs in the test over real UDP sockets.
#[cfg(feature = "srtp")]
mod intercom_interop {
    use super::*;
    use crate::intercom::{Device, Frame, Mixer, Random, TICK_MS};
    use embedded_rtp::rtcp::Writer;

    fn seeded(seed: u64) -> Random {
        let mut rng = common::Rng::new(seed);
        Box::new(move |buf: &mut [u8]| rng.fill(buf))
    }

    fn seed(n: u64) -> [u8; 10] {
        let mut bytes = [0u8; 10];
        seeded(n)(&mut bytes);
        bytes
    }

    fn frames(samples: &[i16]) -> Vec<Frame> {
        samples
            .chunks_exact(FRAME_SAMPLES)
            .map(|chunk| chunk.try_into().unwrap())
            .collect()
    }

    /// The longest run of `heard` that is consecutive frames of `sent`.
    fn longest_exact_run(heard: &[Frame], sent: &[Frame]) -> usize {
        let mut best = 0;
        for start in 0..heard.len() {
            let Some(j) = sent.iter().position(|f| *f == heard[start]) else {
                continue;
            };
            let run = heard[start..]
                .iter()
                .zip(&sent[j..])
                .take_while(|(h, s)| h == s)
                .count();
            best = best.max(run);
        }
        best
    }

    /// Take every datagram waiting on a non-blocking socket.
    fn drain(socket: &UdpSocket, mut f: impl FnMut(SocketAddr, &mut [u8])) {
        let mut buf = [0u8; 2048];
        while let Ok((len, from)) = socket.recv_from(&mut buf) {
            f(from, &mut buf[..len]);
        }
    }

    use std::net::SocketAddr;

    fn bind() -> UdpSocket {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        socket
    }

    /// ffmpeg streams known audio into the room in real time; a listening
    /// device hears it exactly, and every frame it doesn't hear is one the
    /// server's jitter buffer chose to drop.
    ///
    /// Even with `-re`, ffmpeg sends its first half second or so in a
    /// burst. The server's jitter buffer drops what exceeds its maximum
    /// depth (`overflow`), and later drops a frame or two to bring latency
    /// back to target (`drained`), at a moment that varies from run to run.
    /// So the check is an exact account: everything heard is ffmpeg's audio,
    /// in order, with no concealment, and the frames missing are exactly
    /// those the server's counters say it dropped.
    #[test]
    #[ignore = "needs ffmpeg"]
    fn ffmpeg_joins_the_room() {
        let input = test_signal(3 * CLOCK_RATE as usize, 0);
        let dir = scratch("intercom-ffmpeg-joins");
        let input_path = dir.join("in.raw");
        fs::write(&input_path, l16_bytes(&input)).unwrap();

        let server = bind();
        let server_addr = server.local_addr().unwrap();
        let listener = bind();
        let mut mixer = Mixer::new(Default::default(), true, seeded(1));
        let mut device = Device::new("listener", "Listener", None, seed(2));
        let url = format!("rtp://{server_addr}");
        let ffmpeg = spawn(
            "ffmpeg",
            &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-re",
                "-f",
                "s16be",
                "-ar",
                "16000",
                "-ac",
                "1",
                "-i",
                input_path.to_str().unwrap(),
                "-af",
                "asetnsamples=n=320", // one 20 ms frame per packet
                "-c:a",
                "pcm_s16be",
                "-payload_type",
                "96",
                "-packetsize",
                "652",
                "-f",
                "rtp",
                &url,
            ],
        );

        let start = Instant::now();
        let now_ms = || start.elapsed().as_millis() as u64;
        let mut next_tick = start;
        let mut heard = Vec::new();
        while start.elapsed() < Duration::from_millis(4_500) {
            drain(&server, |from, data| {
                mixer.datagram(from, data, now_ms(), &mut |to, d| {
                    drop(server.send_to(d, to))
                });
            });
            drain(&listener, |_, data| device.datagram(data, now_ms()));
            if Instant::now() >= next_tick {
                let frame = device.tick(now_ms(), None, &mut |d| {
                    drop(listener.send_to(d, server_addr));
                });
                heard.extend(frame);
                mixer.tick(now_ms(), &mut |to, d| drop(server.send_to(d, to)));
                next_tick += Duration::from_millis(TICK_MS);
            }
            sleep(Duration::from_millis(1));
        }
        drop(ffmpeg);

        let sent = frames(&input);
        let ffmpeg_member = mixer
            .members()
            .into_iter()
            .find(|m| m.name != "Listener")
            .expect("ffmpeg never joined");
        let counters = ffmpeg_member.counters;

        // Which of ffmpeg's frames each heard frame is. The first and last
        // are concealment fading in and out, so not exact.
        let indexes: Vec<Option<usize>> = heard
            .iter()
            .map(|h| sent.iter().position(|s| s == h))
            .collect();
        let first = indexes
            .iter()
            .position(Option::is_some)
            .expect("heard none of it");
        let last = indexes.iter().rposition(Option::is_some).unwrap();
        let middle: Vec<usize> = indexes[first..=last]
            .iter()
            .map(|i| i.expect("a concealed or foreign frame mid-stream"))
            .collect();
        assert!(
            middle.windows(2).all(|w| w[0] < w[1]),
            "out of order: {middle:?}"
        );
        let gaps: usize = middle.windows(2).map(|w| w[1] - w[0] - 1).sum();
        // Missing: those before the first exact frame (one of which played,
        // faded in) and the gaps after it. Overflow can strike either side
        // of the first frame played, so only the total is fixed.
        assert_eq!(*middle.last().unwrap(), sent.len() - 1, "the end was heard");
        assert_eq!(
            middle[0] + gaps,
            (counters.overflow + counters.drained) as usize + 1,
            "every missing frame is one the server dropped; server: {counters:?}"
        );
        assert_eq!(counters.concealed, 0);
    }

    /// A device talks into the room; the server's mix, as sent to a
    /// participant, decodes in ffmpeg to exactly what was said.
    #[test]
    #[ignore = "needs ffmpeg"]
    fn ffmpeg_decodes_the_mix() {
        const FRAMES: usize = 150;
        let dir = scratch("intercom-ffmpeg-decodes");
        let port = free_port_pair();
        let sdp_path = write_sdp(&dir, port);
        let output_path = dir.join("out.raw");
        let ffmpeg = spawn(
            "ffmpeg",
            &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-protocol_whitelist",
                "file,udp,rtp",
                "-i",
                sdp_path.to_str().unwrap(),
                "-f",
                "s16be",
                "-y",
                output_path.to_str().unwrap(),
            ],
        );
        wait_until_bound(port);

        let server = bind();
        let server_addr = server.local_addr().unwrap();
        let talker_socket = bind();
        // The participant whose mix goes to ffmpeg: it joins with a receiver
        // report and passes on every RTP packet it is sent.
        let relay = bind();
        let mut buf = [0u8; 256];
        let mut writer = Writer::new(&mut buf);
        writer.receiver_report(0xf00d, &[]).unwrap();
        writer
            .source_description(0xf00d, &[(rtcp::sdes::CNAME, b"ffmpeg")])
            .unwrap();
        relay.send_to(writer.finish(), server_addr).unwrap();

        let mut mixer = Mixer::new(Default::default(), true, seeded(3));
        let mut talker = Device::new("talker", "Talker", None, seed(4));
        let said: Vec<Frame> = (0..FRAMES)
            .map(|i| {
                test_signal(FRAME_SAMPLES, i * FRAME_SAMPLES)
                    .try_into()
                    .unwrap()
            })
            .collect();

        let mut now_ms = 0u64;
        for tick in 0..FRAMES + 30 {
            let capture = said.get(tick);
            talker.tick(now_ms, capture, &mut |d| {
                drop(talker_socket.send_to(d, server_addr))
            });
            sleep(Duration::from_millis(2)); // let the loopback deliver
            drain(&server, |from, data| {
                mixer.datagram(from, data, now_ms, &mut |to, d| drop(server.send_to(d, to)));
            });
            mixer.tick(now_ms, &mut |to, d| drop(server.send_to(d, to)));
            sleep(Duration::from_millis(2));
            drain(&relay, |_, data| {
                if !packet::is_rtcp(data) {
                    relay.send_to(data, ("127.0.0.1", port)).unwrap();
                }
            });
            drain(&talker_socket, |_, data| talker.datagram(data, now_ms));
            now_ms += TICK_MS;
        }
        sleep(Duration::from_millis(500));
        ffmpeg.interrupt();

        let decoded = frames(&l16_samples(&fs::read(&output_path).unwrap()));
        let run = longest_exact_run(&decoded, &said);
        assert!(
            run >= FRAMES - 5,
            "ffmpeg decoded {} frames, only {run} of {FRAMES} exact in a row",
            decoded.len()
        );
    }
}
