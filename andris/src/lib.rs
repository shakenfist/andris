//! Andris, a SPICE server that exports an existing X11 desktop.
//!
//! The binary is a thin wrapper around this library, so that tests can
//! drive the server in-process.
//!
//! - [`config`] holds the command line and its validation;
//! - [`install`] holds `andris install-unit`, which installs the systemd
//!   user unit;
//! - [`platform`] holds the seams between the server and the desktop,
//!   and their implementations;
//! - [`server`] holds the SPICE server.
//!
//! Unsafe code is denied crate-wide. There are two exceptions, which
//! alone allow it and justify each block: the X11 backend's MIT-SHM
//! capture, `platform::x11::shm`, which maps memory the X server writes,
//! and the bindings to libpulse, `platform::pulse::ffi`, which andris
//! loads at run time rather than links.

#![deny(unsafe_code)]

pub mod config;
pub mod install;
pub mod platform;
pub mod server;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tracing::{error, info, warn};

use config::{AudioConfig, Config, PlatformConfig};
use platform::pulse::{self, PulseSource};
use platform::synthetic::{SyntheticPlatform, ToneSource};
use platform::x11::X11Platform;
use platform::{AudioSource, GoneSignal, Platform};
use server::{DisplayOptions, HandshakeTimeouts, STOP_GRACE, Server, ServerHandle, ServerOptions};

/// How long andris waits for connections to close once the desktop has
/// gone. There is no desktop left to take released keys, so this only
/// lets each client see its connection close.
pub const GONE_GRACE: Duration = Duration::from_secs(1);

/// Andris's name and version, as `--version` prints them.
pub fn banner() -> String {
    format!("andris {}", env!("CARGO_PKG_VERSION"))
}

/// Run andris with a validated configuration: build the platform the
/// configuration names, start the listeners, and serve until SIGINT or
/// SIGTERM, or until the desktop goes. Fails, naming why, if the desktop
/// went.
///
/// Sound is resolved before the desktop is opened, so that a missing
/// libpulse is reported even when opening the desktop then fails.
pub fn run(config: Config) -> Result<()> {
    let audio = audio_source(&config.audio);
    let mut platform = match &config.platform {
        PlatformConfig::Synthetic { size } => {
            info!("exporting a synthetic {size} desktop");
            SyntheticPlatform::new(*size)?.platform()
        }
        PlatformConfig::X11 { display: name } => {
            info!("exporting X display {name}");
            X11Platform::connect(name)?.platform()
        }
    };
    platform.audio = audio;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    runtime.block_on(serve(config, platform))
}

/// The sound source `audio` names, saying which in the log. Nothing is
/// captured yet: capture starts when a client listens.
///
/// PulseAudio capture needs libpulse, which is loaded here. If it cannot
/// be, andris logs why, with the package to install, and serves without
/// sound, as with `--no-audio`: no source, so no playback channel.
fn audio_source(audio: &AudioConfig) -> Option<Arc<dyn AudioSource>> {
    audio_source_from(audio, pulse::LIBRARY)
}

/// [`audio_source`], loading libpulse from the library `file`, which
/// tests name to see it missing.
fn audio_source_from(audio: &AudioConfig, file: &str) -> Option<Arc<dyn AudioSource>> {
    match audio {
        AudioConfig::Off => {
            info!("sound is off");
            None
        }
        AudioConfig::Pulse { source } => {
            if let Err(err) = pulse::load_from(file) {
                warn!(
                    "cannot load libpulse, so andris serves without sound; install libpulse0 \
                     (Debian, Ubuntu) or pulseaudio-libs (Fedora, RHEL) for sound: {err}"
                );
                return None;
            }
            info!("sound comes from PulseAudio source {source}, captured while a client listens");
            Some(Arc::new(PulseSource::new(source.as_str())))
        }
        AudioConfig::Tone => {
            info!("sound is a synthetic tone");
            Some(Arc::new(ToneSource::default()))
        }
    }
}

