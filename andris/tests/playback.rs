//! The playback channel, against an in-process andris on the synthetic
//! platform: `MODE` first, Opus with the cap and raw PCM without, `START`
//! on sound and `STOP` after a second of silence, multimedia times, drops
//! under a stalled reader, `--no-audio`, capture failing, what the client
//! sends, and capture stopping when the channel closes.
//!
//! Messages are read with ryll's playback types, and Opus packets decoded
//! with `opus-decoder`, ryll's decoder, into a buffer of exactly one 10 ms
//! frame. Sound comes from the synthetic [`ToneSource`], whose frames are
//! stamped with the ticks they were made at, so the times a test checks
//! are exact however loaded the machine; or from a [`ManualSource`] the
//! test feeds frame by frame. Waits are deadline-based, with margins of
//! about twice what is expected.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use andris::platform::synthetic::{TonePattern, ToneSource};
use andris::platform::{
    AUDIO_FRAME_BYTES, AUDIO_FRAME_SAMPLES, AUDIO_FRAME_VALUES, AudioFrame, AudioReceiver,
    AudioSender, AudioSource, audio_queue,
};
use andris::server::session::Session;
use shakenfist_spice_protocol::messages::{
    AckSync, ChannelEntry, ChannelsList, Disconnecting, PlaybackMode, PlaybackPacket,
    PlaybackStart, Pong, ReceivedMessage, SetAck, WireType,
};
use shakenfist_spice_protocol::{
    ChannelType, SpiceError, audio_data_mode, audio_fmt, capabilities, display_server, main_client,
    main_server, playback_server,
};

use support::{TestClient, TestServer, WAIT, wait_until};

/// How long to wait before concluding the server sends nothing.
const QUIET: Duration = Duration::from_millis(300);

/// Ryll's playback caps: CELT and Opus.
const OPUS_CAPS: u32 = capabilities::PLAYBACK_CELT_0_5_1 | capabilities::PLAYBACK_OPUS;

/// A client without Opus: raw PCM.
const RAW_CAPS: u32 = 0;

/// The loudness of a [`ManualSource`] test's sound.
const LOUD: i16 = 1000;

/// One message the playback channel sent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sent {
    Mode(PlaybackMode),
    Start(PlaybackStart),
    Data(PlaybackPacket),
    Stop,
}

impl Sent {
    fn decode(message: &ReceivedMessage) -> Self {
        let payload = message.payload();
        match message.header.message_type {
            playback_server::MODE => Sent::Mode(PlaybackMode::decode(payload).unwrap()),
            playback_server::START => Sent::Start(PlaybackStart::decode(payload).unwrap()),
            playback_server::DATA => Sent::Data(PlaybackPacket::decode(payload).unwrap()),
            playback_server::STOP => {
                assert!(payload.is_empty(), "STOP has no body");
                Sent::Stop
            }
            other => panic!("the playback channel sent opcode {other}"),
        }
    }

    fn data(self) -> PlaybackPacket {
        match self {
            Sent::Data(packet) => packet,
            other => panic!("expected DATA, got {other:?}"),
        }
    }
}

/// The next message on a playback channel.
async fn next(client: &mut TestClient) -> Sent {
    Sent::decode(&client.recv().await)
}

/// Check that nothing arrives for [`QUIET`], and that the channel is
/// still open: a closed channel fails the read.
async fn expect_quiet(client: &mut TestClient) {
    if let Some(message) = client.recv_within(QUIET).await {
        panic!("expected nothing, got {:?}", Sent::decode(&message));
    }
}

/// The main channel that holds the session open, its session, and a
/// playback channel linked with `caps`.
async fn open_playback(server: &TestServer, caps: u32) -> (TestClient, Arc<Session>, TestClient) {
    let (main, id) = server.open_main().await;
    let session = server.handle.session().unwrap();
    assert_eq!(session.id(), id);
    let playback = server.open_with_caps(id, ChannelType::Playback, caps).await;
    (main, session, playback)
}

/// Read `MODE`, which must come first, and check it names `mode`.
async fn expect_mode(client: &mut TestClient, mode: u16) {
    match next(client).await {
        Sent::Mode(announced) => assert_eq!(announced.mode, mode),
        other => panic!("expected MODE first, got {other:?}"),
    }
}

