//! The playback channel: the desktop's sound, to the client.
//!
//! # What is sent
//!
//! `MODE` comes first, saying how `DATA` is encoded: Opus when the client
//! advertised `PLAYBACK_OPUS`, otherwise raw S16LE ([`Mode::for_caps`]), as
//! spice-server chooses. The channel then opens the platform's
//! [`AudioSource`] and sends what it captures, a 10 ms frame at a time:
//!
//! - **`START`** (two channels, S16, 48 kHz) when a frame that is not
//!   silent arrives while the stream is stopped, followed by that frame's
//!   `DATA`, so the sound that broke the silence is not lost;
//! - **`DATA`** for every frame while the stream is started, silent ones
//!   included, so a quiet passage plays as it was captured;
//! - **`STOP`** once [`SILENCE_HOLD_FRAMES`] frames in a row have been
//!   silent, after which nothing is sent until sound returns.
//!
//! spice-server leaves starting and stopping to its producer: QEMU knows
//! when a guest opens and closes its voice. Andris sees only samples, so
//! digital silence is the only signal it has ([`SilenceGate`]). Stopping
//! means an idle desktop costs the client nothing, as the display
//! channel's damage-driven design already promises, and it gives
//! spice-gtk a real `STOP`, which returns its clock to the main channel's.
//!
//! Frames are always 480 samples a channel (10 ms), in both modes:
//! spice-gtk decodes Opus into a buffer of exactly that size. A raw frame
//! is 1920 bytes; an Opus packet at most [`MAX_OPUS_PACKET`] bytes.
//!
//! # Opus
//!
//! The encoder is libopus, stereo, `Application::Audio` (spice-server's
//! choice), at an explicit [`OPUS_BITRATE`], close to libopus's own
//! default for 10 ms stereo frames, named so the metrics can be read
//! against it. A frame encodes in about 0.05 ms, so it is encoded inline
//! rather than on the blocking pool. The encoder is reset at each
//! `START`, because the client makes its decoder afresh there (ryll and
//! spice-gtk both do), and the two must begin from the same state.
//!
//! # Times
//!
//! `START` and each `DATA` carry the multimedia time at which their frame
//! was captured: milliseconds since the session began, the clock the main
//! channel's `INIT` and the display channel's stream frames use
//! ([`multimedia_time`]). Video is stamped a little ahead on that clock;
//! sound carries its capture time unshifted, so a client that syncs video
//! to audio, as spice-gtk does, delays video to match what it plays. Each
//! receiver's capture times never go backwards, so neither do the times
//! sent. `MODE` carries the time it was sent; clients ignore it. Andris
//! sends no `PLAYBACK_LATENCY`, as spice-server has not since v0.16.0, and
//! no `VOLUME` or `MUTE`.
//!
//! # Falling behind
//!
//! Sound is worth playing only on time, so the channel drops rather than
//! queues, as spice-server does ("prefer losing samples to queueing"):
//!
//! - The channel sends each frame as capture delivers it, paced by the
//!   sound server's clock, with no timer of its own.
//! - While a write is stalled, because the client is not reading, the
//!   channel's task waits in it and takes no frames, and capture's queue
//!   ([`AUDIO_QUEUE_FRAMES`](crate::platform::AUDIO_QUEUE_FRAMES) frames)
//!   drops its oldest as new ones arrive. Capture never waits, and memory
//!   never grows. The drops are counted in the session's metrics.
//! - So that it is that queue, and not the socket, that absorbs a stall,
//!   the socket is given a small `TCP_NOTSENT_LOWAT` ([`limit_unsent`]):
//!   Linux's send buffer grows to fit the connection, and without the
//!   limit held all of a two-second stall's raw sound on loopback, to be
//!   played late.
//! - There is no ack window: spice-server runs this channel without one,
//!   and the opcode table has no `SET_ACK`, so andris cannot send one.
//!   `ACK_SYNC` and `ACK` from the client are read and ignored, and
//!   `PONG`s timed, as on every channel.
//!
//! A write still stalled after the channel layer's write timeout closes
//! the channel, as on every other channel.
//!
//! # When capture fails
//!
//! Sound failing never ends the session. If capture cannot be opened,
//! or its receiver closes (no sound server, a source that has gone; the
//! source has already logged why), the channel stays linked and silent,
//! sending `STOP` first if the stream was started. It no longer waits
//! for frames, so it does not spin. Capture is tried again when the
//! client next links the channel. Closing the channel drops the receiver,
//! which stops capture.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use shakenfist_spice_protocol::link::SpiceStream;
use shakenfist_spice_protocol::messages::{
    MessageHeader, PlaybackMode, PlaybackPacket, PlaybackStart,
};
use shakenfist_spice_protocol::{audio_data_mode, audio_fmt, capabilities};
use socket2::SockRef;
use tracing::{debug, info, warn};

