//! Per-session metrics: what a session has sent, how long it took, and
//! how the client is keeping up.
//!
//! A [`Stats`] belongs to a [`Session`](super::session::Session) and is
//! updated by each of its channels through relaxed atomics, so counting
//! costs no lock and no allocation. [`Stats::snapshot`] reads it as a
//! [`StatsSnapshot`], a plain `Clone` struct that tests and the log
//! summary use.
//!
//! The main channel logs a one-line summary at info every
//! [`STATS_INTERVAL`] while frames flow, from the difference between two
//! snapshots ([`StatsSnapshot::since`]), and one more for the whole
//! session when it ends. Nothing is logged for an interval in which no
//! frame was sent ([`StatsSnapshot::has_frames`]).
//!
//! Video streams have counters of their own, not a slot among the draw
//! encodings: a stream frame is not a `DRAW_COPY`, has no image type,
//! and is not refreshed losslessly, and what matters about it (how many
//! streams came and went, what bitrate they aimed for) has no meaning
//! for a draw. The draw counters stay exactly the draws.
//!
//! The playback channel has counters of its own too: audio frames sent,
//! how long Opus took to encode them, how many capture dropped because
//! the channel fell behind, and how often the stream started and
//! stopped. Its bytes are the playback channel's, in the per-channel
//! counters. An interval in which only sound was sent is logged like one
//! in which frames were.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use shakenfist_spice_protocol::ChannelType;

use super::updates::Encoding;

/// How often the main channel logs the summary, while frames flow.
pub const STATS_INTERVAL: Duration = Duration::from_secs(10);

const RELAXED: Ordering = Ordering::Relaxed;

/// How many per-encoding counters there are: one for each [`Encoding`].
///
/// [`encoding_slot`] matches every encoding without a wildcard, so a new
/// [`Encoding`] does not compile until it has a slot, and
/// [`Stats::snapshot`] destructures the counters by this length, so
/// giving it one means naming it there too.
const ENCODINGS: usize = 3;

/// How many per-channel byte counters there are: one for each channel
/// type andris serves.
const CHANNELS: usize = 5;

/// The slot of `encoding` in the per-encoding counters, below
/// [`ENCODINGS`].
const fn encoding_slot(encoding: Encoding) -> usize {
    match encoding {
        Encoding::Lz4 => 0,
        Encoding::Bitmap => 1,
        Encoding::Jpeg => 2,
    }
}

/// The slot of `channel` in the per-channel byte counters, below
/// [`CHANNELS`], if andris serves it.
const fn channel_slot(channel: ChannelType) -> Option<usize> {
    match channel {
        ChannelType::Main => Some(0),
        ChannelType::Display => Some(1),
        ChannelType::Inputs => Some(2),
        ChannelType::Cursor => Some(3),
        ChannelType::Playback => Some(4),
        _ => None,
    }
}

/// Every counter of `counters`, read at once.
fn load<const N: usize>(counters: &[AtomicU64; N]) -> [u64; N] {
    counters.each_ref().map(|counter| counter.load(RELAXED))
}

/// Add `value` to `counter`, stopping at `u64::MAX` rather than wrapping.
fn saturating_add(counter: &AtomicU64, value: u64) {
    let mut total = counter.load(RELAXED);
    loop {
        let sum = total.saturating_add(value);
        match counter.compare_exchange_weak(total, sum, RELAXED, RELAXED) {
            Ok(_) => return,
            Err(now) => total = now,
        }
    }
}

/// A duration in whole microseconds, saturating.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// `duration` in milliseconds to one decimal place, or `-` for `None`.
fn ms(duration: Option<Duration>) -> String {
    duration.map_or_else(
        || '-'.to_string(),
        |d| format!("{:.1}", d.as_secs_f64() * 1e3),
    )
}

/// `duration` in milliseconds to two decimal places, or `-` for `None`:
/// for times well under a millisecond, such as an Opus frame's encode.
fn fine_ms(duration: Option<Duration>) -> String {
    duration.map_or_else(
        || '-'.to_string(),
        |d| format!("{:.2}", d.as_secs_f64() * 1e3),
    )
}