/// Read `START` and check its shape; return its time.
async fn expect_start(client: &mut TestClient) -> u32 {
    match next(client).await {
        Sent::Start(start) => {
            assert_eq!(
                (start.channels, start.format, start.frequency),
                (2, audio_fmt::S16, 48_000)
            );
            start.time
        }
        other => panic!("expected START, got {other:?}"),
    }
}

/// The largest magnitude of the S16LE samples in `bytes`.
fn peak(bytes: &[u8]) -> u16 {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair).unsigned_abs())
        .max()
        .unwrap_or(0)
}

/// Assert that the `START` and `DATA` times in `sent` never go backwards.
fn assert_non_decreasing(sent: &[Sent]) {
    let mut last = 0;
    for message in sent {
        let time = match message {
            Sent::Start(start) => start.time,
            Sent::Data(packet) => packet.time,
            Sent::Mode(_) | Sent::Stop => continue,
        };
        assert!(time >= last, "a time went back from {last} to {time}");
        last = time;
    }
}

/// A tone that sounds for `on_ms` and is silent for `off_ms`.
fn tone(on_ms: u64, off_ms: u64) -> Arc<ToneSource> {
    Arc::new(
        ToneSource::new(TonePattern {
            on: Duration::from_millis(on_ms),
            off: Duration::from_millis(off_ms),
        })
        .unwrap(),
    )
}

/// A tone that never falls silent.
fn endless_tone() -> Arc<ToneSource> {
    tone(24 * 3600 * 1000, 0)
}

/// An [`AudioSource`] the test feeds: each open's sender is kept for the
/// test to take with [`ManualSource::take`].
#[derive(Default)]
struct ManualSource {
    senders: Mutex<Vec<AudioSender>>,
    opens: Mutex<usize>,
}

impl AudioSource for ManualSource {
    fn open(&self) -> anyhow::Result<AudioReceiver> {
        let (sender, receiver) = audio_queue();
        self.senders.lock().unwrap().push(sender);
        *self.opens.lock().unwrap() += 1;
        Ok(receiver)
    }
}

impl ManualSource {
    /// The sender of the next open, once the channel has opened capture.
    async fn take(&self) -> AudioSender {
        wait_until("the playback channel opens capture", || {
            !self.senders.lock().unwrap().is_empty()
        })
        .await;
        self.senders.lock().unwrap().remove(0)
    }

    fn opens(&self) -> usize {
        *self.opens.lock().unwrap()
    }
}

/// An [`AudioSource`] that cannot even start.
struct FailingSource;

impl AudioSource for FailingSource {
    fn open(&self) -> anyhow::Result<AudioReceiver> {
        anyhow::bail!("there is no sound server")
    }
}

/// A frame of `value` in every sample, captured `ms` after `session`
/// began.
fn frame_at(session: &Session, ms: u64, value: i16) -> AudioFrame {
    AudioFrame::new(
        session.started() + Duration::from_millis(ms),
        vec![value; AUDIO_FRAME_VALUES],
    )
    .unwrap()
}

#[tokio::test]
async fn mode_comes_first_and_start_precedes_data() {
    let server = TestServer::start().await;
    let (_main, id) = server.open_main().await;

    // The reply offers what kerbside offers: VOLUME and OPUS.
    let mut probe = TestClient::connect(server.addr()).await;
    let reply = probe.link(id, ChannelType::Playback).await;
    assert_eq!(reply.error, SpiceError::Ok);
    assert_eq!(
        reply.channel_caps,
        vec![capabilities::PLAYBACK_VOLUME | capabilities::PLAYBACK_OPUS]
    );
    drop(probe);

    let mut playback = server
        .open_with_caps(id, ChannelType::Playback, OPUS_CAPS)
        .await;
    expect_mode(&mut playback, audio_data_mode::OPUS).await;
    let start = expect_start(&mut playback).await;
    let first = next(&mut playback).await.data();
    assert_eq!(first.time, start, "the frame that started the stream");
    for _ in 0..10 {
        assert!(next(&mut playback).await.data().time > start);
    }
}

