//! Capturing the desktop's sound over the PulseAudio protocol, from a
//! source: by default the monitor of the session's default sink, which
//! carries whatever the desktop plays.
//!
//! The protocol is served by PulseAudio and by PipeWire's
//! `pipewire-pulse` alike, and an xrdp sink module loaded into either
//! appears as one more sink with a monitor, so one client covers them
//! all. It uses libpulse's simple API, whose calls block, so each capture
//! runs on a thread of its own.
//!
//! # A capture
//!
//! [`AudioSource::open`] spawns the thread and returns at once. The thread
//! connects to the sound server libpulse finds (from `PULSE_SERVER`, the
//! session's `XDG_RUNTIME_DIR`, or the X display's root window, as for any
//! client), and records the source as S16LE stereo at 48 kHz. It reads one
//! frame, [`AUDIO_FRAME_BYTES`] bytes, at a time: `pa_simple_read` returns
//! only once the whole buffer is filled, so every read is exactly one
//! frame and nothing needs re-chunking. The fragment size is set to one
//! frame too ([`buffer_attr`]); left to the server, PulseAudio delivers
//! two-second bursts.
//!
//! Each frame is stamped with the time it was read less the latency
//! libpulse reports for the stream, held so that it never goes backwards
//! (see [`MonotonicStamps`]).
//!
//! The source is resolved when the stream connects. If the default sink
//! changes during a capture, the capture keeps the old one until it is
//! opened again.
//!
//! # Stopping
//!
//! The thread checks whether the receiver has gone after every read, and
//! exits, disconnecting from the sound server, once it has. A read
//! normally returns within 10 ms, but can take seconds while a suspended
//! sink wakes (a PulseAudio null sink has taken two), so the thread is
//! never joined: dropping the receiver returns at once, and the thread
//! follows when its read returns.
//!
//! A capture that cannot connect, names a source that does not exist, or
//! fails part way logs one warning and exits, which closes the receiver.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use tracing::{debug, info, warn};

use super::audio::MonotonicStamps;
use super::{
    AUDIO_CHANNELS, AUDIO_FRAME_BYTES, AUDIO_RATE, AudioFrame, AudioReceiver, AudioSender,
    AudioSource, audio_queue,
};

/// The source `--audio-source` names by default: PulseAudio's alias for
/// the monitor of the default sink, which pipewire-pulse also resolves.
pub const DEFAULT_SOURCE: &str = "@DEFAULT_MONITOR@";

/// The client name the sound server shows for andris, in `pactl list
/// source-outputs` and mixers.
const APPLICATION_NAME: &str = "andris";

/// The stream name the sound server shows for a capture.
const STREAM_NAME: &str = "desktop audio";

/// Captures a PulseAudio source. Cloning it is cheap; each
/// [`AudioSource::open`] is a capture of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulseSource {
    source: String,
    server: Option<String>,
}

impl PulseSource {
    /// Capture `source`, a PulseAudio source name such as
    /// [`DEFAULT_SOURCE`], from the sound server libpulse finds.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            server: None,
        }
    }

    /// Connect to `server`, in libpulse's server string syntax
    /// (`unix:/path/to/native`, say), rather than the one libpulse finds.
    /// For tests: andris itself always leaves the choice to libpulse.
    pub fn with_server(mut self, server: impl Into<String>) -> Self {
        self.server = Some(server.into());
        self
    }

    /// The source captured.
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl AudioSource for PulseSource {
    fn open(&self) -> Result<AudioReceiver> {
        let (sender, receiver) = audio_queue();
        let source = self.clone();
        thread::Builder::new()
            .name("andris-pulse".to_string())
            .spawn(move || source.capture(&sender))
            .context("starting the PulseAudio capture thread")?;
        Ok(receiver)
    }
}

impl PulseSource {
    /// The capture thread: record until the receiver goes or capture
    /// fails, and log a failure once. Returning drops `sender`, which
    /// closes the receiver.
    fn capture(&self, sender: &AudioSender) {
        let source = &self.source;
        match self.record(sender) {
            Ok(()) => debug!("stopped capturing PulseAudio source {source}: nobody is listening"),
            Err(err) => warn!(
                "cannot capture sound from PulseAudio source {source}, so clients hear nothing \
                 until they reconnect: {err:#}"
            ),
        }
    }