/// A total and a count of timed events, with the latest.
#[derive(Debug, Default)]
struct Timing {
    total_us: AtomicU64,
    count: AtomicU64,
    last_us: AtomicU64,
}

impl Timing {
    /// Record one event that took `duration`. The total and the count
    /// stop at `u64::MAX` rather than wrapping.
    fn record(&self, duration: Duration) {
        let us = micros(duration);
        saturating_add(&self.total_us, us);
        saturating_add(&self.count, 1);
        self.last_us.store(us, RELAXED);
    }

    fn read(&self) -> TimingSnapshot {
        TimingSnapshot {
            total: Duration::from_micros(self.total_us.load(RELAXED)),
            count: self.count.load(RELAXED),
            last: Duration::from_micros(self.last_us.load(RELAXED)),
        }
    }
}

/// A [`Timing`] at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimingSnapshot {
    /// The sum of every duration recorded.
    pub total: Duration,
    /// How many were recorded.
    pub count: u64,
    /// The latest, zero if none has been.
    pub last: Duration,
}

impl TimingSnapshot {
    /// The mean duration, if any has been recorded. Any count works,
    /// however large.
    pub fn mean(&self) -> Option<Duration> {
        if self.count == 0 {
            return None;
        }
        let nanos = self.total.as_nanos() / u128::from(self.count);
        // The mean is at most the total, so its seconds fit a u64.
        let secs = u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX);
        Some(Duration::new(secs, (nanos % 1_000_000_000) as u32))
    }

    /// The mean in milliseconds to one decimal place, or `-` if nothing
    /// has been recorded.
    fn mean_ms(&self) -> String {
        ms(self.mean())
    }

    /// The latest duration, if any has been recorded.
    pub fn latest(&self) -> Option<Duration> {
        (self.count > 0).then_some(self.last)
    }

    /// What was recorded since `prev`. The latest is unchanged.
    fn since(&self, prev: &Self) -> Self {
        Self {
            total: self.total.saturating_sub(prev.total),
            count: self.count.saturating_sub(prev.count),
            last: self.last,
        }
    }
}

/// The metrics of one session, updated by its channels.
#[derive(Debug, Default)]
pub struct Stats {
    frames: AtomicU64,
    refreshes: AtomicU64,
    draws: [AtomicU64; ENCODINGS],
    draw_bytes: [AtomicU64; ENCODINGS],
    channel_bytes: [AtomicU64; CHANNELS],
    capture: Timing,
    encode: Timing,
    captured_area: AtomicU64,
    changed_area: AtomicU64,
    scrub_tiles: AtomicU64,
    ack_lag: Timing,
    ping: Timing,
    streams_created: AtomicU64,
    streams_destroyed: AtomicU64,
    stream_frames: AtomicU64,
    stream_bytes: AtomicU64,
    stream_encode: Timing,
    stream_server_drops: AtomicU64,
    /// A gauge, not a counter: the live stream's target, or 0.
    stream_target_bps: AtomicU64,
    audio_frames: AtomicU64,
    audio_encode: Timing,
    audio_drops: AtomicU64,
    audio_starts: AtomicU64,
    audio_stops: AtomicU64,
}

impl Stats {
    /// Count a frame for pending damage, or if `refresh`, a lossless
    /// refresh, sent up to and including its `MARK`.
    pub fn frame_sent(&self, refresh: bool) {
        let counter = if refresh {
            &self.refreshes
        } else {
            &self.frames
        };
        counter.fetch_add(1, RELAXED);
    }

    /// Count one `DRAW_COPY` of `encoding` whose body was `bytes` long.
    pub fn draw_sent(&self, encoding: Encoding, bytes: u64) {
        let slot = encoding_slot(encoding);
        self.draws[slot].fetch_add(1, RELAXED);
        self.draw_bytes[slot].fetch_add(bytes, RELAXED);
    }

