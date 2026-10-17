//! Audio: the frames an [`AudioSource`](super::AudioSource) captures, and
//! the queue that carries them to whoever opened it.
//!
//! - [`AudioFrame`] is 10 ms of sound: [`AUDIO_FRAME_SAMPLES`] samples a
//!   channel, signed 16-bit, stereo interleaved, at [`AUDIO_RATE`], with
//!   the time it was captured.
//! - [`audio_queue`] makes an [`AudioSender`], for the capture side, and
//!   an [`AudioReceiver`], for the consumer. The queue holds at most
//!   [`AUDIO_QUEUE_FRAMES`] frames. When a frame arrives with the queue
//!   full, the oldest frame is dropped and counted, so a consumer that
//!   falls behind loses sound rather than falling further behind.
//! - [`MonotonicStamps`] turns a read time and a reported capture latency
//!   into a capture time that never goes backwards.
//!
//! # Why the queue drops the oldest frame
//!
//! Sound is useful only on time. A consumer that stalls for a moment and
//! then catches up should resume with what is being captured now, not
//! with what was captured while it stalled, so the producer never waits
//! and never grows the queue: the frame it pushes out is the one least
//! worth playing. Eight frames (80 ms) holds the bursts some sound
//! servers deliver in: an xrdp sink's loader forces a 2048-frame quantum,
//! so frames arrive four or five at a time every 42.7 ms.
//!
//! The producer pushes under a short lock and never blocks, so a capture
//! thread can push from outside the async runtime. The consumer awaits
//! [`AudioReceiver::recv`]. Dropping either end closes the queue: the
//! producer learns from [`AudioSender::send`] or [`AudioSender::is_closed`]
//! that nobody is listening and stops capturing, and the consumer gets the
//! frames already queued and then `None`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, ensure};
use tokio::sync::Notify;

/// The sample rate, in samples a second per channel. SPICE's Opus mode
/// runs at 48 kHz, and its raw mode is given the same rate, so captured
/// sound is never resampled.
pub const AUDIO_RATE: u32 = 48_000;

/// Channels: stereo, interleaved left then right.
pub const AUDIO_CHANNELS: usize = 2;

/// Samples a channel in one frame: 10 ms at [`AUDIO_RATE`]. SPICE sends
/// 10 ms frames in both modes, and spice-gtk decodes Opus into a buffer of
/// exactly this size, so no other frame length is possible.
pub const AUDIO_FRAME_SAMPLES: usize = 480;

/// Samples in one frame, all channels: the length of
/// [`AudioFrame::samples`].
pub const AUDIO_FRAME_VALUES: usize = AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS;

/// Bytes in one frame as S16LE, SPICE's raw format.
pub const AUDIO_FRAME_BYTES: usize = AUDIO_FRAME_VALUES * 2;

/// How long one frame lasts.
pub const AUDIO_FRAME_DURATION: Duration = Duration::from_millis(10);

/// The most frames an [`audio_queue`] holds: 80 ms. See the module
/// documentation for why eight.
pub const AUDIO_QUEUE_FRAMES: usize = 8;

/// 10 ms of captured sound: [`AUDIO_FRAME_VALUES`] signed 16-bit samples,
/// stereo interleaved, and when they were captured.
///
/// The fields are private, so a frame always holds exactly one frame's
/// samples.
#[derive(Clone, PartialEq, Eq)]
pub struct AudioFrame {
    captured_at: Instant,
    samples: Box<[i16; AUDIO_FRAME_VALUES]>,
}

impl AudioFrame {
    /// `samples`, captured at `captured_at`. Fails unless there are
    /// exactly [`AUDIO_FRAME_VALUES`] of them.
    pub fn new(captured_at: Instant, samples: Vec<i16>) -> Result<Self> {
        let samples = <Box<[i16; AUDIO_FRAME_VALUES]>>::try_from(samples.into_boxed_slice())
            .map_err(|samples| {
                anyhow!(
                    "an audio frame holds {AUDIO_FRAME_VALUES} samples, not {}",
                    samples.len()
                )
            })?;
        Ok(Self {
            captured_at,
            samples,
        })
    }

    /// `samples`, captured at `captured_at`.
    pub fn from_array(captured_at: Instant, samples: [i16; AUDIO_FRAME_VALUES]) -> Self {
        Self {
            captured_at,
            samples: Box::new(samples),
        }
    }

