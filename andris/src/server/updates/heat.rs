//! Finding the area of the desktop worth sending as a video stream:
//! one that keeps changing, frame after frame, and looks like a
//! photograph.
//!
//! # Heat
//!
//! [`HeatMap`] keeps, for each [`TILE`] x [`TILE`] tile, the frames that
//! changed it, each with its time and whether the tile went out
//! photographic: drawn as JPEG, or as a stream's frame. The display
//! channel records each frame it sends with [`HeatMap::record`], from what
//! its encoder already decided, so nothing here looks at pixels.
//!
//! A tile is *hot* when it changed in at least [`HOT_FRAMES`] frames
//! within [`HOT_WINDOW`], with no gap of more than [`HOT_MAX_GAP`]
//! between one change and the next, nor since the last. A longer gap
//! starts the count again. These are spice-server's numbers for starting
//! a stream, which it counts over drawing commands of the same box; andris
//! has no drawing commands, only tiles that changed, so it counts them a
//! tile at a time.
//!
//! # Candidates
//!
//! A candidate is a 4-connected cluster of hot tiles, as the rectangle
//! that bounds it, clipped to the surface. It must be at least
//! [`MIN_STREAM_SIDE`] pixels each way, and at least
//! [`MIN_PHOTOGRAPHIC_PERCENT`] of its tiles' recent changes must have
//! gone out photographic. That last test is what keeps continuous change
//! that is not a picture from streaming: a scrolling terminal, a build
//! log, an animated progress bar. A client that turned JPEG off therefore
//! never gets a stream, which is deliberate: it asked for exact pixels.
//!
//! [`choose`] then decides, for one channel's one stream: the largest
//! candidate starts a stream, and while one lives, a candidate that
//! outgrows it ends it.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use crate::platform::{Rectangle, Region, Size, TILE};

/// How many frames must change a tile, within [`HOT_WINDOW`], for it to
/// be hot: spice-server starts a stream after 20 frames of the same box.
pub const HOT_FRAMES: usize = 20;

/// How far back a tile's changes count. At the default 30 frames a
/// second, [`HOT_FRAMES`] frames take two thirds of this; a desktop that
/// sends fewer than 10 frames a second never streams.
pub const HOT_WINDOW: Duration = Duration::from_secs(2);

/// The longest gap between a tile's changes that keeps its count going:
/// spice-server's limit between frames of a stream-to-be. A video plays
/// at well under this, while a person typing or scrolling now and then
/// leaves gaps over it.
pub const HOT_MAX_GAP: Duration = Duration::from_millis(200);

/// The least share of a candidate's recent changes, in percent, that
/// must have gone out photographic: spice-server's share of "gradual"
/// drawing commands a stream needs.
pub const MIN_PHOTOGRAPHIC_PERCENT: usize = 20;

/// The shortest side of a stream, in pixels: two tiles, above
/// spice-server's 96 pixels, so that a single busy tile, such as a
/// spinner, never streams.
pub const MIN_STREAM_SIDE: u32 = 2 * TILE;

/// How many times its own area a live stream's cluster may grow to
/// before the stream ends, so that detection can start afresh over the
/// larger area: a video window that was moved or enlarged.
pub const OUTGROWN_FACTOR: u64 = 2;

/// One frame's change to one tile.
#[derive(Debug, Clone, Copy)]
struct Change {
    at: Instant,
    /// Whether the tile went out photographic.
    photographic: bool,
}

/// An area worth streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// The bounds of the cluster of hot tiles, clipped to the surface:
    /// whole tiles but at the surface's edges.
    pub rect: Rectangle,
}

/// What the display channel should do about its stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Start a stream over the rectangle.
    Start(Rectangle),
    /// End the live stream: a cluster that touches it has outgrown it.
    End,
    /// Nothing: no candidate, or the live stream still fits.
    Stay,
}

/// What the display channel should do, given this frame's `candidates`
/// and the `live` stream's rectangle, if one lives.
///
/// Without a stream, the largest candidate starts one. With one, a
/// candidate that touches it and is more than [`OUTGROWN_FACTOR`] times
/// its area ends it; any other candidate waits until it has ended, since
/// a channel has one stream at a time.
pub fn choose(candidates: &[Candidate], live: Option<Rectangle>) -> Choice {
    match live {
        None => candidates
            .iter()
            .max_by_key(|candidate| candidate.rect.area())
            .map_or(Choice::Stay, |candidate| Choice::Start(candidate.rect)),
        Some(stream) => {
            let outgrown = candidates.iter().any(|candidate| {
                candidate.rect.intersects(&stream)
                    && candidate.rect.area() > stream.area().saturating_mul(OUTGROWN_FACTOR)
            });
            if outgrown { Choice::End } else { Choice::Stay }
        }
    }
}