#[tokio::test]
async fn opus_with_the_cap_and_every_packet_decodes_into_ten_ms() {
    // A START, a STOP and a START again, so the packets cover the
    // encoder's reset.
    let server = TestServer::start_with_audio(Some(tone(300, 1200))).await;
    let (_main, session, mut playback) = open_playback(&server, OPUS_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::OPUS).await;

    let mut sent = vec![];
    let mut starts = 0;
    let mut decoder = None;
    let mut packets = 0;
    let mut loud = 0;
    tokio::time::timeout(WAIT, async {
        while starts < 2 || packets < 40 {
            let message = next(&mut playback).await;
            match &message {
                Sent::Start(_) => {
                    // As ryll does: a fresh decoder for each stream.
                    starts += 1;
                    decoder = Some(opus_decoder::OpusDecoder::new(48_000, 2).unwrap());
                }
                Sent::Data(packet) => {
                    assert!(packet.data.len() <= 480, "{} bytes", packet.data.len());
                    let decoder = decoder.as_mut().expect("DATA before START");
                    let mut pcm = [0i16; AUDIO_FRAME_VALUES];
                    let samples = decoder.decode(&packet.data, &mut pcm, false).unwrap();
                    assert_eq!(samples, AUDIO_FRAME_SAMPLES, "a 10 ms frame");
                    packets += 1;
                    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                    if peak(&bytes) > 4000 {
                        loud += 1;
                    }
                }
                Sent::Stop => {}
                Sent::Mode(_) => panic!("a second MODE"),
            }
            sent.push(message);
        }
    })
    .await
    .expect("two STARTs and 40 packets did not arrive");
    assert!(sent.contains(&Sent::Stop));
    assert_non_decreasing(&sent);
    // The tone (a quarter of full scale) survives the round trip.
    assert!(loud >= 20, "only {loud} loud frames decoded");

    let stats = session.stats();
    assert!(stats.audio_encode.count >= packets);
    assert!(stats.audio_frames >= packets);
    assert!(stats.playback_bytes > 0);
    assert_eq!((stats.audio_starts, stats.audio_stops), (2, 1));
}

#[tokio::test]
async fn raw_without_the_opus_cap_sends_the_frames_bytes() {
    let source = tone(10_000, 0);
    let server = TestServer::start_with_audio(Some(source.clone())).await;
    let (_main, session, mut playback) = open_playback(&server, RAW_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::RAW).await;
    let start = expect_start(&mut playback).await;
    for _ in 0..20 {
        let packet = next(&mut playback).await.data();
        assert_eq!(packet.data.len(), AUDIO_FRAME_BYTES);
        // The tone's frames are 10 ms apart from its open, so the time
        // says which frame this is, even if one was dropped.
        let index = u64::from(packet.time - start) / 10;
        let expected = source.frame(index, Instant::now()).to_le_bytes();
        assert_eq!(packet.data, expected, "frame {index}");
    }
    assert_eq!(session.stats().audio_encode.count, 0, "no Opus in raw mode");
}

#[tokio::test]
async fn a_second_of_silence_stops_the_stream_and_sound_starts_it_again() {
    // 300 ms of tone, then 1.6 s of silence: long enough to stop.
    let server = TestServer::start_with_audio(Some(tone(300, 1600))).await;
    let (_main, session, mut playback) = open_playback(&server, RAW_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::RAW).await;

    let mut sent = vec![];
    let mut last_loud: Option<(u32, Instant)> = None;
    let mut last_data = 0;
    let mut stop: Option<(u32, Instant)> = None;
    let mut starts = vec![];
    tokio::time::timeout(WAIT, async {
        while starts.len() < 2 {
            let message = next(&mut playback).await;
            match &message {
                Sent::Start(start) => starts.push(start.time),
                Sent::Data(packet) => {
                    if peak(&packet.data) > 8 && stop.is_none() {
                        last_loud = Some((packet.time, Instant::now()));
                    }
                    last_data = packet.time;
                }
                Sent::Stop => {
                    assert!(stop.is_none(), "a second STOP before the second START");
                    stop = Some((last_data, Instant::now()));
                }
                Sent::Mode(_) => panic!("a second MODE"),
            }
            sent.push(message);
        }
    })
    .await
    .expect("the stream did not stop and start again");

    let (loud_time, loud_at) = last_loud.unwrap();
    let (stop_time, stop_at) = stop.unwrap();
    // The tone sounded for 30 frames from the first START.
    assert_eq!(loud_time, starts[0] + 290);
    // STOP follows the 99 silent frames after the tone: a second of
    // silence in capture time, well within 1.5 s.
    let silence = stop_time - loud_time;
    assert!((980..=1500).contains(&silence), "STOP after {silence} ms");
    // And in wall time, with a margin for a loaded machine.
    let waited = stop_at - loud_at;
    assert!(waited <= Duration::from_secs(3), "STOP after {waited:?}");
    // The tone returns 1.9 s after it began.
    assert_eq!(starts[1], starts[0] + 1900);
    assert_non_decreasing(&sent);
    // Nothing was sent between the STOP and the second START.
    let after_stop = sent.iter().skip_while(|m| **m != Sent::Stop).nth(1);
    assert!(matches!(after_stop, Some(Sent::Start(_))), "{after_stop:?}");

    let stats = session.stats();
    assert_eq!((stats.audio_starts, stats.audio_stops), (2, 1));
}

