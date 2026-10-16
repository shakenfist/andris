//! Damage: where the desktop may have changed, as the platform reports
//! it and the server takes it.
//!
//! - [`Rectangle`] is an area of the desktop in pixels.
//! - [`Region`] is a set of [`TILE`] x [`TILE`] tiles of a desktop of
//!   one size. Damage is rounded out to whole tiles, so a region is
//!   disjoint by construction, small (one bit a tile), and cheap to
//!   union. [`Region::rectangles`] merges its tiles into at most a given
//!   number of rectangles, and [`Region::tiles`] lists them one by one.
//!   [`Rectangle::tiles`] lists the tiles a rectangle touches, so callers
//!   need not re-derive the grid's geometry.
//! - [`DamageHub`] is the platform's side. It holds every live
//!   [`DamageSubscription`], and adds each damaged rectangle to all of
//!   them.
//! - [`DamageSubscription`] is one consumer's side, normally one display
//!   channel. It accumulates damage until [`DamageSubscription::take`]
//!   takes it, and [`DamageSubscription::changed`] waits for it without
//!   polling.
//!
//! # Taking damage
//!
//! Each subscription keeps its pending region behind one lock. Reporting
//! damage adds to the region under that lock, and
//! [`DamageSubscription::take`] swaps it for an empty region under the
//! same lock, so every damaged rectangle lands in exactly one taken
//! region. A consumer takes damage *before* it captures. Damage reported
//! after the take, whether its pixels changed before or after the
//! capture, stays pending and drives the next capture: at worst an area
//! is captured twice, never missed.
//!
//! Each subscription has its own region, so a display channel being
//! replaced by a new client's cannot take damage the new one needs, and
//! a new subscription starts with the whole desktop damaged.

use std::sync::{Arc, Mutex, MutexGuard, Weak};

use anyhow::{Result, anyhow};
use tokio::sync::Notify;

use super::{Gone, Size};

/// The side of a tile, in pixels: the granularity of a [`Region`].
pub const TILE: u32 = 64;

/// An area of the desktop: `width` x `height` pixels whose top left
/// pixel is (`x`, `y`). A rectangle with a zero side is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rectangle {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rectangle {
    /// The rectangle `width` x `height` at (`x`, `y`).
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The whole of a desktop of `size`.
    pub const fn full(size: Size) -> Self {
        Self::new(0, 0, size.width, size.height)
    }

    /// Whether the rectangle has no pixels.
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// One past the rightmost column. A `u64`, so it cannot overflow.
    pub const fn right(&self) -> u64 {
        self.x as u64 + self.width as u64
    }

    /// One past the bottom row. A `u64`, so it cannot overflow.
    pub const fn bottom(&self) -> u64 {
        self.y as u64 + self.height as u64
    }

    /// The number of pixels.
    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// The rectangle's own size, ignoring its position.
    pub const fn size(&self) -> Size {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    /// Whether the rectangle has pixels and lies wholly within a desktop
    /// of `size`.
    pub fn lies_within(&self, size: Size) -> bool {
        !self.is_empty()
            && self.right() <= u64::from(size.width)
            && self.bottom() <= u64::from(size.height)
    }

    /// The part of the rectangle within a desktop of `size`, or `None`
    /// if no pixel of it is.
    pub fn clip(&self, size: Size) -> Option<Rectangle> {
        let right = self.right().min(u64::from(size.width));
        let bottom = self.bottom().min(u64::from(size.height));
        let (x, y) = (u64::from(self.x), u64::from(self.y));
        if right <= x || bottom <= y {
            return None;
        }
        // Each difference is at most a side of `size`, so fits a u32.
        Some(Rectangle::new(
            self.x,
            self.y,
            (right - x) as u32,
            (bottom - y) as u32,
        ))
    }

    /// Whether the two rectangles share a pixel.
    pub fn intersects(&self, other: &Rectangle) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && u64::from(self.x) < other.right()
            && u64::from(other.x) < self.right()
            && u64::from(self.y) < other.bottom()
            && u64::from(other.y) < self.bottom()
    }

    /// The column and row of each [`TILE`] x [`TILE`] tile the rectangle
    /// touches, counted from the desktop's origin, top first and then
    /// left first. An empty rectangle touches none. Nothing is clipped:
    /// a rectangle reaching past a desktop touches tiles outside its
    /// grid.
    pub fn tiles(&self) -> impl Iterator<Item = (u32, u32)> + use<> {
        let tile = u64::from(TILE);
        let (cols, rows) = if self.is_empty() {
            (0..0, 0..0)
        } else {
            // A right or bottom edge is below 2^33, so its tile index is
            // below 2^27 and fits a u32.
            (
                self.x / TILE..self.right().div_ceil(tile) as u32,
                self.y / TILE..self.bottom().div_ceil(tile) as u32,
            )
        };
        rows.flat_map(move |row| cols.clone().map(move |col| (col, row)))
    }

    /// The part of the rectangle within tile (`col`, `row`), or `None`
    /// if it does not touch that tile.
    pub fn tile_part(&self, col: u32, row: u32) -> Option<Rectangle> {
        let tile = u64::from(TILE);
        let left = (u64::from(col) * tile).max(u64::from(self.x));
        let top = (u64::from(row) * tile).max(u64::from(self.y));
        let right = ((u64::from(col) + 1) * tile).min(self.right());
        let bottom = ((u64::from(row) + 1) * tile).min(self.bottom());
        if right <= left || bottom <= top {
            return None;
        }
        // The part lies within the rectangle, so each of these fits a u32.
        Some(Rectangle::new(
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        ))
    }

    /// Whether `other` lies wholly within this rectangle. An empty
    /// `other` lies within any rectangle.
    pub fn contains(&self, other: &Rectangle) -> bool {
        other.is_empty()
            || (other.x >= self.x
                && other.y >= self.y
                && other.right() <= self.right()
                && other.bottom() <= self.bottom())
    }
}

