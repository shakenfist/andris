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
//! # Loading libpulse
//!
//! Andris does not link libpulse: [`load`] loads it the first time it is
//! needed, through the bindings in `ffi`, and keeps it loaded for the
//! life of the process. Without libpulse installed, andris still starts,
//! and serves without sound. A capture loads it too, if nothing has, and
//! fails, as any capture can, if it cannot.
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
//!
//! libpulse's simple API has no timeout, so a sound server that hangs
//! with its socket open leaves the thread blocked in a connect or a read
//! for as long as it hangs. A client relinking the playback channel opens
//! a capture each time, so the threads a [`PulseSource`] has live are
//! counted, and once [`MAX_LIVE_CAPTURES`] are, `open` refuses until one
//! exits.

// Unsafe code, as only this and the two shared-memory mappings (the X11
// platform's and the qemu platform's) are: the foreign function
// interface to libpulse. The module allows it itself.
mod ffi;

use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};

pub use ffi::LoadError;
use ffi::{Api, BufferAttr, PA_SAMPLE_S16LE, SampleSpec, Simple};

use super::audio::MonotonicStamps;
use super::{
    AUDIO_CHANNELS, AUDIO_FRAME_BYTES, AUDIO_RATE, AudioFrame, AudioReceiver, AudioSender,
    AudioSource, audio_queue,
};

/// The library [`load`] loads, by its soname. It brings in `libpulse.so.0`
/// as its own dependency. Both are in Debian and Ubuntu's `libpulse0`, and
/// Fedora and RHEL's `pulseaudio-libs`, which most desktops have
/// installed, since most programs that play sound link libpulse.
pub const LIBRARY: &str = "libpulse-simple.so.0";

/// libpulse, once [`load`] has tried to load it: loaded for the life of
/// the process, or why it could not be. It is never unloaded, since a
/// capture thread is never joined (see *Stopping*), so one may still be
/// inside a read when everything else has gone.
static LOADED: OnceLock<Result<Api, LoadError>> = OnceLock::new();

/// Load libpulse ([`LIBRARY`]), if this is the first call, and say
/// whether it is loaded. Every later call gives the first call's answer,
/// without trying again.
pub fn load() -> Result<(), LoadError> {
    api().map(|_| ())
}

/// [`load`], but from the library `file`. Only [`LIBRARY`] is kept loaded
/// for captures to use. Any other name is a test's, which needs only to
/// see whether it loads, so it is loaded afresh each call and unloaded
/// again at once; no capture can be using it.
pub(crate) fn load_from(file: &str) -> Result<(), LoadError> {
    if file == LIBRARY {
        load()
    } else {
        Api::open(file).map(drop)
    }
}

/// The loaded library, loading it on the first call.
fn api() -> Result<&'static Api, LoadError> {
    LOADED
        .get_or_init(|| Api::open(LIBRARY))
        .as_ref()
        .map_err(Clone::clone)
}

/// The source `--audio-source` names by default: PulseAudio's alias for
/// the monitor of the default sink, which pipewire-pulse also resolves.
pub const DEFAULT_SOURCE: &str = "@DEFAULT_MONITOR@";

/// The client name the sound server shows for andris, in `pactl list
/// source-outputs` and mixers.
const APPLICATION_NAME: &str = "andris";

/// The stream name the sound server shows for a capture.
const STREAM_NAME: &str = "desktop audio";

/// The most capture threads a [`PulseSource`] may have live at once. One
/// a session is normal; a stuck one lingers until its read returns, so a
/// few more leave room for a client to relink past a slow wake without
/// letting a hung sound server collect a thread and a connection per
/// relink.
pub const MAX_LIVE_CAPTURES: usize = 4;

/// Captures a PulseAudio source. Cloning it is cheap, and a clone shares
/// the count of live captures; each [`AudioSource::open`] is a capture of
/// its own.
#[derive(Debug, Clone)]
pub struct PulseSource {
    source: String,
    server: Option<String>,
    live: LiveCaptures,
}

impl PulseSource {
    /// Capture `source`, a PulseAudio source name such as
    /// [`DEFAULT_SOURCE`], from the sound server libpulse finds.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            server: None,
            live: LiveCaptures::default(),
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
    /// Fails, as when the thread cannot be spawned, if
    /// [`MAX_LIVE_CAPTURES`] captures are live already.
    fn open(&self) -> Result<AudioReceiver> {
        let slot = self.live.claim().ok_or_else(|| {
            anyhow!(
                "{MAX_LIVE_CAPTURES} PulseAudio captures are still running, waiting on the \
                 sound server"
            )
        })?;
        let (sender, receiver) = audio_queue();
        let source = self.clone();
        thread::Builder::new()
            .name("andris-pulse".to_string())
            .spawn(move || {
                // Declared after the sender, so dropped before it, on a
                // return or a panic: the slot is free by the time the
                // receiver closes.
                let sender = sender;
                let _slot = slot;
                source.capture(&sender);
            })
            .context("starting the PulseAudio capture thread")?;
        Ok(receiver)
    }
}

/// The count of a [`PulseSource`]'s live capture threads, shared by its
/// clones.
#[derive(Debug, Clone, Default)]
struct LiveCaptures(Arc<AtomicUsize>);