#[tokio::test]
async fn exactly_a_hundred_silent_frames_stop_the_stream() {
    let source = Arc::new(ManualSource::default());
    let server = TestServer::start_with_audio(Some(source.clone())).await;
    let (_main, session, mut playback) = open_playback(&server, RAW_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::RAW).await;
    let sender = source.take().await;

    // Silence while stopped sends nothing.
    for n in 0..5 {
        assert!(sender.send(frame_at(&session, 10 * n, 0)));
    }
    expect_quiet(&mut playback).await;

    // Sound starts the stream, with the sounding frame.
    assert!(sender.send(frame_at(&session, 50, LOUD)));
    assert_eq!(expect_start(&mut playback).await, 50);
    assert_eq!(next(&mut playback).await.data().time, 50);
    // 99 silent frames are sent; the hundredth is a STOP.
    for n in 1..100 {
        let ms = 50 + 10 * n;
        assert!(sender.send(frame_at(&session, ms, 0)));
        let packet = next(&mut playback).await.data();
        assert_eq!(packet.time, ms as u32);
        assert_eq!(peak(&packet.data), 0);
    }
    assert!(sender.send(frame_at(&session, 1050, 0)));
    assert_eq!(next(&mut playback).await, Sent::Stop);
    assert!(sender.send(frame_at(&session, 1060, 0)));
    expect_quiet(&mut playback).await;

    // A sample just over the threshold is sound.
    assert!(sender.send(frame_at(&session, 1070, 9)));
    assert_eq!(expect_start(&mut playback).await, 1070);
    assert_eq!(next(&mut playback).await.data().time, 1070);
    let stats = session.stats();
    assert_eq!((stats.audio_starts, stats.audio_stops), (2, 1));
    assert_eq!(stats.audio_frames, 101);
}

#[tokio::test]
async fn times_follow_capture_and_never_go_backwards() {
    let source = Arc::new(ManualSource::default());
    let server = TestServer::start_with_audio(Some(source.clone())).await;
    let (_main, session, mut playback) = open_playback(&server, OPUS_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::OPUS).await;
    let sender = source.take().await;

    // A stamp from before the session began, as capture latency can
    // make one, is time 0.
    let early = session.started() - Duration::from_millis(5);
    let frame = AudioFrame::new(early, vec![LOUD; AUDIO_FRAME_VALUES]).unwrap();
    assert!(sender.send(frame));
    let mut sent = vec![next(&mut playback).await, next(&mut playback).await];
    assert_eq!(sent[0], Sent::Start(start_at(0)));
    assert_eq!(sent[1].clone().data().time, 0);
    // Frames may share a stamp while a sink wakes; the times are then
    // equal, and never fall.
    for ms in [0, 7, 7, 7, 20, 30, 30, 41] {
        assert!(sender.send(frame_at(&session, ms, LOUD)));
        let message = next(&mut playback).await;
        assert_eq!(message.clone().data().time, ms as u32);
        sent.push(message);
    }
    assert_non_decreasing(&sent);
}

/// `message` as a body to send.
fn body<T: WireType>(message: &T) -> Vec<u8> {
    let mut out = Vec::new();
    message.write(&mut out);
    out
}

/// The `START` andris sends for a stream whose first frame is at `time`.
fn start_at(time: u32) -> PlaybackStart {
    PlaybackStart {
        channels: 2,
        format: audio_fmt::S16,
        frequency: 48_000,
        time,
    }
}

