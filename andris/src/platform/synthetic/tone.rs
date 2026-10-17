//! The synthetic platform's sound: a sine that sounds and falls silent
//! in turn, paced by the async runtime's clock.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::platform::{
    AUDIO_CHANNELS, AUDIO_FRAME_DURATION, AUDIO_FRAME_SAMPLES, AUDIO_FRAME_VALUES, AUDIO_RATE,
    AudioFrame, AudioReceiver, AudioSender, AudioSource, audio_queue,
};

/// The tone's pitch, in hertz: concert A, which divides
/// [`AUDIO_RATE`] into no whole number of samples, so
/// a frame's samples do not repeat from one frame to the next.
pub const TONE_FREQUENCY: u64 = 440;

/// The tone's peak: a quarter of full scale. Every frame of the tone
/// holds samples near it, far above any threshold for digital silence,
/// and it leaves headroom for a client's own mixing.
pub const TONE_AMPLITUDE: i16 = 8192;

/// How long `--platform synthetic`'s tone sounds before each silence.
pub const DEFAULT_TONE_ON: Duration = Duration::from_secs(2);

/// How long each silence between the tones lasts: long enough that a
/// server stopping the stream on silence stops it, so a client sees the
/// stream start and stop over and over.
pub const DEFAULT_TONE_OFF: Duration = Duration::from_secs(2);

/// When a [`ToneSource`] sounds: for `on`, then digital silence for `off`,
/// over and over, starting with the sound. Each is rounded down to whole
/// 10 ms frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TonePattern {
    pub on: Duration,
    pub off: Duration,
}

impl Default for TonePattern {
    /// [`DEFAULT_TONE_ON`] of sound, then [`DEFAULT_TONE_OFF`] of silence.
    fn default() -> Self {
        Self {
            on: DEFAULT_TONE_ON,
            off: DEFAULT_TONE_OFF,
        }
    }
}

/// A synthetic [`AudioSource`]: a [`TONE_FREQUENCY`] sine at
/// [`TONE_AMPLITUDE`], the same in both channels, sounding and falling
/// silent as its [`TonePattern`] says. Each [`AudioSource::open`] starts
/// the pattern afresh, with the sound, and delivers a frame every 10 ms
/// of the async runtime's clock, stamped with the tick it was made at.
/// Under tokio's paused clock, a test can step through it without
/// waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToneSource {
    /// Frames of sound, then of silence, in each cycle.
    on_frames: u64,
    off_frames: u64,
}

impl ToneSource {
    /// A tone that sounds and falls silent as `pattern` says. Fails if
    /// the pattern is shorter than one frame in all.
    pub fn new(pattern: TonePattern) -> Result<Self> {
        let tone = Self {
            on_frames: frames_in(pattern.on),
            off_frames: frames_in(pattern.off),
        };
        if tone.on_frames.saturating_add(tone.off_frames) == 0 {
            bail!("a tone pattern of {pattern:?} is shorter than one 10 ms frame");
        }
        Ok(tone)
    }

    /// Frame `index` of the pattern, counting from 0 at the open, stamped
    /// `captured_at`. The sine's phase runs on across frames, and through
    /// silences, so each burst of sound is continuous.
    pub fn frame(&self, index: u64, captured_at: Instant) -> AudioFrame {
        let cycle = self.on_frames.saturating_add(self.off_frames);
        if index % cycle >= self.on_frames {
            return AudioFrame::silent(captured_at);
        }
        let rate = u64::from(AUDIO_RATE);
        let first = index.wrapping_mul(AUDIO_FRAME_SAMPLES as u64);
        let mut samples = [0; AUDIO_FRAME_VALUES];
        let (pairs, _) = samples.as_chunks_mut::<AUDIO_CHANNELS>();
        for (n, pair) in (0..).zip(pairs) {
            // The phase in whole samples, reduced exactly, so the tone does
            // not drift however long it plays.
            let phase = (first.wrapping_add(n) % rate) * TONE_FREQUENCY % rate;
            let angle = std::f64::consts::TAU * phase as f64 / rate as f64;
            pair.fill((f64::from(TONE_AMPLITUDE) * angle.sin()).round() as i16);
        }
        AudioFrame::from_array(captured_at, samples)
    }