    /// Count `bytes` written to a `channel` channel. Types andris does
    /// not serve are not counted.
    pub fn bytes_sent(&self, channel: ChannelType, bytes: u64) {
        if let Some(slot) = channel_slot(channel) {
            self.channel_bytes[slot].fetch_add(bytes, RELAXED);
        }
    }

    /// Record the time one capture took.
    pub fn capture_done(&self, took: Duration) {
        self.capture.record(took);
    }

    /// Record the time one pass of the update pipeline (compare, merge
    /// and encode) took.
    pub fn encode_done(&self, took: Duration) {
        self.encode.record(took);
    }

    /// Record the area an update captured and the area of it that
    /// changed, in pixels.
    pub fn area_updated(&self, captured: u64, changed: u64) {
        self.captured_area.fetch_add(captured, RELAXED);
        self.changed_area.fetch_add(changed, RELAXED);
    }

    /// Count `tiles` the scrub found changed that damage had not
    /// reported, and return how many it has found in all.
    pub fn scrub_found(&self, tiles: u64) -> u64 {
        self.scrub_tiles.fetch_add(tiles, RELAXED) + tiles
    }

    /// Record the time from sending the message that completed an ack
    /// window to the `ACK` for it.
    pub fn ack_lag(&self, lag: Duration) {
        self.ack_lag.record(lag);
    }

    /// Record a `PING` round trip.
    pub fn ping_round_trip(&self, rtt: Duration) {
        self.ping.record(rtt);
    }

    /// Count a video stream started, aiming for `target_bps`.
    pub fn stream_created(&self, target_bps: u32) {
        self.streams_created.fetch_add(1, RELAXED);
        self.stream_target(target_bps);
    }

    /// Count a video stream ended. No stream is live afterwards.
    pub fn stream_destroyed(&self) {
        self.streams_destroyed.fetch_add(1, RELAXED);
        self.stream_target(0);
    }

    /// Count one `STREAM_DATA` whose frame was `bytes` long.
    pub fn stream_frame_sent(&self, bytes: u64) {
        self.stream_frames.fetch_add(1, RELAXED);
        self.stream_bytes.fetch_add(bytes, RELAXED);
    }

    /// Record the time capturing a stream's frame into the shadow and
    /// encoding it took.
    pub fn stream_encode_done(&self, took: Duration) {
        self.stream_encode.record(took);
    }

    /// Count a stream frame not captured because the ack window was full.
    pub fn stream_server_drop(&self) {
        self.stream_server_drops.fetch_add(1, RELAXED);
    }

    /// Record the live stream's target bitrate, or 0 for none.
    pub fn stream_target(&self, bps: u32) {
        self.stream_target_bps.store(u64::from(bps), RELAXED);
    }

    /// Count one playback `DATA` sent.
    pub fn audio_frame_sent(&self) {
        self.audio_frames.fetch_add(1, RELAXED);
    }

    /// Record the time encoding one audio frame as Opus took.
    pub fn audio_encode_done(&self, took: Duration) {
        self.audio_encode.record(took);
    }

    /// Count `frames` of sound that capture dropped because the playback
    /// channel had not taken them in time.
    pub fn audio_dropped(&self, frames: u64) {
        saturating_add(&self.audio_drops, frames);
    }

    /// Count a playback `START`.
    pub fn audio_started(&self) {
        self.audio_starts.fetch_add(1, RELAXED);
    }

    /// Count a playback `STOP`.
    pub fn audio_stopped(&self) {
        self.audio_stops.fetch_add(1, RELAXED);
    }