impl LiveCaptures {
    /// Count one more capture, or `None` if [`MAX_LIVE_CAPTURES`] are
    /// counted already. The capture is counted until the slot is dropped.
    fn claim(&self) -> Option<CaptureSlot> {
        let mut live = self.0.load(Ordering::Acquire);
        loop {
            if live >= MAX_LIVE_CAPTURES {
                return None;
            }
            match self
                .0
                .compare_exchange_weak(live, live + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(CaptureSlot(Arc::clone(&self.0))),
                Err(now) => live = now,
            }
        }
    }

    #[cfg(test)]
    fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

/// One counted capture, uncounted when dropped.
#[derive(Debug)]
struct CaptureSlot(Arc<AtomicUsize>);

impl Drop for CaptureSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
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
        let api = api().context("loading libpulse")?;
        // libpulse takes C strings, which cannot hold a NUL, so a name
        // with one is refused here; a command-line argument cannot hold
        // one.
        let nul = || anyhow!("the source or server name holds a NUL byte");
        let c_source = CString::new(source.as_str()).map_err(|_| nul())?;
        let c_server = self
            .server
            .as_deref()
            .map(CString::new)
            .transpose()
            .map_err(|_| nul())?;
        let c_name = CString::new(APPLICATION_NAME)?;
        let c_stream_name = CString::new(STREAM_NAME)?;
        let spec = SampleSpec {
            format: PA_SAMPLE_S16LE,
            rate: AUDIO_RATE,
            channels: AUDIO_CHANNELS as u8,
        };
        let stream = Simple::record(
            api,
            c_server.as_deref(),
            &c_name,
            &c_source,
            &c_stream_name,
            &spec,
            &buffer_attr(),
        )
        .context("connecting to the sound server")?;
        info!("capturing sound from PulseAudio source {source}");

        let mut stamps = MonotonicStamps::default();
        let mut bytes = [0; AUDIO_FRAME_BYTES];
        let mut latency_failed = false;
        while !sender.is_closed() {
            stream
                .read(&mut bytes)
                .context("reading from the sound server")?;
            let read_at = Instant::now();
            // The latency only moves the stamp earlier, and the stamps
            // never go backwards, so a failed read costs a stamp a little
            // late rather than the capture.
            let latency = match stream.latency() {
                Ok(latency) => latency,
                Err(e) => {
                    if !latency_failed {
                        latency_failed = true;
                        debug!(
                            error = %e,
                            "the sound server did not report the capture latency; stamping without it"
                        );
                    }
                    Duration::ZERO
                }
            };
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

    #[test]
    fn live_captures_are_capped_and_a_dropped_slot_is_freed() {
        let live = LiveCaptures::default();
        let mut slots: Vec<CaptureSlot> = (0..MAX_LIVE_CAPTURES)
            .map(|_| live.claim().unwrap())
            .collect();
        assert_eq!(live.count(), MAX_LIVE_CAPTURES);
        assert!(live.claim().is_none(), "one more than the cap");
        assert_eq!(live.count(), MAX_LIVE_CAPTURES);
        slots.pop();
        assert_eq!(live.count(), MAX_LIVE_CAPTURES - 1);
        slots.push(live.claim().unwrap());
        assert!(live.claim().is_none());
        // A clone shares the count.
        assert!(live.clone().claim().is_none());
        // A thread that panics frees its slot as it unwinds.
        let slot = slots.pop().unwrap();
        let panicked = thread::spawn(move || {
            let _slot = slot;
            panic!("a capture thread panicking");
        })
        .join();
        assert!(panicked.is_err());
        assert_eq!(live.count(), MAX_LIVE_CAPTURES - 1);
        drop(slots);
        assert_eq!(live.count(), 0);
    }

    #[tokio::test]
    async fn captures_stuck_on_a_hung_server_are_capped_until_they_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native");
        // A sound server that hangs: connections queue at its socket, and
        // nothing ever answers them, so each capture waits in its connect.
        let hung = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let source =
            PulseSource::new(DEFAULT_SOURCE).with_server(format!("unix:{}", path.display()));
        // Clones, as each session's playback channel holds the platform's.
        let mut stuck: Vec<AudioReceiver> = (0..MAX_LIVE_CAPTURES)
            .map(|_| source.clone().open().unwrap())
            .collect();
        let message = format!("{:#}", source.open().unwrap_err());
        assert!(message.contains("still running"), "{message}");
        assert_eq!(source.live.count(), MAX_LIVE_CAPTURES);

        // The server goes: each capture fails, and its thread frees its
        // slot before its receiver closes.
        drop(hung);
        for receiver in &mut stuck {
            assert!(closes_within(receiver, Duration::from_secs(10)).await);
        }
        assert_eq!(source.live.count(), 0);
        let mut again = source.open().unwrap();
        assert!(closes_within(&mut again, Duration::from_secs(10)).await);
        assert_eq!(source.live.count(), 0);
    }

    #[tokio::test]
    async fn a_nul_in_the_source_closes_the_receiver_without_panicking() {
        let mut receiver = PulseSource::new("bad\0name").open().unwrap();
        assert!(closes_within(&mut receiver, Duration::from_secs(5)).await);
    }
}