    /// A frame of S16LE `bytes`, captured at `captured_at`. Fails unless
    /// there are exactly [`AUDIO_FRAME_BYTES`] of them.
    pub fn from_le_bytes(captured_at: Instant, bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == AUDIO_FRAME_BYTES,
            "an audio frame is {AUDIO_FRAME_BYTES} bytes, not {}",
            bytes.len()
        );
        let (pairs, _) = bytes.as_chunks::<2>();
        let samples = pairs.iter().map(|&pair| i16::from_le_bytes(pair)).collect();
        Self::new(captured_at, samples)
    }

    /// A frame of digital silence, every sample zero.
    pub fn silent(captured_at: Instant) -> Self {
        Self {
            captured_at,
            samples: Box::new([0; AUDIO_FRAME_VALUES]),
        }
    }

    /// When the frame was captured, as nearly as its source can tell: the
    /// end of the frame, read from the sound server's clock where it
    /// reports one. Within one [`AudioReceiver`], each frame's time is at
    /// or after the one before.
    pub fn captured_at(&self) -> Instant {
        self.captured_at
    }

    /// The samples, left and right interleaved.
    pub fn samples(&self) -> &[i16] {
        &self.samples[..]
    }

    /// The samples as S16LE, [`AUDIO_FRAME_BYTES`] bytes.
    pub fn to_le_bytes(&self) -> Vec<u8> {
        self.samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// The largest magnitude of any sample: 0 for digital silence, 32768
    /// at most.
    pub fn peak(&self) -> u16 {
        self.samples
            .iter()
            .map(|s| s.unsigned_abs())
            .max()
            .unwrap_or(0)
    }
}

impl std::fmt::Debug for AudioFrame {
    // The samples would swamp any log or assertion message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioFrame")
            .field("captured_at", &self.captured_at)
            .field("peak", &self.peak())
            .finish()
    }
}

/// A new queue of at most [`AUDIO_QUEUE_FRAMES`] frames, as its sending
/// and receiving ends.
pub fn audio_queue() -> (AudioSender, AudioReceiver) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            frames: VecDeque::with_capacity(AUDIO_QUEUE_FRAMES),
            dropped: 0,
            sender_gone: false,
            receiver_gone: false,
        }),
        ready: Notify::new(),
    });
    (AudioSender(shared.clone()), AudioReceiver(shared))
}

/// What both ends of a queue share.
#[derive(Debug)]
struct Shared {
    queue: Mutex<Queue>,
    /// Woken when a frame is queued or the sender goes.
    ready: Notify,
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        // Every update is a few field writes that cannot panic part way,
        // so a poisoned lock still guards a consistent queue.
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug)]
struct Queue {
    frames: VecDeque<AudioFrame>,
    /// Frames dropped because the queue was full, since it was made.
    dropped: u64,
    sender_gone: bool,
    receiver_gone: bool,
}

/// The capture side of an [`audio_queue`]. Dropping it ends the stream:
/// the receiver gets what is queued, then `None`.
#[derive(Debug)]
pub struct AudioSender(Arc<Shared>);

impl AudioSender {
    /// Queue `frame`, dropping the oldest frame queued if the queue is
    /// full. Never blocks. Returns false, and queues nothing, once the
    /// receiver has gone: capture should stop.
    pub fn send(&self, frame: AudioFrame) -> bool {
        {
            let mut queue = self.0.queue();
            if queue.receiver_gone {
                return false;
            }
            if queue.frames.len() >= AUDIO_QUEUE_FRAMES {
                queue.frames.pop_front();
                queue.dropped += 1;
            }
            queue.frames.push_back(frame);
        }
        self.0.ready.notify_one();
        true
    }

    /// Whether the receiver has gone, so that nothing more will be read.
    pub fn is_closed(&self) -> bool {
        self.0.queue().receiver_gone
    }
}

impl Drop for AudioSender {
    fn drop(&mut self) {
        self.0.queue().sender_gone = true;
        self.0.ready.notify_one();
    }
}

/// The consuming side of an [`audio_queue`]. Dropping it tells the
/// capture side to stop.
#[derive(Debug)]
pub struct AudioReceiver(Arc<Shared>);

impl AudioReceiver {
    /// The oldest frame queued, waiting for one if there is none. Returns
    /// `None` once the sender has gone and every frame it queued has been
    /// taken: capture has stopped, for good.
    ///
    /// Cancel safe: a frame is taken only when this returns it, so
    /// dropping the future (in a `select!`, say) loses nothing.
    pub async fn recv(&mut self) -> Option<AudioFrame> {
        loop {
            {
                let mut queue = self.0.queue();
                if let Some(frame) = queue.frames.pop_front() {
                    return Some(frame);
                }
                if queue.sender_gone {
                    return None;
                }
            }
            // A frame queued between the check above and this wait leaves
            // a permit behind, so the wait returns at once.
            self.0.ready.notified().await;
        }
    }