    /// Read every metric. The reads are independent, so a snapshot taken
    /// while channels run may be a message or two out of step.
    pub fn snapshot(&self) -> StatsSnapshot {
        // In slot order: see encoding_slot and channel_slot.
        let [lz4_draws, bitmap_draws, jpeg_draws] = load(&self.draws);
        let [lz4_bytes, bitmap_bytes, jpeg_bytes] = load(&self.draw_bytes);
        let [
            main_bytes,
            display_bytes,
            inputs_bytes,
            cursor_bytes,
            playback_bytes,
        ] = load(&self.channel_bytes);
        StatsSnapshot {
            frames: self.frames.load(RELAXED),
            refreshes: self.refreshes.load(RELAXED),
            lz4_draws,
            bitmap_draws,
            jpeg_draws,
            lz4_bytes,
            bitmap_bytes,
            jpeg_bytes,
            main_bytes,
            display_bytes,
            inputs_bytes,
            cursor_bytes,
            playback_bytes,
            capture: self.capture.read(),
            encode: self.encode.read(),
            captured_area: self.captured_area.load(RELAXED),
            changed_area: self.changed_area.load(RELAXED),
            scrub_tiles: self.scrub_tiles.load(RELAXED),
            ack_lag: self.ack_lag.read(),
            ping: self.ping.read(),
            streams_created: self.streams_created.load(RELAXED),
            streams_destroyed: self.streams_destroyed.load(RELAXED),
            stream_frames: self.stream_frames.load(RELAXED),
            stream_bytes: self.stream_bytes.load(RELAXED),
            stream_encode: self.stream_encode.read(),
            stream_server_drops: self.stream_server_drops.load(RELAXED),
            stream_target_bps: self.stream_target_bps.load(RELAXED),
            audio_frames: self.audio_frames.load(RELAXED),
            audio_encode: self.audio_encode.read(),
            audio_drops: self.audio_drops.load(RELAXED),
            audio_starts: self.audio_starts.load(RELAXED),
            audio_stops: self.audio_stops.load(RELAXED),
        }
    }
}

/// A session's metrics at one moment, as plain numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Frames sent for damage: draws followed by a `MARK`, and the
    /// first frame of a surface.
    pub frames: u64,
    /// Lossless refreshes of JPEG areas sent, each with its `MARK`.
    pub refreshes: u64,
    /// `DRAW_COPY`s sent as LZ4.
    pub lz4_draws: u64,
    /// `DRAW_COPY`s sent as uncompressed bitmaps.
    pub bitmap_draws: u64,
    /// `DRAW_COPY`s sent as JPEG.
    pub jpeg_draws: u64,
    /// The LZ4 draws' message bodies, in bytes.
    pub lz4_bytes: u64,
    /// The bitmap draws' message bodies, in bytes.
    pub bitmap_bytes: u64,
    /// The JPEG draws' message bodies, in bytes.
    pub jpeg_bytes: u64,
    /// Bytes written on the main channel, headers included.
    pub main_bytes: u64,
    /// Bytes written on the display channel, headers included.
    pub display_bytes: u64,
    /// Bytes written on the inputs channel, headers included.
    pub inputs_bytes: u64,
    /// Bytes written on the cursor channel, headers included.
    pub cursor_bytes: u64,
    /// Bytes written on the playback channel, headers included.
    pub playback_bytes: u64,
    /// Desktop captures, the scrub's included.
    pub capture: TimingSnapshot,
    /// Passes of the update pipeline: comparing, merging and encoding.
    pub encode: TimingSnapshot,
    /// Pixels captured for updates, counting twice any captured twice.
    pub captured_area: u64,
    /// Pixels of the tiles that turned out to have changed.
    pub changed_area: u64,
    /// Tiles the scrub found changed that damage had not reported.
    pub scrub_tiles: u64,
    /// From sending the message that completes an ack window to its
    /// `ACK`.
    pub ack_lag: TimingSnapshot,
    /// `PING` round trips.
    pub ping: TimingSnapshot,
    /// Video streams started.
    pub streams_created: u64,
    /// Video streams ended, every way they end.
    pub streams_destroyed: u64,
    /// `STREAM_DATA`s sent.
    pub stream_frames: u64,
    /// Their frames, in bytes.
    pub stream_bytes: u64,
    /// Bringing a stream's capture into the shadow and encoding it.
    pub stream_encode: TimingSnapshot,
    /// Stream frames not captured because the ack window was full.
    pub stream_server_drops: u64,
    /// The live stream's target bitrate, in bits a second, or 0 if no
    /// stream is live. A gauge: [`StatsSnapshot::since`] keeps it.
    pub stream_target_bps: u64,
    /// Playback `DATA`s sent: 10 ms frames of sound.
    pub audio_frames: u64,
    /// Encoding an audio frame as Opus; nothing in raw mode.
    pub audio_encode: TimingSnapshot,
    /// Audio frames capture dropped because the playback channel fell
    /// behind.
    pub audio_drops: u64,
    /// Playback `START`s sent.
    pub audio_starts: u64,
    /// Playback `STOP`s sent.
    pub audio_stops: u64,
}