use super::channel::{ChannelConn, ChannelContext, ClientCaps};
use super::opcodes::PlaybackOpcode;
use super::stats::Stats;
use crate::platform::{AUDIO_CHANNELS, AUDIO_FRAME_BYTES, AUDIO_RATE, AudioFrame, AudioReceiver};

/// The loudest a sample may be in a silent frame: a frame is silent when
/// every sample is within this of zero.
///
/// An idle sink's monitor delivers exact zeros (PulseAudio and PipeWire
/// null sinks and the xrdp sink all did), so any threshold above zero
/// tells idle from playing. Eight leaves a margin for the dither or small
/// DC offset a sink might add, and is about -72 dBFS, below anything
/// audible.
pub const SILENCE_THRESHOLD: u16 = 8;

/// How many silent frames in a row stop the stream: 1 s.
///
/// Long enough to bridge the pauses inside speech and music, so the
/// stream does not flap; short enough that a paused video stops costing
/// the client some 100 kbit/s, and andris the encoder's time, within a
/// second.
pub const SILENCE_HOLD_FRAMES: u32 = 100;

/// The Opus encoder's bitrate, in bits a second.
pub const OPUS_BITRATE: i32 = 96_000;

/// The largest Opus packet sent, in bytes: the most spice-gtk and
/// spice-server accept for one 10 ms frame. Libopus is given a buffer of
/// this size, which it treats as a limit; at [`OPUS_BITRATE`] a packet is
/// about 120 bytes.
pub const MAX_OPUS_PACKET: usize = 480;

/// How many of the mode's largest `DATA` messages the kernel may hold
/// unsent before the channel's writes wait; see [`limit_unsent`].
const UNSENT_MESSAGES: usize = 2;

/// How `DATA` carries sound: the mode `MODE` announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Opus, one 10 ms frame a packet.
    Opus,
    /// S16LE stereo, 1920 bytes a frame.
    Raw,
}

impl Mode {
    /// Opus for a client that advertised `PLAYBACK_OPUS`, raw otherwise.
    pub fn for_caps(caps: &ClientCaps) -> Self {
        if caps.has_channel_mask(capabilities::PLAYBACK_OPUS) {
            Mode::Opus
        } else {
            Mode::Raw
        }
    }

    /// The mode's `audio_data_mode` value.
    pub fn wire(self) -> u16 {
        match self {
            Mode::Opus => audio_data_mode::OPUS,
            Mode::Raw => audio_data_mode::RAW,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Opus => "opus",
            Mode::Raw => "raw",
        }
    }

    /// The largest `DATA` body in this mode: the time, then one frame.
    pub fn max_data_body(self) -> usize {
        PlaybackPacket::HEADER_SIZE
            + match self {
                Mode::Opus => MAX_OPUS_PACKET,
                Mode::Raw => AUDIO_FRAME_BYTES,
            }
    }

    /// The `TCP_NOTSENT_LOWAT` for this mode: [`UNSENT_MESSAGES`] of its
    /// largest `DATA` messages, about 20 ms of sound.
    pub fn unsent_limit(self) -> u32 {
        let bytes = UNSENT_MESSAGES * (MessageHeader::SIZE + self.max_data_body());
        u32::try_from(bytes).unwrap_or(u32::MAX)
    }
}