/// Serve `platform` as `config` says, as [`run`] describes.
async fn serve(config: Config, platform: Platform) -> Result<()> {
    let size = platform.geometry.size()?;
    info!("the desktop is {size}");
    let options = ServerOptions {
        listen: config.listen,
        tls: config.tls,
        ticket: config.ticket,
        display: DisplayOptions {
            frame_interval: config.frame_interval,
            jpeg_quality: config.jpeg_quality,
            ..DisplayOptions::default()
        },
        timeouts: HandshakeTimeouts::default(),
    };
    let gone = platform.gone.clone();
    // Listen before the listeners are ready, so that a signal sent once
    // andris has said it is listening is never met by the default action,
    // which would end the process without releasing held input.
    let signals = Signals::listen()?;
    let handle = Server::bind(options, platform).await?.spawn()?;
    serve_until(handle, signals.next(), gone).await
}

/// SIGINT and SIGTERM, each listened for from [`Signals::listen`] on.
struct Signals {
    interrupt: Signal,
    terminate: Signal,
}

impl Signals {
    fn listen() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("listening for SIGINT")?,
            terminate: signal(SignalKind::terminate()).context("listening for SIGTERM")?,
        })
    }

    /// Wait for either signal, and name the one that came.
    async fn next(mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}

/// Run until `signal` returns or `gone` is set, then stop `handle`.
///
/// When `signal` returns, with the name of the signal that came, this logs
/// that and stops the server, waiting up to [`STOP_GRACE`] for the input
/// that connected clients held to be released, and returns `Ok`.
///
/// When the desktop goes, it logs that and stops the server, waiting only
/// [`GONE_GRACE`]: nothing held can be released into a desktop that has
/// gone, but each client still sees its connection close. It then fails,
/// naming why, so that the process exits with an error.
///
/// `andris` passes the signals it listens for as `signal`. Tests pass a
/// future of their own, so that no real signal is sent inside the test
/// process.
pub async fn serve_until(
    mut handle: ServerHandle,
    signal: impl Future<Output = &'static str>,
    mut gone: GoneSignal,
) -> Result<()> {
    let why = tokio::select! {
        name = signal => {
            info!("{name}: shutting down");
            if !handle.stop(STOP_GRACE).await {
                warn!(
                    "connections were still closing after {}s; keys or buttons held on the \
                     desktop may not have been released",
                    STOP_GRACE.as_secs()
                );
            }
            return Ok(());
        }
        why = gone.wait() => why,
    };
    // Not why: the error returned below says it, and main prints that.
    error!("the desktop has gone, so andris is stopping");
    if !handle.stop(GONE_GRACE).await {
        warn!(
            "connections were still closing after {}s; stopping anyway",
            GONE_GRACE.as_secs()
        );
    }
    bail!("the desktop has gone: {why}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audio_configuration_picks_the_source() {
        assert!(audio_source(&AudioConfig::Off).is_none());
        // Neither starts capturing until opened, so neither needs a sound
        // server or a runtime here. PulseAudio capture does need libpulse,
        // which the development image installs.
        assert!(audio_source(&AudioConfig::Tone).is_some());
        assert!(
            audio_source(&AudioConfig::Pulse {
                source: "@DEFAULT_MONITOR@".into()
            })
            .is_some()
        );
    }

    /// Log lines, collected for a test to read.
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn without_libpulse_there_is_no_sound_and_a_warning_says_why() {
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let source = tracing::subscriber::with_default(subscriber, || {
            audio_source_from(
                &AudioConfig::Pulse {
                    source: "@DEFAULT_MONITOR@".into(),
                },
                "libandris-no-such-libpulse.so.0",
            )
        });
        assert!(source.is_none(), "a PulseAudio source without libpulse");
        let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = logs.lines().collect();
        assert_eq!(lines.len(), 1, "{logs}");
        let line = lines[0];
        assert!(line.contains("WARN"), "{line}");
        for expected in [
            "cannot load libpulse",
            "libandris-no-such-libpulse.so.0",
            "serves without sound",
            "libpulse0 (Debian, Ubuntu)",
            "pulseaudio-libs (Fedora, RHEL)",
        ] {
            assert!(line.contains(expected), "{expected:?} missing from {line}");
        }
    }

    #[test]
    fn banner_has_name_and_version() {
        let banner = banner();
        let version = banner.strip_prefix("andris ").unwrap();
        assert!(!version.is_empty(), "no version in banner: {banner:?}");
        assert!(
            !version.chars().any(char::is_whitespace),
            "version contains whitespace: {version:?}"
        );
    }
}