/// The recent changes of every tile of one surface.
#[derive(Debug, Clone)]
pub struct HeatMap {
    /// The surface's tiles; no tile is held, only the grid.
    grid: Region,
    /// One entry a tile, row by row: its changes since its count last
    /// started, oldest first, none older than [`HOT_WINDOW`] at the last
    /// change.
    tiles: Vec<VecDeque<Change>>,
}

impl HeatMap {
    /// No changes yet, on a surface of `size`.
    pub fn new(size: Size) -> Self {
        let grid = Region::empty(size);
        let (columns, rows) = grid.grid();
        Self {
            grid,
            tiles: vec![VecDeque::new(); columns as usize * rows as usize],
        }
    }

    /// Forget every change, for a new surface of `size`.
    pub fn reset(&mut self, size: Size) {
        *self = Self::new(size);
    }

    /// Record a frame sent at `now`: it changed the tiles of `changed`,
    /// and those also in `photographic` went out photographic. Regions of
    /// another size than the surface's record nothing.
    pub fn record(&mut self, now: Instant, changed: &Region, photographic: &Region) {
        if changed.size() != self.grid.size() {
            return;
        }
        let columns = self.grid.grid().0;
        for (col, row) in changed.tiles() {
            let Some(changes) = self.tiles.get_mut(index(columns, col, row)) else {
                continue;
            };
            if changes
                .back()
                .is_some_and(|last| now.saturating_duration_since(last.at) > HOT_MAX_GAP)
            {
                changes.clear();
            }
            while changes
                .front()
                .is_some_and(|first| now.saturating_duration_since(first.at) > HOT_WINDOW)
            {
                changes.pop_front();
            }
            changes.push_back(Change {
                at: now,
                photographic: photographic.size() == changed.size()
                    && photographic.contains_tile(col, row),
            });
        }
    }

    /// Forget the changes of every tile of `region`, as when a stream
    /// over it ends: a stream there again needs [`HOT_FRAMES`] fresh
    /// frames.
    pub fn forget(&mut self, region: &Region) {
        if region.size() != self.grid.size() {
            return;
        }
        let columns = self.grid.grid().0;
        for (col, row) in region.tiles() {
            if let Some(changes) = self.tiles.get_mut(index(columns, col, row)) {
                changes.clear();
            }
        }
    }

    /// The candidates for a stream at `now`, largest first.
    pub fn candidates(&self, now: Instant) -> Vec<Candidate> {
        let (columns, rows) = self.grid.grid();
        let hot: Vec<bool> = self
            .tiles
            .iter()
            .map(|changes| recent(changes, now).count() >= HOT_FRAMES && !stale(changes, now))
            .collect();
        let mut seen = vec![false; hot.len()];
        let mut candidates = Vec::new();
        for start in 0..hot.len() {
            if !hot[start] || seen[start] {
                continue;
            }
            // Walk the cluster, four ways.
            seen[start] = true;
            let mut cluster = Region::empty(self.grid.size());
            let (mut changes, mut photographic) = (0, 0);
            let mut queue = vec![start];
            while let Some(at) = queue.pop() {
                let (col, row) = (
                    (at % columns as usize) as u32,
                    (at / columns as usize) as u32,
                );
                cluster.insert_tile(col, row);
                for change in recent(&self.tiles[at], now) {
                    changes += 1;
                    photographic += usize::from(change.photographic);
                }
                let neighbours = [
                    (col > 0).then(|| (col - 1, row)),
                    (col + 1 < columns).then_some((col + 1, row)),
                    (row > 0).then(|| (col, row - 1)),
                    (row + 1 < rows).then_some((col, row + 1)),
                ];
                for (c, r) in neighbours.into_iter().flatten() {
                    let next = index(columns, c, r);
                    if hot[next] && !seen[next] {
                        seen[next] = true;
                        queue.push(next);
                    }
                }
            }
            let Some(rect) = cluster.bounds() else {
                continue;
            };
            if rect.width >= MIN_STREAM_SIDE
                && rect.height >= MIN_STREAM_SIDE
                && photographic * 100 >= changes * MIN_PHOTOGRAPHIC_PERCENT
            {
                candidates.push(Candidate { rect });
            }
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.rect.area()));
        candidates
    }
}

/// The changes within [`HOT_WINDOW`] of `now`.
fn recent(changes: &VecDeque<Change>, now: Instant) -> impl Iterator<Item = &Change> {
    changes
        .iter()
        .filter(move |change| now.saturating_duration_since(change.at) <= HOT_WINDOW)
}