    /// How many frames the queue has dropped because it was full, since
    /// it was opened.
    pub fn dropped(&self) -> u64 {
        self.0.queue().dropped
    }
}

impl Drop for AudioReceiver {
    fn drop(&mut self) {
        let mut queue = self.0.queue();
        queue.receiver_gone = true;
        // Nothing will take them now.
        queue.frames.clear();
    }
}

/// Capture times that never go backwards, for a source that knows when it
/// read each frame and how long the sound waited before it was read.
///
/// A sound server's reported latency can rise faster than time passes: a
/// PulseAudio null sink waking from suspension has reported about 0.7 s
/// for a while. Read time less latency would then go backwards, which a
/// client's clock must not, so each time is held at the one before until
/// the read time less latency passes it again.
#[derive(Debug, Default)]
pub struct MonotonicStamps {
    last: Option<Instant>,
}

impl MonotonicStamps {
    /// The capture time of a frame read at `read_at`, which waited
    /// `latency` before it was read: `read_at - latency`, but never before
    /// the time this last returned, nor after `read_at`.
    pub fn stamp(&mut self, read_at: Instant, latency: Duration) -> Instant {
        // A latency too large for the clock to subtract is nonsense, and
        // the read time is the best estimate left.
        let captured = read_at.checked_sub(latency).unwrap_or(read_at);
        let captured = match self.last {
            Some(last) if last > captured => last,
            _ => captured,
        };
        self.last = Some(captured);
        captured
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame whose samples are all `value`, captured at `at`.
    fn frame(at: Instant, value: i16) -> AudioFrame {
        AudioFrame::new(at, vec![value; AUDIO_FRAME_VALUES]).unwrap()
    }

    #[test]
    fn sizes_are_ten_ms_of_s16_stereo_at_48k() {
        assert_eq!(AUDIO_FRAME_VALUES, 960);
        assert_eq!(AUDIO_FRAME_BYTES, 1920);
        assert_eq!(
            AUDIO_FRAME_SAMPLES as u128 * 1000 / u128::from(AUDIO_RATE),
            AUDIO_FRAME_DURATION.as_millis()
        );
    }

    #[test]
    fn a_frame_holds_exactly_one_frame_of_samples() {
        let now = Instant::now();
        assert!(AudioFrame::new(now, vec![0; AUDIO_FRAME_VALUES]).is_ok());
        let message = format!(
            "{:#}",
            AudioFrame::new(now, vec![0; AUDIO_FRAME_VALUES - 1]).unwrap_err()
        );
        assert!(message.contains("959"), "{message}");
        assert!(AudioFrame::new(now, vec![0; AUDIO_FRAME_VALUES + 1]).is_err());
        assert!(AudioFrame::from_le_bytes(now, &[0; AUDIO_FRAME_BYTES - 2]).is_err());
        assert!(AudioFrame::from_le_bytes(now, &[0; AUDIO_FRAME_BYTES + 1]).is_err());
    }

    #[test]
    fn bytes_are_little_endian_and_round_trip() {
        let now = Instant::now();
        let mut bytes = vec![0; AUDIO_FRAME_BYTES];
        bytes[0..4].copy_from_slice(&[0x34, 0x12, 0xff, 0xff]);
        let decoded = AudioFrame::from_le_bytes(now, &bytes).unwrap();
        assert_eq!(&decoded.samples()[..3], &[0x1234, -1, 0]);
        assert_eq!(decoded.to_le_bytes(), bytes);
        assert_eq!(decoded.captured_at(), now);
    }

    #[test]
    fn peak_is_the_largest_magnitude() {
        let now = Instant::now();
        assert_eq!(AudioFrame::silent(now).peak(), 0);
        let mut samples = vec![3; AUDIO_FRAME_VALUES];
        samples[7] = -9;
        assert_eq!(AudioFrame::new(now, samples.clone()).unwrap().peak(), 9);
        samples[100] = i16::MIN;
        assert_eq!(AudioFrame::new(now, samples).unwrap().peak(), 32768);
    }

    #[tokio::test]
    async fn frames_arrive_in_order() {
        let (sender, mut receiver) = audio_queue();
        let start = Instant::now();
        for n in 0..3 {
            assert!(sender.send(frame(start, n)));
        }
        for n in 0..3 {
            assert_eq!(receiver.recv().await.unwrap().samples()[0], n);
        }
        assert_eq!(receiver.dropped(), 0);
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_oldest_and_counts_it() {
        let (sender, mut receiver) = audio_queue();
        let start = Instant::now();
        let sent = AUDIO_QUEUE_FRAMES as i16 + 2;
        for n in 0..sent {
            assert!(sender.send(frame(start, n)), "send never blocks or fails");
        }
        assert_eq!(receiver.dropped(), 2);
        drop(sender);
        let mut kept = Vec::new();
        while let Some(frame) = receiver.recv().await {
            kept.push(frame.samples()[0]);
        }
        assert_eq!(kept, (2..sent).collect::<Vec<_>>(), "the newest eight");
        assert_eq!(receiver.dropped(), 2);
    }

    #[tokio::test]
    async fn the_receiver_gets_what_is_queued_then_none() {
        let (sender, mut receiver) = audio_queue();
        assert!(sender.send(frame(Instant::now(), 5)));
        drop(sender);
        assert_eq!(receiver.recv().await.unwrap().samples()[0], 5);
        assert!(receiver.recv().await.is_none());
        assert!(receiver.recv().await.is_none(), "and stays closed");
    }

    #[test]
    fn dropping_the_receiver_tells_the_sender_to_stop() {
        let (sender, receiver) = audio_queue();
        assert!(!sender.is_closed());
        drop(receiver);
        assert!(sender.is_closed());
        assert!(!sender.send(frame(Instant::now(), 1)));
    }

    #[tokio::test]
    async fn recv_waits_for_a_frame_from_another_thread() {
        let (sender, mut receiver) = audio_queue();
        let pusher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            assert!(sender.send(frame(Instant::now(), 7)));
            sender
        });
        let got = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("the frame never woke the receiver");
        assert_eq!(got.unwrap().samples()[0], 7);
        let sender = pusher.join().unwrap();
        drop(sender);
        let ended = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("the sender's drop never woke the receiver");
        assert!(ended.is_none());
    }

