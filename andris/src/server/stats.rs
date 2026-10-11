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

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use shakenfist_spice_protocol::ChannelType;

use super::updates::Encoding;

/// How often the main channel logs the summary, while frames flow.
pub const STATS_INTERVAL: Duration = Duration::from_secs(10);

const RELAXED: Ordering = Ordering::Relaxed;

/// The slot of `encoding` in the per-encoding counters.
fn encoding_slot(encoding: Encoding) -> usize {
    match encoding {
        Encoding::Lz4 => 0,
        Encoding::Bitmap => 1,
        Encoding::Jpeg => 2,
    }
}

/// The slot of `channel` in the per-channel byte counters, if andris
/// serves it.
fn channel_slot(channel: ChannelType) -> Option<usize> {
    match channel {
        ChannelType::Main => Some(0),
        ChannelType::Display => Some(1),
        ChannelType::Inputs => Some(2),
        ChannelType::Cursor => Some(3),
        _ => None,
    }
}

/// A duration in whole microseconds, saturating.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// A total and a count of timed events, with the latest.
#[derive(Debug, Default)]
struct Timing {
    total_us: AtomicU64,
    count: AtomicU64,
    last_us: AtomicU64,
}

impl Timing {
    fn record(&self, duration: Duration) {
        let us = micros(duration);
        self.total_us.fetch_add(us, RELAXED);
        self.count.fetch_add(1, RELAXED);
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
    /// The mean duration, if any has been recorded.
    pub fn mean(&self) -> Option<Duration> {
        let count = u32::try_from(self.count).ok().filter(|&c| c > 0)?;
        Some(self.total / count)
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
    draws: [AtomicU64; 3],
    draw_bytes: [AtomicU64; 3],
    channel_bytes: [AtomicU64; 4],
    capture: Timing,
    encode: Timing,
    captured_area: AtomicU64,
    changed_area: AtomicU64,
    scrub_tiles: AtomicU64,
    ack_lag: Timing,
    ping: Timing,
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
    /// reported.
    pub fn scrub_found(&self, tiles: u64) {
        self.scrub_tiles.fetch_add(tiles, RELAXED);
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

    /// Read every metric. The reads are independent, so a snapshot taken
    /// while channels run may be a message or two out of step.
    pub fn snapshot(&self) -> StatsSnapshot {
        let load = |counters: &[AtomicU64]| -> Vec<u64> {
            counters.iter().map(|c| c.load(RELAXED)).collect()
        };
        let draws = load(&self.draws);
        let draw_bytes = load(&self.draw_bytes);
        let bytes = load(&self.channel_bytes);
        StatsSnapshot {
            frames: self.frames.load(RELAXED),
            refreshes: self.refreshes.load(RELAXED),
            lz4_draws: draws[0],
            bitmap_draws: draws[1],
            jpeg_draws: draws[2],
            lz4_bytes: draw_bytes[0],
            bitmap_bytes: draw_bytes[1],
            jpeg_bytes: draw_bytes[2],
            main_bytes: bytes[0],
            display_bytes: bytes[1],
            inputs_bytes: bytes[2],
            cursor_bytes: bytes[3],
            capture: self.capture.read(),
            encode: self.encode.read(),
            captured_area: self.captured_area.load(RELAXED),
            changed_area: self.changed_area.load(RELAXED),
            scrub_tiles: self.scrub_tiles.load(RELAXED),
            ack_lag: self.ack_lag.read(),
            ping: self.ping.read(),
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
}

impl StatsSnapshot {
    /// Draws sent, of every encoding.
    pub fn draws(&self) -> u64 {
        self.lz4_draws + self.bitmap_draws + self.jpeg_draws
    }

    /// Bytes written on every channel.
    pub fn total_bytes(&self) -> u64 {
        self.main_bytes + self.display_bytes + self.inputs_bytes + self.cursor_bytes
    }

    /// Whether any frame or refresh was sent: what the periodic log line
    /// waits for.
    pub fn has_frames(&self) -> bool {
        self.frames + self.refreshes > 0
    }

    /// What happened since `prev`, an earlier snapshot of the same
    /// session. Counters and totals are differences; the latest ack lag
    /// and ping are this snapshot's.
    pub fn since(&self, prev: &Self) -> Self {
        let d = |now: u64, then: u64| now.saturating_sub(then);
        Self {
            frames: d(self.frames, prev.frames),
            refreshes: d(self.refreshes, prev.refreshes),
            lz4_draws: d(self.lz4_draws, prev.lz4_draws),
            bitmap_draws: d(self.bitmap_draws, prev.bitmap_draws),
            jpeg_draws: d(self.jpeg_draws, prev.jpeg_draws),
            lz4_bytes: d(self.lz4_bytes, prev.lz4_bytes),
            bitmap_bytes: d(self.bitmap_bytes, prev.bitmap_bytes),
            jpeg_bytes: d(self.jpeg_bytes, prev.jpeg_bytes),
            main_bytes: d(self.main_bytes, prev.main_bytes),
            display_bytes: d(self.display_bytes, prev.display_bytes),
            inputs_bytes: d(self.inputs_bytes, prev.inputs_bytes),
            cursor_bytes: d(self.cursor_bytes, prev.cursor_bytes),
            capture: self.capture.since(&prev.capture),
            encode: self.encode.since(&prev.encode),
            captured_area: d(self.captured_area, prev.captured_area),
            changed_area: d(self.changed_area, prev.changed_area),
            scrub_tiles: d(self.scrub_tiles, prev.scrub_tiles),
            ack_lag: self.ack_lag.since(&prev.ack_lag),
            ping: self.ping.since(&prev.ping),
        }
    }

    /// The one-line summary of this snapshot (normally a
    /// [`StatsSnapshot::since`] difference) over `elapsed`.
    pub fn summary(&self, elapsed: Duration) -> String {
        let secs = elapsed.as_secs_f64().max(f64::EPSILON);
        let ms = |t: &TimingSnapshot| {
            t.mean().map_or_else(
                || '-'.to_string(),
                |d| format!("{:.1}", d.as_secs_f64() * 1e3),
            )
        };
        let ping = self.ping.latest().map_or_else(
            || '-'.to_string(),
            |d| format!("{:.1}", d.as_secs_f64() * 1e3),
        );
        let changed = if self.captured_area > 0 {
            format!(
                "{:.1}%",
                self.changed_area as f64 * 100.0 / self.captured_area as f64
            )
        } else {
            '-'.to_string()
        };
        format!(
            "{:.1} fps ({} refreshes), draws lz4={} bitmap={} jpeg={}, display {:.0} kbit/s, \
             capture {} ms, encode {} ms, ack lag {} ms, ping {} ms, scrub tiles {}, \
             changed {} of captured",
            (self.frames + self.refreshes) as f64 / secs,
            self.refreshes,
            self.lz4_draws,
            self.bitmap_draws,
            self.jpeg_draws,
            self.display_bytes as f64 * 8.0 / 1000.0 / secs,
            ms(&self.capture),
            ms(&self.encode),
            ms(&self.ack_lag),
            ping,
            self.scrub_tiles,
            changed,
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
            (ChannelType::Playback, 100),
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
                s.cursor_bytes
            ),
            (1, 2, 3, 4)
        );
        assert_eq!(s.total_bytes(), 10);
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
             changed 25.0% of captured"
        );
        // With nothing measured the means are dashes.
        let line = StatsSnapshot::default().summary(Duration::from_secs(10));
        assert!(line.contains("capture - ms"), "{line}");
        assert!(line.contains("changed - of captured"), "{line}");
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