impl std::fmt::Display for Rectangle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}+{}+{}", self.width, self.height, self.x, self.y)
    }
}

/// A set of [`TILE`] x [`TILE`] tiles of a desktop of one size. The
/// tiles of the last column and row may be partial; every rectangle a
/// region yields is clipped to the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    size: Size,
    columns: u32,
    rows: u32,
    /// One bit a tile, row by row: tile (`col`, `row`) is bit
    /// `row * columns + col`.
    bits: Vec<u64>,
}

impl Region {
    /// No tiles of a desktop of `size`.
    pub fn empty(size: Size) -> Self {
        let columns = size.width.div_ceil(TILE);
        let rows = size.height.div_ceil(TILE);
        let tiles = columns as usize * rows as usize;
        Self {
            size,
            columns,
            rows,
            bits: vec![0; tiles.div_ceil(64)],
        }
    }

    /// Every tile of a desktop of `size`.
    pub fn full(size: Size) -> Self {
        let mut region = Self::empty(size);
        region.add(Rectangle::full(size));
        region
    }

    /// The size of the desktop the region belongs to.
    pub fn size(&self) -> Size {
        self.size
    }

    /// Tiles across and down a desktop of this size.
    pub fn grid(&self) -> (u32, u32) {
        (self.columns, self.rows)
    }

    /// Add every tile `rect` touches, after clipping it to the desktop.
    /// A rectangle wholly outside the desktop adds nothing.
    pub fn add(&mut self, rect: Rectangle) {
        let Some(rect) = rect.clip(self.size) else {
            return;
        };
        // Clipped, so every tile it touches is within the grid.
        for (col, row) in rect.tiles() {
            self.insert_tile(col, row);
        }
    }

    /// Add every tile of `other`. If `other` belongs to a desktop of
    /// another size, its rectangles are clipped to this region's desktop
    /// and rounded out to its tiles.
    pub fn union(&mut self, other: &Region) {
        if other.size == self.size {
            for (mine, theirs) in self.bits.iter_mut().zip(&other.bits) {
                *mine |= theirs;
            }
        } else {
            for rect in other.rectangles(usize::MAX) {
                self.add(rect);
            }
        }
    }