impl StatsSnapshot {
    /// Draws sent, of every encoding.
    pub fn draws(&self) -> u64 {
        self.lz4_draws + self.bitmap_draws + self.jpeg_draws
    }

    /// Bytes written on every channel.
    pub fn total_bytes(&self) -> u64 {
        self.main_bytes
            + self.display_bytes
            + self.inputs_bytes
            + self.cursor_bytes
            + self.playback_bytes
    }

    /// Whether any frame, refresh, stream frame or audio frame was sent:
    /// what the periodic log line waits for.
    pub fn has_frames(&self) -> bool {
        self.frames + self.refreshes + self.stream_frames + self.audio_frames > 0
    }

    /// What happened since `prev`, an earlier snapshot of the same
    /// session. Counters and totals are differences; the latest ack lag
    /// and ping, and the stream's target, are this snapshot's.
    pub fn since(&self, prev: &Self) -> Self {
        // Every field is named once: a field added to the struct and not
        // here is a missing field in the struct expression below.
        macro_rules! since {
            (
                counters: $($counter:ident),*;
                timings: $($timing:ident),*;
                gauges: $($gauge:ident),*
            ) => {
                Self {
                    $($counter: self.$counter.saturating_sub(prev.$counter),)*
                    $($timing: self.$timing.since(&prev.$timing),)*
                    $($gauge: self.$gauge,)*
                }
            };
        }
        since!(
            counters: frames, refreshes, lz4_draws, bitmap_draws, jpeg_draws, lz4_bytes,
                bitmap_bytes, jpeg_bytes, main_bytes, display_bytes, inputs_bytes, cursor_bytes,
                playback_bytes, captured_area, changed_area, scrub_tiles, streams_created,
                streams_destroyed, stream_frames, stream_bytes, stream_server_drops, audio_frames,
                audio_drops, audio_starts, audio_stops;
            timings: capture, encode, ack_lag, ping, stream_encode, audio_encode;
            gauges: stream_target_bps
        )
    }

    /// The one-line summary of this snapshot (normally a
    /// [`StatsSnapshot::since`] difference) over `elapsed`.
    pub fn summary(&self, elapsed: Duration) -> String {
        let secs = elapsed.as_secs_f64().max(f64::EPSILON);
        let changed = if self.captured_area > 0 {
            format!(
                "{:.1}%",
                self.changed_area as f64 * 100.0 / self.captured_area as f64
            )
        } else {
            '-'.to_string()
        };
        let target = if self.stream_target_bps > 0 {
            format!("{} kbit/s", self.stream_target_bps / 1000)
        } else {
            '-'.to_string()
        };
        format!(
            "{:.1} fps ({} refreshes), draws lz4={} bitmap={} jpeg={}, display {:.0} kbit/s, \
             capture {} ms, encode {} ms, ack lag {} ms, ping {} ms, scrub tiles {}, \
             changed {} of captured, streams created={} destroyed={}, stream frames {} at \
             {:.0} kbit/s, stream encode {} ms, server drops {}, stream target {}, audio frames {} \
             at {:.0} kbit/s, opus encode {} ms, audio drops {}, audio starts={} stops={}",
            (self.frames + self.refreshes) as f64 / secs,
            self.refreshes,
            self.lz4_draws,
            self.bitmap_draws,
            self.jpeg_draws,
            self.display_bytes as f64 * 8.0 / 1000.0 / secs,
            self.capture.mean_ms(),
            self.encode.mean_ms(),
            self.ack_lag.mean_ms(),
            ms(self.ping.latest()),
            self.scrub_tiles,
            changed,
            self.streams_created,
            self.streams_destroyed,
            self.stream_frames,
            self.stream_bytes as f64 * 8.0 / 1000.0 / secs,
            self.stream_encode.mean_ms(),
            self.stream_server_drops,
            target,
            self.audio_frames,
            self.playback_bytes as f64 * 8.0 / 1000.0 / secs,
            fine_ms(self.audio_encode.mean()),
            self.audio_drops,
            self.audio_starts,
            self.audio_stops,
        )
    }
}

