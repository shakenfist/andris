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

/// How many per-encoding counters there are: one for each [`Encoding`].
///
/// [`encoding_slot`] matches every encoding without a wildcard, so a new
/// [`Encoding`] does not compile until it has a slot, and
/// [`Stats::snapshot`] destructures the counters by this length, so
/// giving it one means naming it there too.
const ENCODINGS: usize = 3;

/// How many per-channel byte counters there are: one for each channel
/// type andris serves.
const CHANNELS: usize = 4;

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

    /// Read every metric. The reads are independent, so a snapshot taken
    /// while channels run may be a message or two out of step.
    pub fn snapshot(&self) -> StatsSnapshot {
        // In slot order: see encoding_slot and channel_slot.
        let [lz4_draws, bitmap_draws, jpeg_draws] = load(&self.draws);
        let [lz4_bytes, bitmap_bytes, jpeg_bytes] = load(&self.draw_bytes);
        let [main_bytes, display_bytes, inputs_bytes, cursor_bytes] = load(&self.channel_bytes);
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
        // Every field is named once: a field added to the struct and not
        // here is a missing field in the struct expression below.
        macro_rules! since {
            (counters: $($counter:ident),*; timings: $($timing:ident),*) => {
                Self {
                    $($counter: self.$counter.saturating_sub(prev.$counter),)*
                    $($timing: self.$timing.since(&prev.$timing),)*
                }
            };
        }
        since!(
            counters: frames, refreshes, lz4_draws, bitmap_draws, jpeg_draws, lz4_bytes,
                bitmap_bytes, jpeg_bytes, main_bytes, display_bytes, inputs_bytes, cursor_bytes,
                captured_area, changed_area, scrub_tiles;
            timings: capture, encode, ack_lag, ping
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
            self.capture.mean_ms(),
            self.encode.mean_ms(),
            self.ack_lag.mean_ms(),
            ms(self.ping.latest()),
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
        ]
        .map(|channel| channel_slot(channel).unwrap());
        assert_eq!(channels, [0, 1, 2, 3]);
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