    /// Deliver frames to `sender` every 10 ms until its receiver goes.
    /// The first comes at once. A tick missed because the runtime was
    /// busy is made up at once, as a sound server catching up delivers a
    /// burst.
    async fn play(self, sender: AudioSender) {
        let mut ticks = tokio::time::interval(AUDIO_FRAME_DURATION);
        let mut index = 0;
        loop {
            let tick = ticks.tick().await;
            if !sender.send(self.frame(index, tick.into_std())) {
                return;
            }
            index += 1;
        }
    }
}

impl Default for ToneSource {
    /// The [`TonePattern::default`] tone. Built directly rather than
    /// through [`ToneSource::new`], which cannot refuse the default
    /// pattern; `the_default_tone_is_the_default_pattern` proves it.
    fn default() -> Self {
        Self {
            on_frames: frames_in(DEFAULT_TONE_ON),
            off_frames: frames_in(DEFAULT_TONE_OFF),
        }
    }
}

/// How many whole 10 ms frames fit in `period`.
fn frames_in(period: Duration) -> u64 {
    u64::try_from(period.as_nanos() / AUDIO_FRAME_DURATION.as_nanos()).unwrap_or(u64::MAX)
}

impl AudioSource for ToneSource {
    /// Starts a task on the current async runtime, so it must be called
    /// from within one; it fails otherwise.
    fn open(&self) -> Result<AudioReceiver> {
        let runtime = tokio::runtime::Handle::try_current()
            .context("the synthetic tone plays on the async runtime, and there is none")?;
        let (sender, receiver) = audio_queue();
        runtime.spawn(self.play(sender));
        Ok(receiver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone of `on_ms` sound and `off_ms` silence.
    fn tone(on_ms: u64, off_ms: u64) -> ToneSource {
        ToneSource::new(TonePattern {
            on: Duration::from_millis(on_ms),
            off: Duration::from_millis(off_ms),
        })
        .unwrap()
    }

    #[test]
    fn the_tone_sounds_then_falls_silent_in_turn() {
        let tone = tone(30, 20);
        let now = Instant::now();
        let peaks: Vec<u16> = (0..10).map(|n| tone.frame(n, now).peak()).collect();
        for (n, peak) in peaks.iter().enumerate() {
            if n % 5 < 3 {
                // Far above any silence threshold in every frame, since a
                // frame holds more than four of the sine's periods.
                assert!(
                    (8000..=8192).contains(peak),
                    "frame {n} should sound: {peaks:?}"
                );
            } else {
                assert_eq!(*peak, 0, "frame {n} should be silent: {peaks:?}");
            }
        }
        assert_eq!(tone.frame(3, now).samples(), &[0; AUDIO_FRAME_VALUES]);
    }

    #[test]
    fn the_tone_is_440_hz_and_the_same_in_both_channels() {
        let tone = tone(10_000, 0);
        let now = Instant::now();
        // One second: 100 frames.
        let mut left = Vec::new();
        for n in 0..100 {
            let frame = tone.frame(n, now);
            for pair in frame.samples().as_chunks::<2>().0 {
                assert_eq!(pair[0], pair[1]);
                left.push(pair[0]);
            }
        }
        let rising = left
            .windows(2)
            .filter(|pair| pair[0] < 0 && pair[1] >= 0)
            .count();
        assert!((439..=441).contains(&rising), "{rising} cycles in a second");
    }

    #[test]
    fn the_tone_runs_on_across_frames() {
        let tone = tone(10_000, 0);
        let now = Instant::now();
        // The largest step between samples of the sine: its slope at a
        // zero crossing, 2 pi f A / rate, about 472.
        let step = 480;
        for n in 0..20 {
            let this = tone.frame(n, now);
            let next = tone.frame(n + 1, now);
            let last = this.samples()[AUDIO_FRAME_VALUES - 1];
            let first = next.samples()[0];
            assert!(
                (i32::from(first) - i32::from(last)).abs() <= step,
                "a jump from {last} to {first} between frames {n} and {}",
                n + 1
            );
        }
    }

    #[test]
    fn tone_patterns_must_hold_a_frame() {
        assert!(
            ToneSource::new(TonePattern {
                on: Duration::ZERO,
                off: Duration::from_millis(9),
            })
            .is_err()
        );
        // All silence, or all sound, is allowed.
        let silent = tone(0, 10);
        assert_eq!(silent.frame(0, Instant::now()).peak(), 0);
        let constant = tone(10, 0);
        assert!(constant.frame(12_345, Instant::now()).peak() > 8000);
    }

    #[test]
    fn the_default_tone_is_the_default_pattern() {
        assert_eq!(
            ToneSource::default(),
            ToneSource::new(TonePattern::default()).unwrap()
        );
        let tone = ToneSource::default();
        let now = Instant::now();
        assert!(tone.frame(199, now).peak() > 8000, "two seconds of sound");
        assert_eq!(tone.frame(200, now).peak(), 0, "then two of silence");
        assert_eq!(tone.frame(399, now).peak(), 0);
        assert!(tone.frame(400, now).peak() > 8000, "then sound again");
    }

    #[tokio::test(start_paused = true)]
    async fn the_tone_is_paced_at_one_frame_every_10_ms() {
        let opened = tokio::time::Instant::now();
        let mut receiver = tone(30, 20).open().unwrap();
        let mut stamps = Vec::new();
        let mut peaks = Vec::new();
        for _ in 0..10 {
            let frame = receiver.recv().await.unwrap();
            stamps.push(frame.captured_at());
            peaks.push(frame.peak());
        }
        assert_eq!(
            stamps[0],
            opened.into_std(),
            "the first frame comes at once"
        );
        for pair in stamps.windows(2) {
            assert_eq!(pair[1] - pair[0], AUDIO_FRAME_DURATION);
        }
        assert_eq!(
            tokio::time::Instant::now() - opened,
            AUDIO_FRAME_DURATION * 9,
            "frames come in real time, not as fast as they are read"
        );
        let sounding: Vec<bool> = peaks.iter().map(|&peak| peak > 0).collect();
        assert_eq!(
            sounding,
            [
                true, true, true, false, false, true, true, true, false, false
            ]
        );
        assert_eq!(receiver.dropped(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_reader_loses_the_oldest_tone_frames() {
        let mut receiver = tone(10_000, 0).open().unwrap();
        let first = receiver.recv().await.unwrap().captured_at();
        // Stall for 100 ms: ten frames, two more than the queue holds.
        // Half a frame more, so that the tenth tick is due before the
        // sleep ends rather than with it.
        tokio::time::sleep(AUDIO_FRAME_DURATION * 10 + AUDIO_FRAME_DURATION / 2).await;
        let next = receiver.recv().await.unwrap().captured_at();
        assert_eq!(receiver.dropped(), 2);
        assert_eq!(next - first, AUDIO_FRAME_DURATION * 3);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_receiver_stops_the_tone() {
        let metrics = tokio::runtime::Handle::current().metrics();
        let mut receiver = ToneSource::default().open().unwrap();
        receiver.recv().await.unwrap();
        assert_eq!(metrics.num_alive_tasks(), 1);
        drop(receiver);
        tokio::time::sleep(AUDIO_FRAME_DURATION * 3).await;
        assert_eq!(metrics.num_alive_tasks(), 0, "the tone's task has ended");
    }

    #[test]
    fn opening_the_tone_outside_a_runtime_fails() {
        let message = format!("{:#}", ToneSource::default().open().unwrap_err());
        assert!(message.contains("async runtime"), "{message}");
    }
}