    /// Whether the region has no tiles.
    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&word| word == 0)
    }

    /// How many tiles the region holds.
    pub fn tile_count(&self) -> usize {
        self.bits
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// The pixels of the region's tiles, clipped to its desktop.
    pub fn area(&self) -> u64 {
        self.tiles()
            .map(|(col, row)| self.tile_rect(col, row).area())
            .sum()
    }

    /// Whether tile (`col`, `row`) is in the region; `false` outside the
    /// grid.
    pub fn contains_tile(&self, col: u32, row: u32) -> bool {
        let Some(bit) = self.bit(col, row) else {
            return false;
        };
        self.bits
            .get(bit / 64)
            .is_some_and(|word| word & (1 << (bit % 64)) != 0)
    }

    /// Add tile (`col`, `row`). A tile outside the grid adds nothing.
    pub fn insert_tile(&mut self, col: u32, row: u32) {
        if let Some(bit) = self.bit(col, row)
            && let Some(word) = self.bits.get_mut(bit / 64)
        {
            *word |= 1 << (bit % 64);
        }
    }

    /// Remove tile (`col`, `row`). A tile outside the grid removes
    /// nothing.
    pub fn remove_tile(&mut self, col: u32, row: u32) {
        if let Some(bit) = self.bit(col, row)
            && let Some(word) = self.bits.get_mut(bit / 64)
        {
            *word &= !(1 << (bit % 64));
        }
    }

    /// The column and row of each of the region's tiles, top first and
    /// then left first. Skips empty stretches a word of 64 tiles at a
    /// time, and allocates nothing.
    pub fn tiles(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let columns = self.columns.max(1) as usize;
        self.bits.iter().enumerate().flat_map(move |(at, &word)| {
            let mut word = word;
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let index = at * 64 + word.trailing_zeros() as usize;
                word &= word - 1;
                // Only bits within the grid are ever set, so both fit
                // a u32.
                Some(((index % columns) as u32, (index / columns) as u32))
            })
        })
    }

    /// The pixels of tile (`col`, `row`), clipped to the desktop: a whole
    /// [`TILE`] x [`TILE`] tile except in the last column and row. A tile
    /// outside the grid is an empty rectangle at the desktop's edge.
    pub fn tile_rect(&self, col: u32, row: u32) -> Rectangle {
        self.span_rect(col, col.saturating_add(1), row, row.saturating_add(1))
    }

    /// The region's tiles as at most `max` disjoint rectangles, clipped
    /// to the desktop, top first and then left first. A `max` of 0 is
    /// taken as 1.
    ///
    /// Tiles are merged in stages, each used only if the one before
    /// yields more than `max` rectangles:
    ///
    /// 1. runs of tiles within a tile row, then runs with the same span
    ///    in adjacent rows (exact);
    /// 2. one span per tile row, from its first tile to its last, merged
    ///    down the same way;
    /// 3. full-width bands of consecutive tile rows;
    /// 4. one rectangle bounding every tile.
    ///
    /// Stages 2 to 4 cover tiles the region does not hold.
    pub fn rectangles(&self, max: usize) -> Vec<Rectangle> {
        let max = max.max(1);
        let exact = self.merge(|row| self.runs(row));
        if exact.len() <= max {
            return exact;
        }
        let spans = self.merge(|row| self.span(row).into_iter().collect());
        if spans.len() <= max {
            return spans;
        }
        let bands = self.merge(|row| {
            self.span(row)
                .map(|_| (0, self.columns))
                .into_iter()
                .collect()
        });
        if bands.len() <= max {
            return bands;
        }
        self.bounds().into_iter().collect()
    }

    /// One rectangle bounding every tile, or `None` if there are none.
    pub fn bounds(&self) -> Option<Rectangle> {
        let mut rows = (0..self.rows).filter_map(|row| self.span(row).map(|span| (row, span)));
        let (top, (mut left, mut right)) = rows.next()?;
        let mut bottom = top;
        for (row, (first, end)) in rows {
            bottom = row;
            left = left.min(first);
            right = right.max(end);
        }
        Some(self.span_rect(left, right, top, bottom + 1))
    }

    /// Tile (`col`, `row`)'s bit, or `None` outside the grid.
    fn bit(&self, col: u32, row: u32) -> Option<usize> {
        (col < self.columns && row < self.rows)
            .then(|| row as usize * self.columns as usize + col as usize)
    }

    /// The runs of consecutive tiles in tile row `row`, as half-open
    /// column ranges, left first.
    fn runs(&self, row: u32) -> Vec<(u32, u32)> {
        let mut runs = Vec::new();
        let mut start = None;
        for col in 0..self.columns {
            match (self.contains_tile(col, row), start) {
                (true, None) => start = Some(col),
                (false, Some(first)) => {
                    runs.push((first, col));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(first) = start {
            runs.push((first, self.columns));
        }
        runs
    }

    /// The columns from tile row `row`'s first tile to its last, half
    /// open, or `None` if it has none.
    fn span(&self, row: u32) -> Option<(u32, u32)> {
        let first = (0..self.columns).find(|&col| self.contains_tile(col, row))?;
        let last = (0..self.columns)
            .rev()
            .find(|&col| self.contains_tile(col, row))?;
        Some((first, last + 1))
    }

    /// Merge the column ranges `spans` gives for each tile row into
    /// rectangles, extending a rectangle down while the next row has a
    /// range with exactly its columns.
    fn merge(&self, spans: impl Fn(u32) -> Vec<(u32, u32)>) -> Vec<Rectangle> {
        // Each open rectangle: its columns and its first row.
        let mut open: Vec<(u32, u32, u32)> = Vec::new();
        let mut done = Vec::new();
        for row in 0..self.rows {
            let mut next = Vec::new();
            for (first, end) in spans(row) {
                match open.iter().position(|o| o.0 == first && o.1 == end) {
                    Some(at) => next.push(open.swap_remove(at)),
                    None => next.push((first, end, row)),
                }
            }
            done.extend(
                open.drain(..)
                    .map(|(first, end, top)| self.span_rect(first, end, top, row)),
            );
            open = next;
        }
        done.extend(
            open.into_iter()
                .map(|(first, end, top)| self.span_rect(first, end, top, self.rows)),
        );
        done.sort_by_key(|rect| (rect.y, rect.x));
        done
    }

    /// The pixels of tile columns `first..end` and tile rows
    /// `top..bottom`, clipped to the desktop. Columns and rows beyond the
    /// grid clip to an empty rectangle at its edge.
    fn span_rect(&self, first: u32, end: u32, top: u32, bottom: u32) -> Rectangle {
        let tile = u64::from(TILE);
        let (width, height) = (u64::from(self.size.width), u64::from(self.size.height));
        let x = (u64::from(first) * tile).min(width);
        let y = (u64::from(top) * tile).min(height);
        let right = (u64::from(end) * tile).min(width);
        let bottom = (u64::from(bottom) * tile).min(height);
        // Each is at most a side of the desktop, so fits a u32.
        Rectangle::new(
            x as u32,
            y as u32,
            right.saturating_sub(x) as u32,
            bottom.saturating_sub(y) as u32,
        )
    }
}

/// What a subscription holds.
#[derive(Debug)]
struct Pending {
    region: Region,
    /// Why the desktop is gone, once it is.
    gone: Option<String>,
}

/// The state a subscription shares with its hub.
#[derive(Debug)]
struct Shared {
    pending: Mutex<Pending>,
    /// Woken whenever damage is added, the desktop resizes, or it goes.
    wake: Notify,
}

impl Shared {
    fn pending(&self) -> Result<MutexGuard<'_, Pending>> {
        self.pending
            .lock()
            .map_err(|_| anyhow!("a damage subscription's lock is poisoned"))
    }
}

/// One consumer's view of the desktop's damage. Create it with
/// [`DamageHub::subscribe`]; its pending damage starts as the whole
/// desktop. Dropping it unsubscribes.
#[derive(Debug)]
pub struct DamageSubscription {
    shared: Arc<Shared>,
}

impl DamageSubscription {
    /// Wait until damage is pending or the desktop is gone. Returns at
    /// once if either already holds, including for damage reported
    /// before this was called. Cancel-safe: a dropped call loses
    /// nothing, and the next call sees what is pending.
    pub async fn changed(&self) {
        loop {
            if self.ready() {
                return;
            }
            // Damage reported between the check and this await leaves a
            // permit, so the await returns at once. A stale permit only
            // costs one more check.
            self.shared.wake.notified().await;
        }
    }

    /// Take the pending damage, leaving an empty region of the same
    /// size. The region's size is the desktop's as last reported, so a
    /// region of another size than the consumer expects means a resize.
    /// Never blocks. Fails with [`Gone`] once the desktop has gone.
    pub fn take(&self) -> Result<Region> {
        let mut pending = self.shared.pending()?;
        if let Some(why) = &pending.gone {
            return Err(anyhow::Error::new(Gone(why.clone())));
        }
        let empty = Region::empty(pending.region.size());
        Ok(std::mem::replace(&mut pending.region, empty))
    }

    /// Put `region` back into the pending damage, as after a capture of
    /// it failed. Never blocks. Fails with [`Gone`] once the desktop has
    /// gone.
    pub fn restore(&self, region: &Region) -> Result<()> {
        self.add(region).map(|_| ())
    }

    /// Add `region` to the pending damage, as when the consumer finds a
    /// change the platform did not report, and wake the subscription if
    /// that added anything. A region of another size is clipped and
    /// rounded out, as [`Region::union`] does. Returns how many of the
    /// tiles were not already pending. Never blocks. Fails with [`Gone`]
    /// once the desktop has gone.
    pub fn add(&self, region: &Region) -> Result<usize> {
        let added = {
            let mut pending = self.shared.pending()?;
            if let Some(why) = &pending.gone {
                return Err(anyhow::Error::new(Gone(why.clone())));
            }
            let before = pending.region.tile_count();
            pending.region.union(region);
            pending.region.tile_count() - before
        };
        // Nothing added leaves the subscription as ready, or as idle, as it
        // was.
        if added > 0 {
            self.shared.wake.notify_one();
        }
        Ok(added)
    }

    /// Whether any pending damage touches `rect`, leaving it pending: for
    /// a consumer that cannot take damage yet and wants to know what it
    /// would cover. A poisoned lock counts as touching, so that
    /// [`DamageSubscription::take`] reports it.
    pub fn touches(&self, rect: Rectangle) -> bool {
        self.shared.pending().map_or(true, |pending| {
            rect.tiles()
                .any(|(col, row)| pending.region.contains_tile(col, row))
        })
    }

    /// Whether damage is pending or the desktop is gone. A poisoned lock
    /// counts as ready, so that [`DamageSubscription::take`] reports it.
    fn ready(&self) -> bool {
        self.shared.pending().map_or(true, |pending| {
            pending.gone.is_some() || !pending.region.is_empty()
        })
    }
}

/// The hub's state.
#[derive(Debug)]
struct Hub {
    size: Size,
    subscribers: Vec<Weak<Shared>>,
    gone: Option<String>,
}

impl Hub {
    /// Forget the subscriptions that have been dropped.
    fn prune(&mut self) {
        self.subscribers.retain(|weak| weak.strong_count() > 0);
    }

    /// The live subscriptions, forgetting those that have been dropped.
    fn live(&mut self) -> Vec<Arc<Shared>> {
        self.prune();
        self.subscribers.iter().filter_map(Weak::upgrade).collect()
    }
}

/// The platform's side of damage: it adds each report to every live
/// [`DamageSubscription`] and wakes them. Its methods never block for
/// long, so a platform thread may call them directly.
#[derive(Debug)]
pub struct DamageHub {
    hub: Mutex<Hub>,
}

impl DamageHub {
    /// A hub for a desktop of `size`, with no subscriptions.
    pub fn new(size: Size) -> Self {
        Self {
            hub: Mutex::new(Hub {
                size,
                subscribers: Vec::new(),
                gone: None,
            }),
        }
    }

    /// A new subscription whose pending damage is the whole desktop.
    /// Fails with [`Gone`] once the desktop has gone. Dropped
    /// subscriptions are forgotten here too, so clients that connect and
    /// leave while the desktop reports no damage cannot grow the list.
    pub fn subscribe(&self) -> Result<DamageSubscription> {
        let mut hub = self.hub()?;
        if let Some(why) = &hub.gone {
            return Err(anyhow::Error::new(Gone(why.clone())));
        }
        hub.prune();
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending {
                region: Region::full(hub.size),
                gone: None,
            }),
            wake: Notify::new(),
        });
        hub.subscribers.push(Arc::downgrade(&shared));
        Ok(DamageSubscription { shared })
    }

    /// Report `rects` as damaged to every subscription, and wake each.
    pub fn damage(&self, rects: &[Rectangle]) -> Result<()> {
        if rects.is_empty() {
            return Ok(());
        }
        for shared in self.hub()?.live() {
            {
                let mut pending = shared.pending()?;
                for &rect in rects {
                    pending.region.add(rect);
                }
            }
            shared.wake.notify_one();
        }
        Ok(())
    }

    /// The desktop is now `size`: every subscription's pending damage
    /// becomes the whole desktop at that size, and each is woken.
    pub fn resize(&self, size: Size) -> Result<()> {
        let mut hub = self.hub()?;
        hub.size = size;
        for shared in hub.live() {
            shared.pending()?.region = Region::full(size);
            shared.wake.notify_one();
        }
        Ok(())
    }

    /// The desktop has gone for good, because of `why`. Every
    /// subscription is woken, and from now on taking damage or
    /// subscribing fails with [`Gone`].
    pub fn gone(&self, why: String) -> Result<()> {
        let mut hub = self.hub()?;
        for shared in hub.live() {
            shared.pending()?.gone = Some(why.clone());
            shared.wake.notify_one();
        }
        hub.gone = Some(why);
        Ok(())
    }

    /// The desktop's size as last reported to the hub.
    pub fn size(&self) -> Result<Size> {
        Ok(self.hub()?.size)
    }

    /// How many subscriptions are live, forgetting dropped ones.
    pub fn subscribers(&self) -> Result<usize> {
        Ok(self.hub()?.live().len())
    }

    fn hub(&self) -> Result<MutexGuard<'_, Hub>> {
        self.hub
            .lock()
            .map_err(|_| anyhow!("the damage hub's lock is poisoned"))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use shakenfist_spice_compression::MAX_IMAGE_DIMENSION;

    use super::*;
    use crate::platform::is_gone;

    /// 4 x 3 tiles, the last column 36 pixels wide and the last row 8
    /// rows tall.
    const SIZE: Size = Size {
        width: 228,
        height: 136,
    };

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rectangle {
        Rectangle::new(x, y, width, height)
    }

    /// A region of `SIZE` holding the tiles marked `#` in `rows`.
    fn tiles(rows: &[&str]) -> Region {
        let mut region = Region::empty(SIZE);
        for (row, line) in rows.iter().enumerate() {
            for (col, mark) in line.chars().enumerate() {
                if mark == '#' {
                    region.add(rect(col as u32 * TILE, row as u32 * TILE, 1, 1));
                }
            }
        }
        region
    }

    #[test]
    fn rectangle_clip_and_intersect() {
        let size = Size {
            width: 100,
            height: 50,
        };
        assert_eq!(rect(90, 40, 20, 20).clip(size), Some(rect(90, 40, 10, 10)));
        assert_eq!(rect(100, 0, 5, 5).clip(size), None);
        assert_eq!(rect(0, 0, 0, 5).clip(size), None);
        assert_eq!(
            rect(u32::MAX, u32::MAX, u32::MAX, u32::MAX).clip(size),
            None
        );
        assert_eq!(
            Rectangle::full(size).clip(size),
            Some(Rectangle::full(size))
        );
        assert!(rect(0, 0, 10, 10).intersects(&rect(9, 9, 1, 1)));
        assert!(!rect(0, 0, 10, 10).intersects(&rect(10, 0, 1, 1)));
        assert!(!rect(0, 0, 10, 10).intersects(&rect(5, 5, 0, 1)));
        assert!(rect(90, 40, 10, 10).lies_within(size));
        assert!(!rect(90, 40, 11, 10).lies_within(size));
        assert!(!rect(0, 0, 0, 10).lies_within(size));
    }

    #[test]
    fn add_rounds_out_to_tiles() {
        let mut region = Region::empty(SIZE);
        assert!(region.is_empty());
        region.add(rect(63, 63, 2, 2));
        assert_eq!(region.tile_count(), 4);
        for (col, row) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            assert!(region.contains_tile(col, row));
        }
        assert!(!region.contains_tile(2, 0));
        assert!(!region.contains_tile(99, 99));
    }

    #[test]
    fn add_clips_rectangles_partly_or_wholly_outside() {
        let mut region = Region::empty(SIZE);
        region.add(rect(200, 130, 1000, 1000));
        assert_eq!(region.tile_count(), 1);
        assert!(region.contains_tile(3, 2));

        let mut region = Region::empty(SIZE);
        region.add(rect(SIZE.width, 0, 10, 10));
        region.add(rect(0, SIZE.height, 10, 10));
        region.add(rect(u32::MAX, u32::MAX, u32::MAX, u32::MAX));
        region.add(rect(0, 0, 0, 0));
        assert!(region.is_empty());
    }

    #[test]
    fn full_covers_every_tile_and_clips_the_last_column_and_row() {
        let region = Region::full(SIZE);
        assert_eq!(region.grid(), (4, 3));
        assert_eq!(region.tile_count(), 12);
        assert_eq!(region.rectangles(32), vec![Rectangle::full(SIZE)]);
        assert_eq!(region.bounds(), Some(Rectangle::full(SIZE)));
    }

    #[test]
    fn the_largest_desktop_fits() {
        let size = Size {
            width: MAX_IMAGE_DIMENSION,
            height: MAX_IMAGE_DIMENSION,
        };
        let region = Region::full(size);
        assert_eq!(region.grid(), (256, 256));
        assert_eq!(region.bits.len(), 1024, "one bit a tile");
        assert_eq!(region.tile_count(), 65536);
        assert_eq!(region.rectangles(1), vec![Rectangle::full(size)]);
        let mut corner = Region::empty(size);
        corner.add(rect(MAX_IMAGE_DIMENSION - 1, MAX_IMAGE_DIMENSION - 1, 1, 1));
        assert_eq!(corner.rectangles(1), vec![rect(16320, 16320, 64, 64)]);
    }

    #[test]
    fn a_zero_sized_region_is_empty() {
        let region = Region::full(Size {
            width: 0,
            height: 0,
        });
        assert!(region.is_empty());
        assert!(region.rectangles(32).is_empty());
        assert_eq!(region.bounds(), None);
    }

    #[test]
    fn rectangles_of_an_empty_region() {
        assert!(Region::empty(SIZE).rectangles(32).is_empty());
    }

    #[test]
    fn rectangles_merge_runs_within_a_row() {
        let region = tiles(&["##.#", "....", "...."]);
        assert_eq!(
            region.rectangles(32),
            vec![rect(0, 0, 128, 64), rect(192, 0, 36, 64)]
        );
    }

    #[test]
    fn rectangles_merge_equal_spans_down() {
        let region = tiles(&[".##.", ".##.", "...."]);
        assert_eq!(region.rectangles(32), vec![rect(64, 0, 128, 128)]);
    }

    #[test]
    fn rectangles_do_not_merge_unequal_spans() {
        let region = tiles(&[".##.", ".###", ".##."]);
        assert_eq!(
            region.rectangles(32),
            vec![
                rect(64, 0, 128, 64),
                rect(64, 64, 164, 64),
                rect(64, 128, 128, 8),
            ]
        );
    }

    #[test]
    fn rectangles_merge_a_span_that_reappears_beside_another() {
        // The left column runs down all three rows while the right
        // breaks and restarts.
        let region = tiles(&["#..#", "#...", "#..#"]);
        assert_eq!(
            region.rectangles(32),
            vec![
                rect(0, 0, 64, 136),
                rect(192, 0, 36, 64),
                rect(192, 128, 36, 8),
            ]
        );
    }

    #[test]
    fn rectangles_clip_the_last_column_and_row() {
        let region = tiles(&["....", "....", "...#"]);
        assert_eq!(region.rectangles(32), vec![rect(192, 128, 36, 8)]);
    }

    #[test]
    fn over_the_cap_rows_collapse_to_spans() {
        let region = tiles(&["#.#.", "#.#.", "...."]);
        assert_eq!(region.rectangles(2).len(), 2);
        assert_eq!(region.rectangles(1), vec![rect(0, 0, 192, 128)]);
        // Spans merge down only while they are equal.
        let region = tiles(&["#.#.", ".#.#", "#.#."]);
        assert_eq!(region.rectangles(6).len(), 6, "exact");
        assert_eq!(
            region.rectangles(3),
            vec![
                rect(0, 0, 192, 64),
                rect(64, 64, 164, 64),
                rect(0, 128, 192, 8),
            ]
        );
    }

    #[test]
    fn over_the_cap_spans_collapse_to_bands() {
        // Three single tiles whose spans differ, so stage 2 still yields
        // three rectangles.
        let region = tiles(&["#...", ".#..", "#..."]);
        assert_eq!(region.rectangles(3).len(), 3);
        // With a cap of 2 the rows become full-width bands, which merge
        // into one.
        assert_eq!(region.rectangles(2), vec![rect(0, 0, 228, 136)]);
    }

    #[test]
    fn over_the_cap_bands_collapse_to_the_bounds() {
        // Two separate bands even when full width.
        let region = tiles(&[".#..", "....", "..#."]);
        assert_eq!(region.rectangles(2).len(), 2, "two single tiles");
        assert_eq!(
            region.rectangles(1),
            vec![rect(64, 0, 128, 136)],
            "the bounds, not full width"
        );
    }

    #[test]
    fn a_cap_of_zero_is_one() {
        let region = tiles(&["#..#", "....", "...."]);
        assert_eq!(region.rectangles(0).len(), 1);
    }

    #[test]
    fn union_of_the_same_size_adds_tiles() {
        let mut region = tiles(&["#...", "....", "...."]);
        region.union(&tiles(&["....", "....", "...#"]));
        assert_eq!(region, tiles(&["#...", "....", "...#"]));
    }

    #[test]
    fn union_of_another_size_clips() {
        let mut region = Region::empty(SIZE);
        let mut larger = Region::empty(Size {
            width: 1000,
            height: 1000,
        });
        // Inside; straddling the right edge; wholly outside.
        larger.add(rect(100, 100, 1, 1));
        larger.add(rect(200, 100, 1, 1));
        larger.add(rect(900, 900, 1, 1));
        region.union(&larger);
        assert_eq!(region, tiles(&["....", ".#.#", "...."]));

        let mut region = Region::empty(SIZE);
        let smaller = Region::full(Size {
            width: 10,
            height: 10,
        });
        region.union(&smaller);
        assert_eq!(region, tiles(&["#...", "....", "...."]));
    }

    #[test]
    fn a_rectangle_lists_the_tiles_it_touches() {
        assert_eq!(
            rect(63, 10, 2, 60).tiles().collect::<Vec<_>>(),
            vec![(0, 0), (1, 0), (0, 1), (1, 1)]
        );
        assert_eq!(
            rect(64, 64, 64, 64).tiles().collect::<Vec<_>>(),
            vec![(1, 1)]
        );
        // Unaligned, but empty.
        assert_eq!(rect(10, 10, 0, 5).tiles().count(), 0);
        assert_eq!(rect(10, 10, 5, 0).tiles().count(), 0);
        // Nothing overflows at the far edge of the coordinates.
        let far = rect(u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        assert_eq!(far.tiles().next(), Some((u32::MAX / TILE, u32::MAX / TILE)));
    }

    #[test]
    fn a_rectangle_is_cut_into_its_tile_parts() {
        let r = rect(60, 0, 10, 70);
        assert_eq!(r.tile_part(0, 0), Some(rect(60, 0, 4, 64)));
        assert_eq!(r.tile_part(1, 0), Some(rect(64, 0, 6, 64)));
        assert_eq!(r.tile_part(0, 1), Some(rect(60, 64, 4, 6)));
        assert_eq!(r.tile_part(2, 0), None);
        assert_eq!(rect(0, 0, 0, 0).tile_part(0, 0), None);
        let parts: u64 = r
            .tiles()
            .filter_map(|(col, row)| r.tile_part(col, row))
            .map(|part| part.area())
            .sum();
        assert_eq!(parts, r.area());
    }

    #[test]
    fn rectangle_containment() {
        let outer = rect(10, 10, 20, 20);
        assert!(outer.contains(&outer));
        assert!(outer.contains(&rect(15, 15, 15, 15)));
        assert!(!outer.contains(&rect(15, 15, 16, 15)));
        assert!(!outer.contains(&rect(9, 15, 2, 2)));
        assert!(outer.contains(&rect(500, 500, 0, 0)), "empty");
    }

    #[test]
    fn tiles_lists_the_region_top_first_then_left() {
        let region = tiles(&["#..#", ".#..", "...#"]);
        assert_eq!(
            region.tiles().collect::<Vec<_>>(),
            vec![(0, 0), (3, 0), (1, 1), (3, 2)]
        );
        assert_eq!(Region::empty(SIZE).tiles().count(), 0);
        // Across the word boundaries of the largest desktop.
        let size = Size {
            width: MAX_IMAGE_DIMENSION,
            height: MAX_IMAGE_DIMENSION,
        };
        let full = Region::full(size);
        assert_eq!(full.tiles().count(), 65536);
        assert_eq!(full.tiles().last(), Some((255, 255)));
    }

    #[test]
    fn tile_rect_clips_to_the_desktop() {
        let region = Region::empty(SIZE);
        assert_eq!(region.tile_rect(0, 0), rect(0, 0, 64, 64));
        assert_eq!(region.tile_rect(3, 1), rect(192, 64, 36, 64));
        assert_eq!(region.tile_rect(3, 2), rect(192, 128, 36, 8));
        assert!(region.tile_rect(4, 0).is_empty(), "outside the grid");
        assert!(region.tile_rect(u32::MAX, u32::MAX).is_empty());
    }

    #[test]
    fn tiles_are_inserted_and_removed_one_at_a_time() {
        let mut region = Region::empty(SIZE);
        region.insert_tile(3, 2);
        region.insert_tile(1, 0);
        // Outside the grid: not taken as a tile of the next row.
        region.insert_tile(4, 0);
        assert_eq!(region, tiles(&[".#..", "....", "...#"]));
        assert_eq!(region.area(), 64 * 64 + 36 * 8);
        region.remove_tile(1, 0);
        region.remove_tile(4, 0);
        assert_eq!(region, tiles(&["....", "....", "...#"]));
        assert_eq!(Region::full(SIZE).area(), Rectangle::full(SIZE).area());
    }

    /// Whether `changed` returns within a short wait.
    async fn returns(subscription: &DamageSubscription) -> bool {
        tokio::time::timeout(Duration::from_millis(50), subscription.changed())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn a_new_subscription_has_the_whole_desktop_pending() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        assert!(returns(&subscription).await);
        assert_eq!(subscription.take().unwrap(), Region::full(SIZE));
    }

    #[tokio::test]
    async fn take_leaves_the_region_empty() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();
        let taken = subscription.take().unwrap();
        assert!(taken.is_empty());
        assert_eq!(taken.size(), SIZE);
        assert!(!returns(&subscription).await);
    }

    #[tokio::test]
    async fn damage_before_waiting_is_not_lost() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();
        hub.damage(&[rect(70, 70, 1, 1)]).unwrap();
        assert!(returns(&subscription).await);
        assert_eq!(
            subscription.take().unwrap(),
            tiles(&["....", ".#..", "...."])
        );
    }

    #[tokio::test]
    async fn damage_while_waiting_wakes() {
        let hub = Arc::new(DamageHub::new(SIZE));
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();
        let reporter = hub.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            reporter.damage(&[rect(0, 0, 1, 1)]).unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), subscription.changed())
            .await
            .unwrap();
        task.await.unwrap();
        assert_eq!(subscription.take().unwrap().tile_count(), 1);
    }

    #[tokio::test]
    async fn changed_is_cancel_safe() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();
        // A pending changed() dropped by select!.
        tokio::select! {
            _ = subscription.changed() => panic!("nothing was pending"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        hub.damage(&[rect(0, 0, 1, 1)]).unwrap();
        assert!(returns(&subscription).await);
        subscription.take().unwrap();

        // A changed() that is waiting, is woken by damage, and is dropped
        // before it is polled again: the next call still returns.
        // Boxed, so that dropping it drops the future itself.
        let mut waiting = Box::pin(subscription.changed());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        hub.damage(&[rect(0, 0, 1, 1)]).unwrap();
        drop(waiting);
        assert!(returns(&subscription).await);
        assert_eq!(subscription.take().unwrap().tile_count(), 1);
    }

    #[tokio::test]
    async fn a_stale_wake_is_not_a_change() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();
        // Damage taken without waiting leaves a permit behind.
        hub.damage(&[rect(0, 0, 1, 1)]).unwrap();
        subscription.take().unwrap();
        assert!(!returns(&subscription).await);
    }

    #[tokio::test]
    async fn restore_puts_damage_back() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();
        hub.damage(&[rect(0, 0, 1, 1)]).unwrap();
        let taken = subscription.take().unwrap();
        hub.damage(&[rect(200, 0, 1, 1)]).unwrap();
        subscription.restore(&taken).unwrap();
        assert!(returns(&subscription).await);
        assert_eq!(
            subscription.take().unwrap(),
            tiles(&["#..#", "....", "...."])
        );
    }

    #[tokio::test]
    async fn add_counts_only_new_tiles_and_wakes_only_for_them() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();
        assert_eq!(subscription.add(&Region::empty(SIZE)).unwrap(), 0);
        assert!(!returns(&subscription).await, "nothing added, no wake");

        hub.damage(&[rect(0, 0, 1, 1)]).unwrap();
        assert_eq!(
            subscription.add(&tiles(&["##..", "....", "...."])).unwrap(),
            1,
            "tile (0, 0) was already pending"
        );
        assert!(returns(&subscription).await);
        assert_eq!(
            subscription.take().unwrap(),
            tiles(&["##..", "....", "...."])
        );

        // A waiting subscription is woken by an addition.
        let mut waiting = Box::pin(subscription.changed());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        assert_eq!(
            subscription.add(&tiles(&["....", "....", "...#"])).unwrap(),
            1
        );
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap();

        // Another size is clipped to the desktop's.
        let mut larger = Region::empty(Size {
            width: 1000,
            height: 1000,
        });
        larger.add(rect(900, 900, 1, 1));
        larger.add(rect(70, 70, 1, 1));
        assert_eq!(subscription.add(&larger).unwrap(), 1);

        hub.gone("the X11 damage thread has stopped".to_string())
            .unwrap();
        assert!(is_gone(&subscription.add(&Region::full(SIZE)).unwrap_err()));
    }

    #[tokio::test]
    async fn resize_makes_the_pending_region_full_at_the_new_size() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();
        let size = Size {
            width: 100,
            height: 100,
        };
        hub.resize(size).unwrap();
        assert!(returns(&subscription).await);
        assert_eq!(subscription.take().unwrap(), Region::full(size));
        assert_eq!(hub.size().unwrap(), size);
        assert_eq!(hub.subscribe().unwrap().take().unwrap(), Region::full(size));
    }

    #[tokio::test]
    async fn damage_reaches_every_subscription() {
        let hub = DamageHub::new(SIZE);
        let first = hub.subscribe().unwrap();
        let second = hub.subscribe().unwrap();
        first.take().unwrap();
        second.take().unwrap();
        hub.damage(&[rect(0, 0, 1, 1)]).unwrap();
        assert_eq!(first.take().unwrap().tile_count(), 1);
        assert_eq!(second.take().unwrap().tile_count(), 1);
    }

    #[tokio::test]
    async fn after_gone_changed_returns_and_take_fails() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        subscription.take().unwrap();
        hub.gone("the X11 damage thread has stopped".to_string())
            .unwrap();
        assert!(returns(&subscription).await);
        let error = subscription.take().unwrap_err();
        assert!(is_gone(&error));
        assert!(error.to_string().contains("damage thread"));
        assert!(is_gone(
            &subscription.restore(&Region::full(SIZE)).unwrap_err()
        ));
        assert!(is_gone(&hub.subscribe().unwrap_err()));
    }

    #[test]
    fn a_dropped_subscription_is_pruned() {
        let hub = DamageHub::new(SIZE);
        let kept = hub.subscribe().unwrap();
        let dropped = hub.subscribe().unwrap();
        assert_eq!(hub.subscribers().unwrap(), 2);
        drop(dropped);
        hub.damage(&[rect(0, 0, 1, 1)]).unwrap();
        assert_eq!(hub.subscribers().unwrap(), 1);
        assert_eq!(hub.hub().unwrap().subscribers.len(), 1);
        drop(kept);
        assert_eq!(hub.subscribers().unwrap(), 0);
    }

    #[test]
    fn subscribing_prunes_dropped_subscriptions() {
        // No damage is reported, so only subscribing can prune.
        let hub = DamageHub::new(SIZE);
        let kept = hub.subscribe().unwrap();
        for _ in 0..100 {
            drop(hub.subscribe().unwrap());
        }
        assert_eq!(hub.hub().unwrap().subscribers.len(), 2);
        drop(kept);
    }

    #[test]
    fn touches_peeks_without_taking() {
        let hub = DamageHub::new(SIZE);
        let subscription = hub.subscribe().unwrap();
        // A new subscription has the whole desktop pending.
        subscription.take().unwrap();
        assert!(!subscription.touches(rect(0, 0, 64, 64)));
        hub.damage(&[rect(70, 10, 1, 1)]).unwrap();
        assert!(subscription.touches(rect(0, 0, 65, 64)));
        assert!(!subscription.touches(rect(0, 0, 64, 64)));
        assert!(!subscription.touches(rect(128, 0, 64, 64)));
        // Still pending.
        assert_eq!(subscription.take().unwrap().tile_count(), 1);
    }
}