/// The multimedia time of `at`, for a session that began at `started`:
/// milliseconds since it began, wrapping as a `u32` does after some 49
/// days, as spice-server's does. A time before the session began, which
/// a capture stamp less its latency can be, is 0.
pub fn multimedia_time(started: Instant, at: Instant) -> u32 {
    // Truncation is the wrap.
    at.saturating_duration_since(started).as_millis() as u32
}

/// Keep at most [`Mode::unsent_limit`] bytes waiting in the kernel for the
/// playback channel's socket, so a stalled client makes the channel's
/// writes wait, and capture's queue drop, within a few frames.
///
/// Without it, Linux's send buffer, which grows to fit the connection,
/// would hold seconds of sound for a client that has stopped reading, to
/// be played late once it reads again. Only unsent data is limited:
/// what the network holds in flight is not, so a slow link is not
/// throttled. A failure to set it costs only that, and is logged at
/// debug.
pub fn limit_unsent(stream: &SpiceStream, mode: Mode) {
    let tcp = match stream {
        SpiceStream::Plain(tcp) => tcp,
        SpiceStream::Tls(tls) => tls.get_ref().0,
        SpiceStream::TlsServer(tls) => tls.get_ref().0,
    };
    let limit = mode.unsent_limit();
    if let Err(e) = SockRef::from(tcp).set_tcp_notsent_lowat(limit) {
        debug!(error = %e, limit, "setting TCP_NOTSENT_LOWAT on the playback channel failed");
    }
}

/// What to do with one captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing: the stream is stopped and the frame is silent.
    Skip,
    /// Send `START`, then the frame.
    Start,
    /// Send the frame.
    Send,
    /// Send `STOP` in place of the frame: the stream has been silent for
    /// [`SILENCE_HOLD_FRAMES`] frames.
    Stop,
}

/// Decides, frame by frame, when the stream starts and stops: it starts
/// on the first frame that is not silent, and stops on the
/// [`SILENCE_HOLD_FRAMES`]th silent frame in a row. It begins stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SilenceGate {
    started: bool,
    /// Silent frames in a row while started.
    silent: u32,
}

impl SilenceGate {
    /// Whether a frame whose loudest sample has magnitude `peak` is
    /// silent.
    pub fn is_silent(peak: u16) -> bool {
        peak <= SILENCE_THRESHOLD
    }

    /// What to do with a frame whose loudest sample has magnitude `peak`
    /// (see [`AudioFrame::peak`]).
    pub fn step(&mut self, peak: u16) -> Step {
        let silent = Self::is_silent(peak);
        if !self.started {
            if silent {
                return Step::Skip;
            }
            self.started = true;
            self.silent = 0;
            return Step::Start;
        }
        if !silent {
            self.silent = 0;
            return Step::Send;
        }
        self.silent += 1;
        if self.silent >= SILENCE_HOLD_FRAMES {
            self.stop();
            Step::Stop
        } else {
            Step::Send
        }
    }

    /// Whether the stream is started.
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Stop the stream without a frame: capture has ended.
    pub fn stop(&mut self) {
        self.started = false;
        self.silent = 0;
    }
}

/// Encodes frames for `DATA` in the channel's mode.
enum Codec {
    Raw,
    Opus {
        encoder: opus::Encoder,
        /// Where a packet is encoded, [`MAX_OPUS_PACKET`] bytes.
        packet: Vec<u8>,
    },
}