#[tokio::test]
async fn a_stalled_reader_loses_frames_rather_than_queueing_them() {
    let server = TestServer::start_with_audio(Some(endless_tone())).await;
    let (_main, id) = server.open_main().await;
    let session = server.handle.session().unwrap();
    // Raw frames, and a small receive buffer, so the stall reaches andris
    // within a few frames.
    let mut playback = server
        .open_with_recv_buffer(id, ChannelType::Playback, RAW_CAPS, 4096)
        .await;
    expect_mode(&mut playback, audio_data_mode::RAW).await;
    let mut last = expect_start(&mut playback).await;
    for _ in 0..5 {
        last = next(&mut playback).await.data().time;
    }
    assert_eq!(session.stats().audio_drops, 0);

    // Stop reading for two seconds: 200 frames of sound.
    let stall = Duration::from_secs(2);
    let stalled_at = session.elapsed();
    tokio::time::sleep(stall).await;
    let resumed = session.elapsed().as_millis() as u32;

    // What arrives first is what was waiting: a few frames from the
    // stall's start, then a jump to the frames captured once there was
    // room again. A queue would deliver all 200 in order.
    let mut backlog = 0;
    let mut largest_gap = 0;
    tokio::time::timeout(WAIT, async {
        loop {
            let packet = next(&mut playback).await.data();
            largest_gap = largest_gap.max(packet.time - last);
            last = packet.time;
            if packet.time >= resumed {
                break;
            }
            backlog += 1;
        }
    })
    .await
    .expect("the channel never caught up");
    let stalled_ms = stall.as_millis() as u32;
    assert!(
        backlog < 60,
        "{backlog} frames of the stall's {} were kept",
        stalled_ms / 10
    );
    assert!(
        largest_gap > 100,
        "no frames were skipped: gap {largest_gap} ms"
    );
    wait_until("the drops are counted", || session.stats().audio_drops > 0).await;

    // It recovers: frames follow one another again, 10 ms apart.
    let mut previous = next(&mut playback).await.data().time;
    for _ in 0..20 {
        let packet = next(&mut playback).await.data();
        assert_eq!(packet.data.len(), AUDIO_FRAME_BYTES);
        assert!(
            packet.time - previous <= 20,
            "{previous} to {}",
            packet.time
        );
        previous = packet.time;
    }
    let stats = session.stats();
    let drops = stats.audio_drops;
    assert!(
        drops >= u64::from(stalled_ms / 10) / 2,
        "{drops} drops in a {stall:?} stall from {stalled_at:?}"
    );
}

#[tokio::test]
async fn without_audio_playback_is_not_listed_and_its_link_is_refused() {
    let server = TestServer::start_with_audio(None).await;
    let (mut main, id) = server.open_main().await;
    // INIT and the first PING.
    main.recv().await;
    main.recv().await;
    main.send(main_client::ATTACH_CHANNELS, &[]).await;
    let message = main.recv().await;
    assert_eq!(message.header.message_type, main_server::CHANNELS_LIST);
    let list = ChannelsList::decode(message.payload()).unwrap();
    let entry = |channel_type: ChannelType| ChannelEntry {
        channel_type: channel_type as u8,
        channel_id: 0,
    };
    assert_eq!(
        list.channels,
        vec![
            entry(ChannelType::Display),
            entry(ChannelType::Cursor),
            entry(ChannelType::Inputs),
        ]
    );

    let mut client = TestClient::connect(server.addr()).await;
    let reply = client.link(id, ChannelType::Playback).await;
    assert_eq!(reply.error, SpiceError::ChannelUnavailable);
    client.expect_closed().await;
    assert_eq!(server.handle.session_id(), Some(id), "the session goes on");
}

/// Check the session's other channels still work: the main channel
/// answers, and a display channel links and starts.
async fn expect_session_up(server: &TestServer, main: &mut TestClient, id: u32) {
    main.send(main_client::ATTACH_CHANNELS, &[]).await;
    loop {
        let message = main.recv().await;
        if message.header.message_type == main_server::CHANNELS_LIST {
            break;
        }
    }
    let mut display = server.open(id, ChannelType::Display).await;
    let message = display.recv().await;
    assert_eq!(message.header.message_type, display_server::SET_ACK);
    SetAck::decode(message.payload()).unwrap();
    assert_eq!(server.handle.session_id(), Some(id));
}

#[tokio::test]
async fn a_capture_that_cannot_start_leaves_the_channel_silent_and_the_session_up() {
    let server = TestServer::start_with_audio(Some(Arc::new(FailingSource))).await;
    let (mut main, session, mut playback) = open_playback(&server, OPUS_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::OPUS).await;
    expect_quiet(&mut playback).await;
    expect_session_up(&server, &mut main, session.id()).await;
    // The playback channel is still linked, and still silent.
    expect_quiet(&mut playback).await;
    assert_eq!(session.stats().audio_frames, 0);
}

