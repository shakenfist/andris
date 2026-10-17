//! PulseAudio capture against a real sound server: a tone played into the
//! default sink arrives through its monitor as 10 ms frames, stamped in
//! order; the stream goes from the server once the receiver is dropped;
//! and a source that does not exist closes the receiver.
//!
//! It runs only when `ANDRIS_TEST_PULSE` is set to anything non-empty,
//! with `PULSE_SERVER` naming a sound server whose default sink is one
//! nothing else plays into: a PulseAudio null sink at 48 kHz, say. With
//! `ANDRIS_TEST_PULSE` unset, as under `make test`, the test says on
//! stderr that it is skipping, and passes without doing anything. With it
//! set and `PULSE_SERVER` unset or empty, the test fails rather than let
//! libpulse find a server through `XDG_RUNTIME_DIR`, which would be the
//! desktop's own: it plays a tone there.
//!
//! The tone is played with `pacat`, and the stream's removal is checked
//! with `pactl`; both must be on the `PATH`.

// Test helpers, where a failed unwrap is a failed test. clippy's
// allow-unwrap-in-tests covers only #[test] functions themselves.
#![allow(clippy::unwrap_used)]

use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use andris::platform::pulse::{DEFAULT_SOURCE, PulseSource};
use andris::platform::{AUDIO_FRAME_VALUES, AudioFrame, AudioReceiver, AudioSource};

/// The tone's pitch and peak.
const FREQUENCY: f64 = 440.0;
const AMPLITUDE: f64 = 8192.0;

/// How long to wait for the tone to come through. A PulseAudio null sink
/// can take two seconds to wake for its first read.
const ARRIVAL: Duration = Duration::from_secs(10);

/// Held by each test while it uses the sound server: the tone of one would
/// reach the other's capture.
static SERVER_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Whether to run, saying so on stderr when not. Panics when asked to run
/// without an explicit `PULSE_SERVER`.
fn enabled() -> bool {
    let set = |name: &str| std::env::var(name).is_ok_and(|value| !value.is_empty());
    if !set("ANDRIS_TEST_PULSE") {
        // Straight to stderr: libtest captures eprintln!, which would
        // hide that this passed without testing anything.
        let _ = writeln!(
            std::io::stderr(),
            "ANDRIS_TEST_PULSE is unset; SKIPPING the PulseAudio capture test"
        );
        return false;
    }
    assert!(
        set("PULSE_SERVER"),
        "ANDRIS_TEST_PULSE is set but PULSE_SERVER is not: name the sound server to play \
         the test tone into, rather than letting libpulse find the desktop's own"
    );
    true
}

/// `pacat` playing a sine into the default sink until killed. Killed on
/// drop.
struct Tone(Child);

impl Tone {
    fn play() -> Self {
        let mut child = Command::new("pacat")
            .args([
                "--playback",
                "--format=s16le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=20",
                "--stream-name=andris test tone",
            ])
            .stdin(Stdio::piped())
            .spawn()
            .expect("pacat could not be run");
        let mut stdin = child.stdin.take().unwrap();
        std::thread::spawn(move || {
            // A second of the tone at a time, until pacat goes.
            let second: Vec<u8> = (0..48_000)
                .flat_map(|n| {
                    let phase = std::f64::consts::TAU * FREQUENCY * f64::from(n) / 48_000.0;
                    let sample = ((AMPLITUDE * phase.sin()).round() as i16).to_le_bytes();
                    [sample, sample].concat()
                })
                .collect();
            while stdin.write_all(&second).is_ok() {}
        });
        Self(child)
    }
}

impl Drop for Tone {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The next frame, failing the test if none comes within [`ARRIVAL`].
async fn next(receiver: &mut AudioReceiver) -> AudioFrame {
    tokio::time::timeout(ARRIVAL, receiver.recv())
        .await
        .expect("no frame arrived")
        .expect("the capture closed")
}

/// How many record streams the sound server has.
fn record_streams() -> usize {
    let output = Command::new("pactl")
        .args(["list", "short", "source-outputs"])
        .output()
        .expect("pactl could not be run");
    assert!(output.status.success(), "pactl failed: {output:?}");
    String::from_utf8(output.stdout).unwrap().lines().count()
}

#[tokio::test]
async fn a_tone_arrives_through_the_monitor() {
    if !enabled() {
        return;
    }
    let _turn = SERVER_TURN.lock().await;
    let before = record_streams();
    let mut receiver = PulseSource::new(DEFAULT_SOURCE).open().unwrap();
    let _tone = Tone::play();

    // Skip the silence before the tone reaches the monitor.
    let waiting = Instant::now();
    let mut frame = next(&mut receiver).await;
    while frame.peak() < 1000 {
        assert!(waiting.elapsed() < ARRIVAL, "the tone never arrived");
        frame = next(&mut receiver).await;
    }

    // Half a second of the tone: whole frames, stamped in order and never
    // after they were read, at the tone's pitch and level.
    let mut left = Vec::new();
    let mut last = frame.captured_at();
    for _ in 0..50 {
        let frame = next(&mut receiver).await;
        assert_eq!(frame.samples().len(), AUDIO_FRAME_VALUES);
        assert!(frame.captured_at() >= last, "a stamp went backwards");
        assert!(frame.captured_at() <= Instant::now());
        last = frame.captured_at();
        let peak = frame.peak();
        assert!((7000..=9000).contains(&peak), "a frame's peak is {peak}");
        left.extend(
            frame
                .samples()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| pair[0]),
        );
    }
    let cycles = left
        .windows(2)
        .filter(|pair| pair[0] < 0 && pair[1] >= 0)
        .count();
    assert!(
        (210..=230).contains(&cycles),
        "{cycles} cycles in half a second of a 440 Hz tone"
    );
    let latency = last.elapsed();
    assert!(
        latency < Duration::from_secs(1),
        "frames are {latency:?} old"
    );

    // Dropping the receiver ends the stream on the server.
    assert_eq!(record_streams(), before + 1);
    drop(receiver);
    let dropped = Instant::now();
    while record_streams() > before {
        assert!(
            dropped.elapsed() < Duration::from_secs(5),
            "the record stream outlived its receiver"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_missing_source_closes_the_receiver() {
    if !enabled() {
        return;
    }
    let _turn = SERVER_TURN.lock().await;
    let mut receiver = PulseSource::new("andris-no-such-source").open().unwrap();
    let ended = tokio::time::timeout(ARRIVAL, receiver.recv())
        .await
        .expect("the capture neither failed nor delivered");
    assert!(ended.is_none(), "a missing source delivered {ended:?}");
}