impl Codec {
    fn new(mode: Mode) -> Result<Self> {
        match mode {
            Mode::Raw => Ok(Codec::Raw),
            Mode::Opus => {
                let mut encoder = opus::Encoder::new(
                    AUDIO_RATE,
                    opus::Channels::Stereo,
                    opus::Application::Audio,
                )
                .context("creating the Opus encoder")?;
                encoder
                    .set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))
                    .context("setting the Opus encoder's bitrate")?;
                Ok(Codec::Opus {
                    encoder,
                    packet: vec![0; MAX_OPUS_PACKET],
                })
            }
        }
    }

    /// Begin a new stream: Opus forgets the last one, as the client's
    /// decoder, made afresh at `START`, has.
    fn restart(&mut self) -> Result<()> {
        if let Codec::Opus { encoder, .. } = self {
            encoder
                .reset_state()
                .context("resetting the Opus encoder")?;
        }
        Ok(())
    }

    /// `frame` as `DATA` carries it, and how long Opus took to encode it.
    fn encode(&mut self, frame: &AudioFrame) -> Result<(Vec<u8>, Option<Duration>)> {
        match self {
            Codec::Raw => Ok((frame.to_le_bytes(), None)),
            Codec::Opus { encoder, packet } => {
                let began = Instant::now();
                let len = encoder
                    .encode(frame.samples(), packet)
                    .context("encoding an audio frame as Opus")?;
                let took = began.elapsed();
                Ok((packet[..len].to_vec(), Some(took)))
            }
        }
    }
}

/// The channel's state between frames.
struct Stream {
    started: Instant,
    codec: Codec,
    gate: SilenceGate,
    /// The capture's drop count when it was last read, so each read
    /// counts only the new drops.
    dropped: u64,
}

impl Stream {
    /// Count the drops `capture` has made since it was last read.
    fn count_drops(&mut self, capture: &AudioReceiver, stats: &Stats) {
        let dropped = capture.dropped();
        if dropped > self.dropped {
            stats.audio_dropped(dropped - self.dropped);
        }
        self.dropped = dropped;
    }

    /// Start, send or stop for `frame`, as the gate says.
    async fn frame(
        &mut self,
        conn: &mut ChannelConn<PlaybackOpcode>,
        frame: &AudioFrame,
    ) -> Result<()> {
        let time = multimedia_time(self.started, frame.captured_at());
        match self.gate.step(frame.peak()) {
            Step::Skip => Ok(()),
            Step::Start => {
                self.codec.restart()?;
                let start = PlaybackStart {
                    channels: AUDIO_CHANNELS as u32,
                    format: audio_fmt::S16,
                    frequency: AUDIO_RATE,
                    time,
                };
                conn.send_msg(PlaybackOpcode::Start, &start).await?;
                conn.session().counters().audio_started();
                debug!(session = conn.session().id(), time, "sound: START");
                self.send(conn, frame, time).await
            }
            Step::Send => self.send(conn, frame, time).await,
            Step::Stop => self.stop(conn).await,
        }
    }

    /// Send `frame` as `DATA` stamped `time`.
    async fn send(
        &mut self,
        conn: &mut ChannelConn<PlaybackOpcode>,
        frame: &AudioFrame,
        time: u32,
    ) -> Result<()> {
        let (data, took) = self.codec.encode(frame)?;
        if let Some(took) = took {
            conn.session().counters().audio_encode_done(took);
        }
        conn.send_msg(PlaybackOpcode::Data, &PlaybackPacket { time, data })
            .await?;
        conn.session().counters().audio_frame_sent();
        Ok(())
    }

    /// Send `STOP`.
    async fn stop(&mut self, conn: &mut ChannelConn<PlaybackOpcode>) -> Result<()> {
        self.gate.stop();
        conn.send(PlaybackOpcode::Stop, &[]).await?;
        conn.session().counters().audio_stopped();
        debug!(session = conn.session().id(), "sound: STOP");
        Ok(())
    }
}

/// The next frame from `capture`, or `None` once it has ended.
async fn next_frame(capture: Option<&mut AudioReceiver>) -> Option<AudioFrame> {
    capture?.recv().await
}