    #[tokio::test]
    async fn a_cancelled_recv_loses_nothing() {
        let (sender, mut receiver) = audio_queue();
        // Cancel a wait on an empty queue, as a select! that took another
        // branch would.
        let waited = tokio::time::timeout(Duration::from_millis(20), receiver.recv()).await;
        assert!(waited.is_err());
        assert!(sender.send(frame(Instant::now(), 3)));
        let got = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap();
        assert_eq!(got.unwrap().samples()[0], 3);
    }

    #[test]
    fn stamps_subtract_the_latency() {
        let mut stamps = MonotonicStamps::default();
        let read = Instant::now() + Duration::from_secs(10);
        assert_eq!(
            stamps.stamp(read, Duration::from_millis(5)),
            read - Duration::from_millis(5)
        );
        let next = read + Duration::from_millis(10);
        assert_eq!(
            stamps.stamp(next, Duration::from_millis(5)),
            next - Duration::from_millis(5)
        );
    }

    #[test]
    fn stamps_never_go_backwards_when_the_latency_jumps() {
        let mut stamps = MonotonicStamps::default();
        let start = Instant::now() + Duration::from_secs(10);
        let mut last = stamps.stamp(start, Duration::from_millis(5));
        // A null sink waking: the reported latency leaps to 0.7 s, then
        // falls back over the following reads.
        let latencies = [700, 700, 650, 400, 100, 20, 5, 5];
        let mut held = 0;
        for (n, latency) in latencies.into_iter().enumerate() {
            let read = start + AUDIO_FRAME_DURATION * (n as u32 + 1);
            let stamp = stamps.stamp(read, Duration::from_millis(latency));
            assert!(stamp >= last, "read {n} went backwards");
            assert!(stamp <= read, "read {n} is after its read time");
            if stamp == last {
                held += 1;
            }
            last = stamp;
        }
        assert!(held >= 2, "the jump should hold the stamp, held {held}");
        let final_read = start + AUDIO_FRAME_DURATION * latencies.len() as u32;
        assert_eq!(
            last,
            final_read - Duration::from_millis(5),
            "once the latency settles, stamps follow it again"
        );
    }

    #[test]
    fn an_impossible_latency_stamps_the_read_time() {
        let mut stamps = MonotonicStamps::default();
        let read = Instant::now();
        assert_eq!(stamps.stamp(read, Duration::MAX), read);
    }
}