#[tokio::test]
async fn a_capture_that_ends_stops_the_stream_and_relinking_tries_again() {
    let source = Arc::new(ManualSource::default());
    let server = TestServer::start_with_audio(Some(source.clone())).await;
    let (mut main, session, mut playback) = open_playback(&server, RAW_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::RAW).await;
    let sender = source.take().await;
    assert!(sender.send(frame_at(&session, 10, LOUD)));
    expect_start(&mut playback).await;
    next(&mut playback).await.data();

    // The sound server goes: the stream stops, and the channel stays.
    drop(sender);
    assert_eq!(next(&mut playback).await, Sent::Stop);
    expect_quiet(&mut playback).await;
    expect_session_up(&server, &mut main, session.id()).await;
    expect_quiet(&mut playback).await;
    assert_eq!(source.opens(), 1);

    // Linking the channel again opens capture again.
    let mut again = server
        .open_with_caps(session.id(), ChannelType::Playback, RAW_CAPS)
        .await;
    expect_mode(&mut again, audio_data_mode::RAW).await;
    let sender = source.take().await;
    assert_eq!(source.opens(), 2);
    assert!(sender.send(frame_at(&session, 2000, LOUD)));
    assert_eq!(expect_start(&mut again).await, 2000);
}

#[tokio::test]
async fn closing_the_channel_stops_capture() {
    let source = Arc::new(ManualSource::default());
    let server = TestServer::start_with_audio(Some(source.clone())).await;
    let (_main, _session, mut playback) = open_playback(&server, OPUS_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::OPUS).await;
    let sender = source.take().await;
    assert!(!sender.is_closed());
    drop(playback);
    wait_until("capture is told to stop", || sender.is_closed()).await;
}

#[tokio::test]
async fn client_messages_mid_stream_leave_data_flowing_and_disconnecting_closes_the_channel() {
    let source = Arc::new(ManualSource::default());
    let server = TestServer::start_with_audio(Some(source.clone())).await;
    let (mut main, session, mut playback) = open_playback(&server, RAW_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::RAW).await;
    let sender = source.take().await;
    assert!(sender.send(frame_at(&session, 10, LOUD)));
    assert_eq!(expect_start(&mut playback).await, 10);
    assert_eq!(next(&mut playback).await.data().time, 10);

    // What a client may send on any channel, and an opcode playback does
    // not know. The PONG goes last, and is timed once it has been read,
    // so the others have been read by then too.
    playback
        .send(main_client::ACK_SYNC, &body(&AckSync { generation: 1 }))
        .await;
    playback.send(main_client::ACK, &[]).await;
    playback.send(60000, &[1, 2, 3]).await;
    let pong = Pong {
        id: 1,
        timestamp: session.clock_ns(),
    };
    playback.send(main_client::PONG, &body(&pong)).await;
    wait_until("the PONG is timed", || session.stats().ping.count == 1).await;

    // DATA goes on, in order.
    for n in 2..12 {
        assert!(sender.send(frame_at(&session, 10 * n, LOUD)));
        assert_eq!(next(&mut playback).await.data().time, 10 * n as u32);
    }

    // DISCONNECTING closes the channel, stopping capture, and only the
    // channel.
    let disconnecting = Disconnecting {
        timestamp: 0,
        reason: 0,
    };
    playback
        .send(main_client::DISCONNECTING, &body(&disconnecting))
        .await;
    playback.expect_closed().await;
    wait_until("capture is told to stop", || sender.is_closed()).await;
    expect_session_up(&server, &mut main, session.id()).await;
}

#[tokio::test]
async fn dropping_the_client_mid_stream_stops_capture() {
    let source = Arc::new(ManualSource::default());
    let server = TestServer::start_with_audio(Some(source.clone())).await;
    let (_main, session, mut playback) = open_playback(&server, RAW_CAPS).await;
    expect_mode(&mut playback, audio_data_mode::RAW).await;
    let sender = source.take().await;
    assert!(sender.send(frame_at(&session, 10, LOUD)));
    expect_start(&mut playback).await;
    for n in 1..5 {
        assert_eq!(next(&mut playback).await.data().time, 10 * n);
        assert!(sender.send(frame_at(&session, 10 * u64::from(n + 1), LOUD)));
    }

    // Sound keeps arriving after the client goes, so the channel may be
    // writing a DATA as it notices.
    drop(playback);
    let mut ms = 50;
    wait_until("capture is told to stop", || {
        ms += 10;
        !sender.send(frame_at(&session, ms, LOUD))
    })
    .await;
    assert!(sender.is_closed());
}