/// Open the platform's audio capture, or say why there is none.
fn open_capture(ctx: &ChannelContext) -> Option<AudioReceiver> {
    let session = ctx.session.id();
    let Some(source) = ctx.platform.audio.as_ref() else {
        // A playback link is refused without audio, so this is not
        // reached; staying silent is the right answer if it is.
        debug!(
            session,
            "the platform has no audio; the playback channel stays silent"
        );
        return None;
    };
    match source.open() {
        Ok(capture) => Some(capture),
        Err(e) => {
            warn!(
                session,
                error = format!("{e:#}"),
                "audio capture could not start; the playback channel stays open, silent"
            );
            None
        }
    }
}

/// Serve a playback channel until the client closes it.
pub async fn run(ctx: ChannelContext, mut conn: ChannelConn<PlaybackOpcode>) -> Result<()> {
    let session = ctx.session.clone();
    let id = session.id();
    let mode = Mode::for_caps(&ctx.caps);
    let announce = PlaybackMode {
        time: multimedia_time(session.started(), Instant::now()),
        mode: mode.wire(),
    };
    conn.send_msg(PlaybackOpcode::Mode, &announce).await?;
    let codec = Codec::new(mode).inspect_err(|e| {
        warn!(
            session = id,
            error = format!("{e:#}"),
            "the playback channel cannot encode; closing it"
        );
    })?;

    let mut capture = open_capture(&ctx);
    info!(
        session = id,
        mode = mode.name(),
        capture_opened = capture.is_some(),
        "playback channel open"
    );
    let mut stream = Stream {
        started: session.started(),
        codec,
        gate: SilenceGate::default(),
        dropped: 0,
    };
    loop {
        tokio::select! {
            message = conn.recv() => {
                let Some(message) = message? else {
                    return Ok(());
                };
                conn.ignore(&message);
            }
            frame = next_frame(capture.as_mut()), if capture.is_some() => {
                if let Some(receiver) = &capture {
                    stream.count_drops(receiver, session.counters());
                }
                match frame {
                    Some(frame) => stream.frame(&mut conn, &frame).await?,
                    None => {
                        debug!(
                            session = id,
                            "audio capture has ended; the playback channel stays open, silent"
                        );
                        capture = None;
                        if stream.gate.is_started() {
                            stream.stop(&mut conn).await?;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{AUDIO_FRAME_SAMPLES, AUDIO_FRAME_VALUES};

    const LOUD: u16 = 1000;

    #[test]
    fn the_mode_follows_the_opus_cap() {
        let caps = |channel: u32| ClientCaps {
            common: vec![],
            channel: vec![channel],
        };
        // Ryll's playback word: CELT and OPUS.
        assert_eq!(Mode::for_caps(&caps(0b1001)), Mode::Opus);
        assert_eq!(Mode::for_caps(&caps(1 << 3)), Mode::Opus);
        assert_eq!(Mode::for_caps(&caps(1)), Mode::Raw, "CELT alone");
        assert_eq!(Mode::for_caps(&ClientCaps::default()), Mode::Raw);
        assert_eq!(Mode::Opus.wire(), 3);
        assert_eq!(Mode::Raw.wire(), 1);
    }

    #[test]
    fn data_bodies_and_the_unsent_limit() {
        assert_eq!(Mode::Raw.max_data_body(), 4 + 1920);
        assert_eq!(Mode::Opus.max_data_body(), 4 + 480);
        assert_eq!(Mode::Raw.unsent_limit(), 2 * (6 + 4 + 1920));
        assert_eq!(Mode::Opus.unsent_limit(), 2 * (6 + 4 + 480));
    }

    #[test]
    fn multimedia_time_counts_milliseconds_and_saturates_before_the_start() {
        let started = Instant::now();
        assert_eq!(multimedia_time(started, started), 0);
        assert_eq!(
            multimedia_time(started, started + Duration::from_micros(12_999)),
            12
        );
        if let Some(before) = started.checked_sub(Duration::from_secs(1)) {
            assert_eq!(multimedia_time(started, before), 0);
        }
        // It wraps as a u32.
        let later = started + Duration::from_millis(u64::from(u32::MAX) + 6);
        assert_eq!(multimedia_time(started, later), 5);
    }

    #[test]
    fn the_gate_starts_on_sound_and_skips_silence_while_stopped() {
        let mut gate = SilenceGate::default();
        assert!(!gate.is_started());
        assert_eq!(gate.step(0), Step::Skip);
        assert_eq!(gate.step(SILENCE_THRESHOLD), Step::Skip, "at the threshold");
        assert_eq!(gate.step(SILENCE_THRESHOLD + 1), Step::Start);
        assert!(gate.is_started());
        assert_eq!(gate.step(LOUD), Step::Send);
    }

    #[test]
    fn the_gate_sends_silence_and_stops_on_the_hundredth_silent_frame() {
        let mut gate = SilenceGate::default();
        assert_eq!(gate.step(LOUD), Step::Start);
        for n in 1..SILENCE_HOLD_FRAMES {
            assert_eq!(gate.step(0), Step::Send, "silent frame {n}");
        }
        assert_eq!(gate.step(0), Step::Stop);
        assert!(!gate.is_started());
        assert_eq!(gate.step(0), Step::Skip);
        assert_eq!(gate.step(LOUD), Step::Start, "sound starts it again");
    }

    #[test]
    fn sound_resets_the_silence_count() {
        let mut gate = SilenceGate::default();
        gate.step(LOUD);
        for _ in 0..3 {
            for _ in 1..SILENCE_HOLD_FRAMES {
                assert_eq!(gate.step(3), Step::Send);
            }
            assert_eq!(gate.step(LOUD), Step::Send, "sound just in time");
        }
        assert!(gate.is_started());
    }

    #[test]
    fn stopping_the_gate_waits_for_sound_again() {
        let mut gate = SilenceGate::default();
        gate.step(LOUD);
        gate.stop();
        assert!(!gate.is_started());
        assert_eq!(gate.step(0), Step::Skip);
        assert_eq!(gate.step(LOUD), Step::Start);
    }

    /// A frame of a 1 kHz sine at a quarter of full scale.
    fn sine(at: Instant) -> AudioFrame {
        let mut samples = vec![0i16; AUDIO_FRAME_VALUES];
        for (n, pair) in samples.chunks_mut(AUDIO_CHANNELS).enumerate() {
            let angle = std::f64::consts::TAU * 1000.0 * n as f64 / f64::from(AUDIO_RATE);
            pair.fill((8192.0 * angle.sin()) as i16);
        }
        AudioFrame::new(at, samples).unwrap()
    }

    #[test]
    fn raw_data_is_the_frames_bytes() {
        let frame = sine(Instant::now());
        let mut codec = Codec::new(Mode::Raw).unwrap();
        let (data, took) = codec.encode(&frame).unwrap();
        assert_eq!(data, frame.to_le_bytes());
        assert_eq!(data.len(), AUDIO_FRAME_BYTES);
        assert_eq!(took, None);
    }

    #[test]
    fn opus_packets_are_ten_ms_and_within_the_limit() {
        let mut codec = Codec::new(Mode::Opus).unwrap();
        let mut decoder = opus::Decoder::new(AUDIO_RATE, opus::Channels::Stereo).unwrap();
        let now = Instant::now();
        for n in 0..50 {
            if n == 25 {
                codec.restart().unwrap();
                decoder.reset_state().unwrap();
            }
            let (packet, took) = codec.encode(&sine(now)).unwrap();
            assert!(took.is_some());
            assert!(!packet.is_empty() && packet.len() <= MAX_OPUS_PACKET);
            assert_eq!(
                opus::packet::get_nb_samples(&packet, AUDIO_RATE).unwrap(),
                AUDIO_FRAME_SAMPLES
            );
            let mut pcm = [0i16; AUDIO_FRAME_VALUES];
            assert_eq!(
                decoder.decode(&packet, &mut pcm, false).unwrap(),
                AUDIO_FRAME_SAMPLES
            );
        }
    }
}