/// Whether the last change is more than [`HOT_MAX_GAP`] before `now`, or
/// there is none: the tile's count has stopped.
fn stale(changes: &VecDeque<Change>, now: Instant) -> bool {
    changes
        .back()
        .is_none_or(|last| now.saturating_duration_since(last.at) > HOT_MAX_GAP)
}

/// The entry of tile (`col`, `row`) in a grid `columns` tiles wide.
fn index(columns: u32, col: u32, row: u32) -> usize {
    row as usize * columns as usize + col as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Size = Size {
        width: 640,
        height: 384,
    };

    /// A frame every 33 ms, the default rate.
    const FRAME: Duration = Duration::from_millis(33);

    fn region(rects: &[Rectangle]) -> Region {
        let mut region = Region::empty(SIZE);
        for &rect in rects {
            region.add(rect);
        }
        region
    }

    /// Record `frames` frames from `start`, one every [`FRAME`], each
    /// changing `area`, photographic if `photo`. Returns the time of the
    /// last.
    fn frames(
        heat: &mut HeatMap,
        start: Instant,
        frames: u32,
        area: Rectangle,
        photo: bool,
    ) -> Instant {
        let changed = region(&[area]);
        let photographic = if photo {
            changed.clone()
        } else {
            Region::empty(SIZE)
        };
        let mut at = start;
        for frame in 0..frames {
            at = start + FRAME * frame;
            heat.record(at, &changed, &photographic);
        }
        at
    }

    const VIDEO: Rectangle = Rectangle::new(128, 64, 256, 192);

    #[test]
    fn a_moving_photograph_streams_after_twenty_frames() {
        let mut heat = HeatMap::new(SIZE);
        let start = Instant::now();
        let last = frames(&mut heat, start, 19, VIDEO, true);
        assert!(heat.candidates(last).is_empty(), "19 frames are not enough");
        let last = frames(&mut heat, last + FRAME, 1, VIDEO, true);
        assert_eq!(heat.candidates(last), vec![Candidate { rect: VIDEO }]);
        assert_eq!(choose(&heat.candidates(last), None), Choice::Start(VIDEO));
        // Still hot until the gap passes.
        assert_eq!(heat.candidates(last + HOT_MAX_GAP).len(), 1);
        assert!(heat.candidates(last + HOT_MAX_GAP + FRAME).is_empty());
    }

    #[test]
    fn continuous_change_that_is_not_a_photograph_never_streams() {
        let mut heat = HeatMap::new(SIZE);
        let last = frames(&mut heat, Instant::now(), 100, VIDEO, false);
        assert!(heat.candidates(last).is_empty());
    }

    #[test]
    fn a_fifth_photographic_is_enough() {
        // Four frames in five lossless, every fifth photographic.
        let mut heat = HeatMap::new(SIZE);
        let changed = region(&[VIDEO]);
        let start = Instant::now();
        let mut at = start;
        for frame in 0..40u32 {
            at = start + FRAME * frame;
            let photographic = if frame % 5 == 0 {
                changed.clone()
            } else {
                Region::empty(SIZE)
            };
            heat.record(at, &changed, &photographic);
        }
        assert_eq!(heat.candidates(at).len(), 1);

        // One in six is not.
        let mut heat = HeatMap::new(SIZE);
        for frame in 0..42u32 {
            at = start + FRAME * frame;
            let photographic = if frame % 6 == 0 {
                changed.clone()
            } else {
                Region::empty(SIZE)
            };
            heat.record(at, &changed, &photographic);
        }
        assert!(heat.candidates(at).is_empty());
    }

    #[test]
    fn a_one_tile_animation_never_streams() {
        let mut heat = HeatMap::new(SIZE);
        let last = frames(
            &mut heat,
            Instant::now(),
            100,
            Rectangle::new(64, 64, 64, 64),
            true,
        );
        assert!(heat.candidates(last).is_empty());
        // Nor does a strip one tile tall.
        let mut heat = HeatMap::new(SIZE);
        let last = frames(
            &mut heat,
            Instant::now(),
            100,
            Rectangle::new(0, 64, 640, 64),
            true,
        );
        assert!(heat.candidates(last).is_empty());
    }

    #[test]
    fn a_pause_over_the_gap_starts_the_count_again() {
        let mut heat = HeatMap::new(SIZE);
        let start = Instant::now();
        let last = frames(&mut heat, start, 15, VIDEO, true);
        // 250 ms later, 15 more frames make 30 in under 2 s, but the run
        // is only 15 long.
        let resumed = last + Duration::from_millis(250);
        let last = frames(&mut heat, resumed, 15, VIDEO, true);
        assert!(heat.candidates(last).is_empty());
        let last = frames(&mut heat, last + FRAME, 5, VIDEO, true);
        assert_eq!(heat.candidates(last).len(), 1);
    }

    #[test]
    fn a_pause_within_the_gap_does_not() {
        let mut heat = HeatMap::new(SIZE);
        let start = Instant::now();
        let last = frames(&mut heat, start, 10, VIDEO, true);
        let last = frames(&mut heat, last + HOT_MAX_GAP, 10, VIDEO, true);
        assert_eq!(heat.candidates(last).len(), 1);
    }

    #[test]
    fn only_the_window_counts() {
        // One frame every 150 ms: 20 of them take 2.85 s, so at most 14
        // fall within any 2 s.
        let mut heat = HeatMap::new(SIZE);
        let changed = region(&[VIDEO]);
        let start = Instant::now();
        let mut at = start;
        for frame in 0..40u32 {
            at = start + Duration::from_millis(150) * frame;
            heat.record(at, &changed, &changed);
        }
        assert!(heat.candidates(at).is_empty());
    }

    #[test]
    fn clusters_are_four_connected_and_the_largest_comes_first() {
        let mut heat = HeatMap::new(SIZE);
        let small = Rectangle::new(0, 0, 128, 128);
        // Touches `small` only at a corner.
        let large = Rectangle::new(128, 128, 256, 256);
        let changed = region(&[small, large]);
        let start = Instant::now();
        let mut at = start;
        for frame in 0..20u32 {
            at = start + FRAME * frame;
            heat.record(at, &changed, &changed);
        }
        let candidates = heat.candidates(at);
        assert_eq!(
            candidates,
            vec![Candidate { rect: large }, Candidate { rect: small }]
        );
        assert_eq!(choose(&candidates, None), Choice::Start(large));
    }

    #[test]
    fn a_cluster_at_the_edge_is_clipped_to_the_surface() {
        let size = Size {
            width: 300,
            height: 200,
        };
        let mut heat = HeatMap::new(size);
        let mut changed = Region::empty(size);
        changed.add(Rectangle::new(128, 64, 172, 136));
        let start = Instant::now();
        let mut at = start;
        for frame in 0..20u32 {
            at = start + FRAME * frame;
            heat.record(at, &changed, &changed);
        }
        assert_eq!(
            heat.candidates(at),
            vec![Candidate {
                rect: Rectangle::new(128, 64, 172, 136)
            }]
        );
    }

    #[test]
    fn forgotten_tiles_need_fresh_frames() {
        let mut heat = HeatMap::new(SIZE);
        let last = frames(&mut heat, Instant::now(), 20, VIDEO, true);
        heat.forget(&region(&[VIDEO]));
        assert!(heat.candidates(last).is_empty());
        let last = frames(&mut heat, last + FRAME, 19, VIDEO, true);
        assert!(heat.candidates(last).is_empty());
        let last = frames(&mut heat, last + FRAME, 1, VIDEO, true);
        assert_eq!(heat.candidates(last).len(), 1);
    }

    #[test]
    fn a_live_stream_ends_only_when_outgrown() {
        let stream = Rectangle::new(128, 128, 128, 128);
        let candidate = |rect| Candidate { rect };
        // Inside it, or the same: it feeds the stream.
        assert_eq!(choose(&[candidate(stream)], Some(stream)), Choice::Stay);
        // Twice its area: still within the limit.
        let twice = Rectangle::new(128, 128, 256, 128);
        assert_eq!(choose(&[candidate(twice)], Some(stream)), Choice::Stay);
        // More than twice, and touching it: outgrown.
        let larger = Rectangle::new(64, 128, 320, 128);
        assert_eq!(choose(&[candidate(larger)], Some(stream)), Choice::End);
        // A larger candidate elsewhere waits.
        let elsewhere = Rectangle::new(384, 0, 256, 256);
        assert_eq!(choose(&[candidate(elsewhere)], Some(stream)), Choice::Stay);
        assert_eq!(choose(&[], Some(stream)), Choice::Stay);
        assert_eq!(choose(&[], None), Choice::Stay);
    }

    #[test]
    fn regions_of_another_size_are_ignored() {
        let mut heat = HeatMap::new(SIZE);
        let other = Region::full(Size {
            width: 64,
            height: 64,
        });
        let start = Instant::now();
        for frame in 0..30u32 {
            heat.record(start + FRAME * frame, &other, &other);
        }
        heat.forget(&other);
        assert!(heat.candidates(start + FRAME * 29).is_empty());
    }
}