    /// Connect, then read frames into `sender` until its receiver goes.
    fn record(&self, sender: &AudioSender) -> Result<()> {
        let source = &self.source;
        // libpulse-simple-binding panics on a NUL in a name, so refuse it
        // here; a command-line argument cannot hold one.
        ensure!(
            !source.contains('\0') && !self.server.as_deref().unwrap_or("").contains('\0'),
            "the source or server name holds a NUL byte"
        );
        let spec = Spec {
            format: Format::S16le,
            rate: AUDIO_RATE,
            channels: AUDIO_CHANNELS as u8,
        };
        let stream = Simple::new(
            self.server.as_deref(),
            APPLICATION_NAME,
            Direction::Record,
            Some(source),
            STREAM_NAME,
            &spec,
            None,
            Some(&buffer_attr()),
        )
        .context("connecting to the sound server")?;
        info!("capturing sound from PulseAudio source {source}");

        let mut stamps = MonotonicStamps::default();
        let mut bytes = [0; AUDIO_FRAME_BYTES];
        while !sender.is_closed() {
            stream
                .read(&mut bytes)
                .context("reading from the sound server")?;
            let read_at = Instant::now();
            let latency = Duration::from(
                stream
                    .get_latency()
                    .context("reading the capture latency from the sound server")?,
            );
            let frame = AudioFrame::from_le_bytes(stamps.stamp(read_at, latency), &bytes)?;
            if !sender.send(frame) {
                break;
            }
        }
        Ok(())
    }
}

/// The buffering asked of the sound server: deliver a fragment of one
/// frame, so frames arrive as they are captured, and leave the rest to the
/// server. `u32::MAX` is libpulse's "server default"; the other fields
/// apply only to playback.
fn buffer_attr() -> BufferAttr {
    BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: AUDIO_FRAME_BYTES as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_one_frame_fragments() {
        let attr = buffer_attr();
        assert_eq!(attr.fragsize, 1920);
        assert_eq!(attr.maxlength, u32::MAX);
    }

    #[test]
    fn the_default_source_is_the_default_sinks_monitor() {
        assert_eq!(
            PulseSource::new(DEFAULT_SOURCE).source(),
            "@DEFAULT_MONITOR@"
        );
    }

    /// Wait up to `wait` for `receiver` to close, failing if a frame
    /// arrives instead.
    async fn closes_within(receiver: &mut AudioReceiver, wait: Duration) -> bool {
        match tokio::time::timeout(wait, receiver.recv()).await {
            Ok(None) => true,
            Ok(Some(frame)) => panic!("a capture that should fail delivered {frame:?}"),
            Err(_) => false,
        }
    }

    #[tokio::test]
    async fn no_sound_server_closes_the_receiver_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        // A socket that does not exist: no sound server, and nothing
        // libpulse could spawn one at, since it spawns one only when left
        // to pick the server itself.
        let server = format!("unix:{}", dir.path().join("native").display());
        let source = PulseSource::new(DEFAULT_SOURCE).with_server(server);
        let opened = Instant::now();
        let mut receiver = source.open().unwrap();
        assert!(
            opened.elapsed() < Duration::from_millis(500),
            "open blocked for {:?}",
            opened.elapsed()
        );
        assert!(closes_within(&mut receiver, Duration::from_secs(10)).await);
        assert_eq!(receiver.dropped(), 0);
        // A second open tries again, and fails the same way.
        let mut again = source.open().unwrap();
        assert!(closes_within(&mut again, Duration::from_secs(10)).await);
    }

    #[tokio::test]
    async fn a_nul_in_the_source_closes_the_receiver_without_panicking() {
        let mut receiver = PulseSource::new("bad\0name").open().unwrap();
        assert!(closes_within(&mut receiver, Duration::from_secs(5)).await);
    }
}