/// Decides when the periodic summary is due, and what it says. It holds
/// the snapshot the last line was computed from.
#[derive(Debug, Clone, Default)]
pub struct Reporter {
    prev: StatsSnapshot,
}

impl Reporter {
    /// A reporter whose first interval starts from `start`.
    pub fn new(start: StatsSnapshot) -> Self {
        Self { prev: start }
    }

    /// The summary line for the `elapsed` since the last call, or `None`
    /// if no frame was sent in it. Either way the next interval starts
    /// from `now`.
    pub fn interval_line(&mut self, now: StatsSnapshot, elapsed: Duration) -> Option<String> {
        let delta = now.since(&self.prev);
        self.prev = now;
        delta.has_frames().then(|| delta.summary(elapsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy() -> StatsSnapshot {
        let stats = Stats::default();
        stats.frame_sent(false);
        stats.draw_sent(Encoding::Lz4, 1000);
        stats.bytes_sent(ChannelType::Display, 12_500);
        stats.capture_done(Duration::from_millis(2));
        stats.encode_done(Duration::from_millis(4));
        stats.area_updated(1000, 250);
        stats.ack_lag(Duration::from_millis(6));
        stats.ping_round_trip(Duration::from_micros(500));
        stats.snapshot()
    }

    /// A session with one live stream that sent two frames.
    fn streaming() -> StatsSnapshot {
        let stats = Stats::default();
        stats.stream_created(2_000_000);
        stats.stream_frame_sent(10_000);
        stats.stream_frame_sent(15_000);
        stats.stream_encode_done(Duration::from_millis(2));
        stats.stream_encode_done(Duration::from_millis(4));
        stats.stream_server_drop();
        stats.stream_target(1_500_000);
        stats.snapshot()
    }

    #[test]
    fn stream_counters_count_and_the_target_is_a_gauge() {
        let stats = Stats::default();
        stats.stream_created(2_000_000);
        stats.stream_frame_sent(100);
        let first = stats.snapshot();
        assert_eq!(
            (first.streams_created, first.stream_target_bps),
            (1, 2_000_000)
        );
        assert!(first.has_frames(), "stream frames alone count as frames");
        stats.stream_frame_sent(50);
        stats.stream_target(1_000_000);
        let delta = stats.snapshot().since(&first);
        assert_eq!((delta.stream_frames, delta.stream_bytes), (1, 50));
        assert_eq!(delta.streams_created, 0);
        // The target is the latest, not a difference.
        assert_eq!(delta.stream_target_bps, 1_000_000);
        stats.stream_destroyed();
        let last = stats.snapshot();
        assert_eq!((last.streams_destroyed, last.stream_target_bps), (1, 0));
        // No draws: stream frames are not draws.
        assert_eq!(last.draws(), 0);
    }

    #[test]
    fn audio_counters_count_and_reach_the_summary() {
        let stats = Stats::default();
        stats.audio_started();
        for _ in 0..100 {
            stats.audio_frame_sent();
        }
        stats.bytes_sent(ChannelType::Playback, 12_500);
        stats.audio_encode_done(Duration::from_micros(40));
        stats.audio_encode_done(Duration::from_micros(60));
        stats.audio_dropped(3);
        stats.audio_dropped(2);
        stats.audio_stopped();
        let first = stats.snapshot();
        assert!(first.has_frames(), "audio frames alone count as frames");
        assert_eq!(
            (
                first.audio_frames,
                first.audio_drops,
                first.audio_starts,
                first.audio_stops
            ),
            (100, 5, 1, 1)
        );
        assert_eq!(first.audio_encode.mean(), Some(Duration::from_micros(50)));
        assert_eq!(first.total_bytes(), 12_500);
        let line = first.summary(Duration::from_secs(1));
        assert!(
            line.ends_with(
                "audio frames 100 at 100 kbit/s, opus encode 0.05 ms, audio drops 5, \
                 audio starts=1 stops=1"
            ),
            "{line}"
        );
        stats.audio_frame_sent();
        stats.audio_started();
        let delta = stats.snapshot().since(&first);
        assert_eq!((delta.audio_frames, delta.audio_starts), (1, 1));
        assert_eq!((delta.audio_drops, delta.audio_stops), (0, 0));
        assert_eq!(delta.audio_encode.count, 0);
        assert_eq!(delta.draws(), 0, "audio frames are not draws");
    }

    #[test]
    fn counters_land_in_the_right_slots() {
        let stats = Stats::default();
        stats.draw_sent(Encoding::Jpeg, 7);
        stats.draw_sent(Encoding::Bitmap, 5);
        stats.draw_sent(Encoding::Bitmap, 5);
        for (channel, bytes) in [
            (ChannelType::Main, 1),
            (ChannelType::Display, 2),
            (ChannelType::Inputs, 3),
            (ChannelType::Cursor, 4),
            (ChannelType::Playback, 5),
            (ChannelType::Record, 100),
        ] {
            stats.bytes_sent(channel, bytes);
        }
        let s = stats.snapshot();
        assert_eq!((s.lz4_draws, s.bitmap_draws, s.jpeg_draws), (0, 2, 1));
        assert_eq!((s.bitmap_bytes, s.jpeg_bytes), (10, 7));
        assert_eq!(
            (
                s.main_bytes,
                s.display_bytes,
                s.inputs_bytes,
                s.cursor_bytes,
                s.playback_bytes
            ),
            (1, 2, 3, 4, 5)
        );
        assert_eq!(s.total_bytes(), 15);
        assert_eq!(s.draws(), 3);
    }

    #[test]
    fn timings_keep_total_count_and_latest() {
        let stats = Stats::default();
        assert_eq!(stats.snapshot().ping.mean(), None);
        assert_eq!(stats.snapshot().ping.latest(), None);
        stats.ping_round_trip(Duration::from_millis(2));
        stats.ping_round_trip(Duration::from_millis(4));
        let ping = stats.snapshot().ping;
        assert_eq!(ping.count, 2);
        assert_eq!(ping.mean(), Some(Duration::from_millis(3)));
        assert_eq!(ping.latest(), Some(Duration::from_millis(4)));
    }

    #[test]
    fn since_subtracts_counters_and_keeps_the_latest() {
        let stats = Stats::default();
        stats.frame_sent(false);
        stats.draw_sent(Encoding::Lz4, 10);
        stats.ping_round_trip(Duration::from_millis(1));
        let first = stats.snapshot();
        stats.frame_sent(false);
        stats.frame_sent(true);
        stats.draw_sent(Encoding::Lz4, 30);
        stats.ping_round_trip(Duration::from_millis(9));
        let delta = stats.snapshot().since(&first);
        assert_eq!((delta.frames, delta.refreshes), (1, 1));
        assert_eq!((delta.lz4_draws, delta.lz4_bytes), (1, 30));
        assert_eq!(delta.ping.count, 1);
        assert_eq!(delta.ping.mean(), Some(Duration::from_millis(9)));
        assert_eq!(delta.ping.latest(), Some(Duration::from_millis(9)));
        // A snapshot since itself is idle.
        assert!(!first.since(&first).has_frames());
    }

    #[test]
    fn summary_formats_the_interval() {
        let line = busy().summary(Duration::from_secs(1));
        assert_eq!(
            line,
            "1.0 fps (0 refreshes), draws lz4=1 bitmap=0 jpeg=0, display 100 kbit/s, \
             capture 2.0 ms, encode 4.0 ms, ack lag 6.0 ms, ping 0.5 ms, scrub tiles 0, \
             changed 25.0% of captured, streams created=0 destroyed=0, stream frames 0 at \
             0 kbit/s, stream encode - ms, server drops 0, stream target -, audio frames 0 at \
             0 kbit/s, opus encode - ms, audio drops 0, audio starts=0 stops=0"
        );
        let line = streaming().summary(Duration::from_secs(2));
        assert!(
            line.ends_with(
                "streams created=1 destroyed=0, stream frames 2 at 100 kbit/s, \
                 stream encode 3.0 ms, server drops 1, stream target 1500 kbit/s, audio frames 0 \
                 at 0 kbit/s, opus encode - ms, audio drops 0, audio starts=0 stops=0"
            ),
            "{line}"
        );
        // With nothing measured the means are dashes.
        let line = StatsSnapshot::default().summary(Duration::from_secs(10));
        assert!(line.contains("capture - ms"), "{line}");
        assert!(line.contains("changed - of captured"), "{line}");
    }

    #[test]
    fn every_slot_is_in_range_and_its_own() {
        let encodings = [Encoding::Lz4, Encoding::Bitmap, Encoding::Jpeg].map(encoding_slot);
        let mut sorted = encodings;
        sorted.sort_unstable();
        assert_eq!(sorted, [0, 1, 2]);
        assert_eq!(encodings.len(), ENCODINGS);
        let channels = [
            ChannelType::Main,
            ChannelType::Display,
            ChannelType::Inputs,
            ChannelType::Cursor,
            ChannelType::Playback,
        ]
        .map(|channel| channel_slot(channel).unwrap());
        assert_eq!(channels, [0, 1, 2, 3, 4]);
        assert_eq!(channels.len(), CHANNELS);
    }

    #[test]
    fn the_mean_works_for_any_count() {
        let many = TimingSnapshot {
            total: Duration::from_secs(u64::from(u32::MAX) * 2),
            count: u64::from(u32::MAX) * 2,
            last: Duration::ZERO,
        };
        assert_eq!(many.mean(), Some(Duration::from_secs(1)));
        let most = TimingSnapshot {
            total: Duration::from_micros(u64::MAX),
            count: u64::MAX,
            last: Duration::ZERO,
        };
        assert_eq!(most.mean(), Some(Duration::from_micros(1)));
        assert_eq!(TimingSnapshot::default().mean(), None);
    }

    #[test]
    fn timing_totals_saturate() {
        let stats = Stats::default();
        stats.ping_round_trip(Duration::MAX);
        stats.ping_round_trip(Duration::from_secs(1));
        let ping = stats.snapshot().ping;
        assert_eq!(ping.total, Duration::from_micros(u64::MAX));
        assert_eq!(ping.count, 2);
        assert_eq!(ping.latest(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn the_scrub_count_returns_the_total() {
        let stats = Stats::default();
        assert_eq!(stats.scrub_found(3), 3);
        assert_eq!(stats.scrub_found(4), 7);
        assert_eq!(stats.snapshot().scrub_tiles, 7);
    }

    #[test]
    fn an_idle_interval_logs_nothing() {
        let mut reporter = Reporter::default();
        let idle = StatsSnapshot::default();
        assert_eq!(reporter.interval_line(idle, STATS_INTERVAL), None);

        let busy = busy();
        assert!(reporter.interval_line(busy, STATS_INTERVAL).is_some());
        // Nothing more was sent, so the next interval is quiet.
        assert_eq!(reporter.interval_line(busy, STATS_INTERVAL), None);
    }
}
