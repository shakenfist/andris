//! The update pipeline: from captured pixels to the `DRAW_COPY` bodies
//! that bring the client's surface up to date.
//!
//! # The shadow
//!
//! [`UpdateEncoder`] keeps a *shadow*: the pixels of the whole surface as
//! andris last sent them, which is what the client's surface holds. A
//! capture is compared with the shadow, not with the capture before it,
//! so a change is sent once however it came to be captured.
//!
//! A shadow is made from a complete capture, of the whole desktop, and
//! every tile of that first capture counts as changed: the first frame of
//! a surface is all of it. A capture of another size than the shadow
//! drops it, and must itself be complete. So must the first capture after
//! [`UpdateEncoder::reset`].
//!
//! # From a capture to rectangles
//!
//! 1. Each patch of the capture is compared with the shadow one [`TILE`]
//!    x [`TILE`] tile at a time, a row at a time, over the part of the
//!    tile the patch covers. A patch that covers a tile only in part, such
//!    as a band of rows not aligned to tiles, is compared over that part
//!    alone; the rest of the tile is not looked at.
//! 2. Each part that differs is copied into the shadow, and its tile joins
//!    the changed [`Region`].
//! 3. [`Region::rectangles`] merges the region into at most
//!    [`MAX_RECTS_PER_FRAME`] rectangles. Over that cap its later stages
//!    cover tiles that did not change, or were not captured.
//! 4. A rectangle over [`MAX_DRAW_BYTES`] raw is split by rows into pieces
//!    that fit, so that every message stays far below kerbside's 16 MiB
//!    cap. The pieces are whole tile rows, so every draw covers whole
//!    tiles (clipped to the surface).
//! 5. Each piece is encoded from the shadow as one `DRAW_COPY`.
//!
//! The shadow is brought up to date before anything is encoded, and every
//! piece is encoded from it. A tile the merge covered but that did not
//! change, or was not captured, therefore carries what the client already
//! has, so sending it again is harmless.
//!
//! # Encoding
//!
//! Each `DRAW_COPY` draws on surface [`SURFACE_ID`]: the rectangle as the
//! box, no clip, `OP_PUT`, no mask, and one image exactly the rectangle's
//! size, all of which is the source area. The image is, in order of
//! preference:
//!
//! 1. JPEG, when the client may take it ([`EncodeOptions::jpeg`]), the
//!    rectangle [`looks_photographic`], and the JPEG comes to at most
//!    [`JPEG_MAX_PERCENT`] of what lossless would send;
//! 2. LZ4, when the client may take LZ4 and the result is smaller than
//!    the raw pixels;
//! 3. an uncompressed top-down 32-bit `BITMAP`.
//!
//! The classifier's thresholds were chosen from measurements of real
//! screens; each constant says what they showed. JPEG is never sent with
//! alpha. Each [`Encoding`] says whether it is lossy, and only JPEG is:
//! [`LossyTiles`] tracks where the client holds JPEG's approximation, so
//! that it can be sent again losslessly once it stops changing. Image ids
//! count from 1, and no image asks to be cached.
//!
//! This module knows nothing of sockets, so it is tested on its own.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use shakenfist_spice_compression::{Bgrx, encode_spice_jpeg, encode_spice_lz4};
use shakenfist_spice_protocol::ImageType;
use shakenfist_spice_protocol::constants::{bitmap_flags, bitmap_fmt, ropd};
use shakenfist_spice_protocol::messages::{
    BinaryData, BitmapHeader, BitmapPalette, BitmapPayload, Clip, DrawBase, DrawCopyBuilder,
    ImageDescriptor, ImagePayload, Rect, SpiceImage,
};
use tokio::time::Instant;

use crate::platform::{Capture, Patch, Rectangle, Region, Size, TILE};

/// The most rectangles one update merges its changed tiles into, before
/// any is split. Each becomes at least one message, and a client pays a
/// cost per message.
pub const MAX_RECTS_PER_FRAME: usize = 32;

/// The most raw pixel bytes in one `DRAW_COPY`: a larger rectangle is
/// split by rows. A row of the widest desktop is 64 KiB, so a piece is
/// always at least 64 rows.
pub const MAX_DRAW_BYTES: usize = 4 * 1024 * 1024;

/// The surface every update draws on: the primary surface.
pub const SURFACE_ID: u32 = 0;

/// The smallest rectangle, in pixels, that may be sent as JPEG: one
/// whole [`TILE`].
///
/// Measured with `andris/tests/jpeg_corpus.rs` on a corpus of real
/// screens, photographic content gains plenty at one tile: the JPEG at
/// quality 85 of a 64 x 64 photo tile is under a tenth of its LZ4.
/// Rectangles are tile-merged, so anything smaller is a tile clipped by
/// the surface's edge, and stays lossless.
pub const JPEG_MIN_AREA: u64 = TILE as u64 * TILE as u64;

/// The shortest side of a rectangle that may be sent as JPEG: one 4:2:0
/// MCU. A thinner strip is mostly padding.
pub const JPEG_MIN_SIDE: u32 = 16;

/// A rectangle is tried as JPEG only if its LZ4 encoding is more than
/// this percentage of its raw bytes: LZ4 saves less than half.
///
/// This is the classifier's signal. In the corpus every 64 x 64 tile of
/// text, whether a terminal, a document or a real desktop, came out
/// under 30%, and almost all under 20%; photographs came out at 40% to
/// 100%, with 1% of their tiles under 50%. Smooth gradients came out at
/// 10% to 40%, overlapping the text, so no threshold separates them, and
/// they stay lossless, where LZ4 already takes them to a fifth or less.
/// 50% keeps the widest margin from text, where a JPEG would smear
/// glyphs, for the cost of that 1% of photo tiles going as LZ4. The size
/// test that follows cannot stand in for this one: JPEG was the smaller
/// for 43% of the real desktop's tiles, and for a whole terminal.
pub const JPEG_MIN_LZ4_PERCENT: u64 = 50;

/// A JPEG is sent only if it is at most this percentage of what lossless
/// would send.
///
/// An area sent as JPEG is sent again losslessly once still, so a JPEG
/// that saves little costs more than it gains. In the corpus the JPEG of
/// a photograph came to 3% to 9% of its LZ4, by rectangle size, and
/// this limit turned none away; it bounds the waste on content the
/// classifier misjudges.
pub const JPEG_MAX_PERCENT: u64 = 50;

/// A JPEG quality, 1 to 100: `--jpeg-quality` when it is not 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegQuality(u8);

impl JpegQuality {
    /// `--jpeg-quality`'s default, and spice-server's.
    pub const DEFAULT: Self = Self(85);

    /// The quality `quality`, or `None` unless it is 1 to 100.
    pub const fn new(quality: u8) -> Option<Self> {
        if matches!(quality, 1..=100) {
            Some(Self(quality))
        } else {
            None
        }
    }

    /// The quality, 1 to 100.
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// What the client may be sent, as far as encoding goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncodeOptions {
    /// Whether images may be LZ4.
    pub lz4: bool,
    /// The quality to encode a photographic rectangle as JPEG at, or
    /// `None` if images must be lossless.
    pub jpeg: Option<JpegQuality>,
}

impl EncodeOptions {
    /// These options with JPEG ruled out: for a lossless refresh.
    pub fn lossless(self) -> Self {
        Self { jpeg: None, ..self }
    }
}

/// How one draw's image is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// LZ4-compressed BGRX.
    Lz4,
    /// An uncompressed top-down 32-bit `BITMAP`.
    Bitmap,
    /// A baseline JPEG, with no alpha.
    Jpeg,
}

impl Encoding {
    /// Whether the client's pixels may differ from the shadow's once the
    /// image is drawn: only for JPEG.
    pub fn is_lossy(self) -> bool {
        match self {
            Self::Lz4 | Self::Bitmap => false,
            Self::Jpeg => true,
        }
    }

    /// The SPICE image type this encoding is sent as.
    fn image_type(self) -> ImageType {
        match self {
            Self::Lz4 => ImageType::Lz4,
            Self::Bitmap => ImageType::Pixmap,
            Self::Jpeg => ImageType::Jpeg,
        }
    }
}

/// One `DRAW_COPY`, ready to send.
#[derive(Clone, PartialEq, Eq)]
pub struct Draw {
    /// The part of the surface it draws.
    pub rect: Rectangle,
    /// How its image is encoded.
    pub encoding: Encoding,
    /// The `DRAW_COPY` message body.
    pub body: Vec<u8>,
}

impl Draw {
    /// Whether the client's pixels in [`Draw::rect`] may differ from the
    /// shadow's once it is drawn.
    pub fn is_lossy(&self) -> bool {
        self.encoding.is_lossy()
    }

    /// The rectangle's pixels as raw BGRX, in bytes.
    pub fn raw_bytes(&self) -> u64 {
        self.rect.area() * 4
    }

    /// The message body's length in bytes: the image and the drawing
    /// fields around it.
    pub fn encoded_bytes(&self) -> u64 {
        self.body.len() as u64
    }
}

impl std::fmt::Debug for Draw {
    // The body would swamp any log or assertion message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Draw")
            .field("rect", &self.rect)
            .field("encoding", &self.encoding)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// What one capture, or one refresh, sends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Update {
    /// The draws, top first and then left first; empty when nothing
    /// changed. Each draws one rectangle, so their number is the
    /// rectangles sent.
    pub draws: Vec<Draw>,
    /// The pixels the capture held, counting twice any it held twice.
    pub captured_area: u64,
    /// The pixels of the tiles that changed, clipped to the surface.
    pub changed_area: u64,
}

impl Update {
    /// Whether there is nothing to send.
    pub fn is_empty(&self) -> bool {
        self.draws.is_empty()
    }

    /// The draws' rectangles as raw BGRX, in bytes.
    pub fn raw_bytes(&self) -> u64 {
        self.draws.iter().map(Draw::raw_bytes).sum()
    }

    /// The draws' message bodies, in bytes.
    pub fn encoded_bytes(&self) -> u64 {
        self.draws.iter().map(Draw::encoded_bytes).sum()
    }
}

/// The pixels the client's surface holds: BGRX, rows `width * 4` bytes
/// apart with no padding.
struct Shadow {
    size: Size,
    pixels: Vec<u8>,
}

impl Shadow {
    /// A shadow of `capture`, one of whose patches must cover the whole
    /// desktop. Every patch is copied in, in order.
    fn from_capture(capture: &Capture) -> Result<Self> {
        let size = capture.size();
        let full = Rectangle::full(size);
        ensure!(
            capture.patches().iter().any(|patch| patch.rect() == full),
            "the first capture of a {size} surface must be all of it"
        );
        let mut shadow = Self {
            size,
            pixels: vec![0; size.width as usize * size.height as usize * 4],
        };
        for patch in capture.patches() {
            shadow.copy_in(patch, patch.rect())?;
        }
        Ok(shadow)
    }

    fn row_bytes(&self) -> usize {
        self.size.width as usize * 4
    }

    /// Where in the buffer the `width` pixels from (`x`, `y`) are.
    fn span(&self, x: u32, y: u32, width: u32) -> std::ops::Range<usize> {
        let start = y as usize * self.row_bytes() + x as usize * 4;
        start..start + width as usize * 4
    }

    /// The tiles where `capture` differs from the shadow, changing
    /// nothing.
    fn compare(&self, capture: &Capture) -> Result<Region> {
        let mut changed = Region::empty(self.size);
        for patch in capture.patches() {
            for part in tile_parts(patch.rect()) {
                if self.differs(patch, part)? {
                    changed.add(part);
                }
            }
        }
        Ok(changed)
    }

    /// The tiles where `capture` differs from the shadow, copying each
    /// part that differs into it.
    fn apply(&mut self, capture: &Capture) -> Result<Region> {
        let mut changed = Region::empty(self.size);
        for patch in capture.patches() {
            for part in tile_parts(patch.rect()) {
                if self.differs(patch, part)? {
                    self.copy_in(patch, part)?;
                    changed.add(part);
                }
            }
        }
        Ok(changed)
    }

    /// Whether `part` of `patch` differs from the shadow, compared a row
    /// at a time.
    fn differs(&self, patch: &Patch, part: Rectangle) -> Result<bool> {
        for y in part.y..part.y + part.height {
            let theirs = patch_row(patch, part, y)?;
            let mine = self
                .pixels
                .get(self.span(part.x, y, part.width))
                .with_context(|| format!("{part} lies outside a {} shadow", self.size))?;
            if theirs != mine {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Copy `part` of `patch` into the shadow.
    fn copy_in(&mut self, patch: &Patch, part: Rectangle) -> Result<()> {
        let size = self.size;
        for y in part.y..part.y + part.height {
            let theirs = patch_row(patch, part, y)?;
            let span = self.span(part.x, y, part.width);
            self.pixels
                .get_mut(span)
                .with_context(|| format!("{part} lies outside a {size} shadow"))?
                .copy_from_slice(theirs);
        }
        Ok(())
    }

    /// `rect` of the shadow as an image to encode.
    fn image(&self, rect: Rectangle) -> Result<Bgrx<'_>> {
        ensure!(
            rect.lies_within(self.size),
            "{rect} does not lie within a {} surface",
            self.size
        );
        let start = self.span(rect.x, rect.y, rect.width).start;
        let end = self.span(rect.x, rect.y + rect.height - 1, rect.width).end;
        let data = self
            .pixels
            .get(start..end)
            .with_context(|| format!("{rect} lies outside a {} shadow", self.size))?;
        Bgrx::new(data, rect.width, rect.height, self.row_bytes())
            .with_context(|| format!("{rect} of a {} surface is not a valid image", self.size))
    }
}

/// The desktop's row `y` within `part` of `patch`, without padding.
fn patch_row(patch: &Patch, part: Rectangle, y: u32) -> Result<&[u8]> {
    let rect = patch.rect();
    let left = (part.x - rect.x) as usize * 4;
    patch
        .pixels()
        .row(y - rect.y)
        .and_then(|row| row.get(left..left + part.width as usize * 4))
        .with_context(|| format!("{part} lies outside a patch of {rect}"))
}

/// The parts of `rect` within each [`TILE`] x [`TILE`] tile it touches,
/// top first and then left first.
fn tile_parts(rect: Rectangle) -> impl Iterator<Item = Rectangle> {
    let (right, bottom) = (rect.right(), rect.bottom());
    let tile = u64::from(TILE);
    // Both ranges are empty for an empty rectangle.
    let cols = u64::from(rect.x) / tile..right.div_ceil(tile);
    let rows = u64::from(rect.y) / tile..bottom.div_ceil(tile);
    rows.flat_map(move |row| {
        cols.clone().map(move |col| {
            let left = (col * tile).max(u64::from(rect.x));
            let top = (row * tile).max(u64::from(rect.y));
            let end = ((col + 1) * tile).min(right);
            let foot = ((row + 1) * tile).min(bottom);
            // Each lies within the rectangle, so fits a u32.
            Rectangle::new(
                left as u32,
                top as u32,
                (end - left) as u32,
                (foot - top) as u32,
            )
        })
    })
}

/// `rect` in pieces of whole rows, top first, each at most `max_bytes`
/// raw, or one row a piece if a row is larger. When a piece may be at
/// least [`TILE`] rows, its height is a whole number of tiles, so that a
/// tile-aligned rectangle splits into tile-aligned pieces and no tile is
/// sent half in one piece and half in another.
fn split(rect: Rectangle, max_bytes: usize) -> impl Iterator<Item = Rectangle> {
    let row_bytes = (rect.width as usize * 4).max(1);
    let mut rows = u32::try_from(max_bytes / row_bytes)
        .unwrap_or(u32::MAX)
        .max(1);
    if rows >= TILE {
        rows -= rows % TILE;
    }
    (0..rect.height.div_ceil(rows)).map(move |index| {
        // index * rows < height, so neither overflows.
        let top = index * rows;
        Rectangle::new(
            rect.x,
            rect.y + top,
            rect.width,
            rows.min(rect.height - top),
        )
    })
}

/// A rectangle in SPICE's wire form.
fn wire_rect(rect: Rectangle) -> Result<Rect> {
    let as_i32 = |v: u64| i32::try_from(v).context("a coordinate does not fit in an i32");
    Ok(Rect {
        top: as_i32(u64::from(rect.y))?,
        left: as_i32(u64::from(rect.x))?,
        bottom: as_i32(rect.bottom())?,
        right: as_i32(rect.right())?,
    })
}

/// Whether a `width` x `height` image whose LZ4 encoding is `lz4_len`
/// bytes looks photographic enough to try as JPEG: large enough, and
/// LZ4 leaves more than [`JPEG_MIN_LZ4_PERCENT`] of its raw bytes.
pub fn looks_photographic(width: u32, height: u32, lz4_len: usize) -> bool {
    let area = u64::from(width) * u64::from(height);
    let raw_len = area * 4;
    width.min(height) >= JPEG_MIN_SIDE
        && area >= JPEG_MIN_AREA
        && (lz4_len as u64).saturating_mul(100) > raw_len * JPEG_MIN_LZ4_PERCENT
}

/// Choose how to encode `image`, and encode it.
///
/// - JPEG if `options` allow it, the image [`looks_photographic`], and
///   the JPEG is at most [`JPEG_MAX_PERCENT`] of what lossless would
///   send. The LZ4 the classifier needs is made whether or not LZ4 may
///   be sent.
/// - Otherwise LZ4 if `options` allow it and it comes out smaller than
///   the raw pixels.
/// - Otherwise a top-down 32-bit `BITMAP` with no row padding.
fn choose_encoding(image: &Bgrx<'_>, options: EncodeOptions) -> Result<(Encoding, ImagePayload)> {
    let row_bytes = image.width() as usize * 4;
    let raw_len = row_bytes * image.height() as usize;
    let lz4 = (options.lz4 || options.jpeg.is_some()).then(|| encode_spice_lz4(image));
    if let (Some(quality), Some(lz4)) = (options.jpeg, &lz4)
        && looks_photographic(image.width(), image.height(), lz4.len())
    {
        let lossless_len = if options.lz4 {
            lz4.len().min(raw_len)
        } else {
            raw_len
        };
        let data = encode_spice_jpeg(image, quality.get())
            .with_context(|| format!("encoding a {}x{} JPEG", image.width(), image.height()))?;
        if data.len() as u64 * 100 <= lossless_len as u64 * JPEG_MAX_PERCENT {
            return Ok((Encoding::Jpeg, ImagePayload::Jpeg(BinaryData { data })));
        }
    }
    if options.lz4
        && let Some(data) = lz4
        && data.len() < raw_len
    {
        return Ok((Encoding::Lz4, ImagePayload::Lz4(BinaryData { data })));
    }
    let mut data = Vec::with_capacity(raw_len);
    for row in image
        .data()
        .chunks(image.stride())
        .take(image.height() as usize)
    {
        data.extend_from_slice(row.get(..row_bytes).context("an image row is short")?);
    }
    let header = BitmapHeader {
        format: bitmap_fmt::BIT32,
        flags: bitmap_flags::TOP_DOWN,
        x: image.width(),
        y: image.height(),
        stride: u32::try_from(row_bytes).context("an image row is too wide")?,
        palette: BitmapPalette::None,
    };
    Ok((
        Encoding::Bitmap,
        ImagePayload::Bitmap(BitmapPayload { header, data }),
    ))
}

/// The `DRAW_COPY` that puts `rect` of `shadow` on the surface, its image
/// carrying id `image_id`.
fn encode_draw(
    shadow: &Shadow,
    rect: Rectangle,
    image_id: u64,
    options: EncodeOptions,
) -> Result<Draw> {
    let image = shadow.image(rect)?;
    let (encoding, payload) = choose_encoding(&image, options)?;
    let image = SpiceImage {
        descriptor: ImageDescriptor {
            image_id,
            image_type: encoding.image_type() as u8,
            flags: 0,
            width: rect.width,
            height: rect.height,
        },
        payload,
    };
    let base = DrawBase {
        surface_id: SURFACE_ID,
        bbox: wire_rect(rect)?,
        clip: Clip::none(),
    };
    let src_area = wire_rect(Rectangle::new(0, 0, rect.width, rect.height))?;
    let body = DrawCopyBuilder::new(&base, &image, src_area)
        .rop_descriptor(ropd::OP_PUT)
        .build();
    Ok(Draw {
        rect,
        encoding,
        body,
    })
}

/// The tiles of a surface whose pixels the client may hold only
/// approximately, because they were last sent lossily, each with the time
/// it is due to be sent again losslessly.
///
/// A tile becomes lossy when a lossy draw touches it, due `delay` later.
/// Damage that touches a lossy tile postpones it to `delay` after the
/// damage. A tile lying wholly within a lossless draw, or in a region a
/// refresh sent, is exact again. Each tile keeps its own time, so an area
/// that keeps changing does not hold back the refresh of one that has
/// stopped.
#[derive(Debug, Clone)]
pub struct LossyTiles {
    size: Size,
    columns: u32,
    delay: Duration,
    /// One entry a tile, row by row: when it is due, or `None` if it is
    /// exact.
    due: Vec<Option<Instant>>,
}

impl LossyTiles {
    /// No lossy tiles, and no surface until [`LossyTiles::reset`] gives
    /// one; a tile is refreshed once it has been still for `delay`.
    pub fn new(delay: Duration) -> Self {
        Self {
            size: Size {
                width: 0,
                height: 0,
            },
            columns: 0,
            delay,
            due: Vec::new(),
        }
    }

    /// Forget every lossy tile, for a new surface of `size`.
    pub fn reset(&mut self, size: Size) {
        let region = Region::empty(size);
        let (columns, rows) = region.grid();
        self.size = size;
        self.columns = columns;
        self.due = vec![None; columns as usize * rows as usize];
    }

    /// The size of the surface the tiles belong to.
    pub fn size(&self) -> Size {
        self.size
    }

    /// Whether every tile is exact.
    pub fn is_empty(&self) -> bool {
        self.due.iter().all(Option::is_none)
    }

    /// The lossy tiles, as a region.
    pub fn region(&self) -> Region {
        self.region_where(|_| true)
    }

    /// When the earliest lossy tile is due, if there is one.
    pub fn next_due(&self) -> Option<Instant> {
        self.due.iter().flatten().min().copied()
    }

    /// The lossy tiles due by `now`, as a region. They stay lossy until
    /// [`LossyTiles::clear`].
    pub fn due(&self, now: Instant) -> Region {
        self.region_where(|due| due <= now)
    }

    /// Postpone every lossy tile `damage` touches to `delay` after `now`.
    /// Damage of a desktop of another size touches nothing: the surface is
    /// about to be replaced.
    pub fn touch(&mut self, damage: &Region, now: Instant) {
        if damage.size() != self.size {
            return;
        }
        let later = now + self.delay;
        for (index, due) in self.due.iter_mut().enumerate() {
            let (col, row) = tile_at(index, self.columns);
            if due.is_some() && damage.contains_tile(col, row) {
                *due = Some(later);
            }
        }
    }

    /// Record what `draws`, sent at `now`, did to the client: each tile a
    /// lossless draw covers wholly is exact, and then each tile a lossy
    /// draw touches is lossy, due `delay` after `now`.
    pub fn record(&mut self, draws: &[Draw], now: Instant) {
        for draw in draws.iter().filter(|draw| !draw.is_lossy()) {
            for (index, tile) in self.tiles_of(draw.rect) {
                if tile_within(tile, draw.rect)
                    && let Some(due) = self.due.get_mut(index)
                {
                    *due = None;
                }
            }
        }
        let later = now + self.delay;
        for draw in draws.iter().filter(|draw| draw.is_lossy()) {
            for (index, _) in self.tiles_of(draw.rect) {
                if let Some(due) = self.due.get_mut(index) {
                    *due = Some(later);
                }
            }
        }
    }

    /// Mark every tile of `region` exact, as once a refresh has sent it
    /// losslessly. A region of another size clears nothing.
    pub fn clear(&mut self, region: &Region) {
        if region.size() != self.size {
            return;
        }
        for (index, due) in self.due.iter_mut().enumerate() {
            let (col, row) = tile_at(index, self.columns);
            if region.contains_tile(col, row) {
                *due = None;
            }
        }
    }

    /// The lossy tiles whose time satisfies `keep`, as a region.
    fn region_where(&self, keep: impl Fn(Instant) -> bool) -> Region {
        let mut region = Region::empty(self.size);
        for (index, due) in self.due.iter().enumerate() {
            if due.is_some_and(&keep) {
                let (col, row) = tile_at(index, self.columns);
                region.add(Rectangle::new(col * TILE, row * TILE, TILE, TILE));
            }
        }
        region
    }

    /// The index and rectangle, clipped to the surface, of each tile
    /// `rect` touches.
    fn tiles_of(&self, rect: Rectangle) -> Vec<(usize, Rectangle)> {
        let mut region = Region::empty(self.size);
        region.add(rect);
        let (columns, rows) = region.grid();
        let mut tiles = Vec::new();
        for row in 0..rows {
            for col in 0..columns {
                if region.contains_tile(col, row)
                    && let Some(tile) =
                        Rectangle::new(col * TILE, row * TILE, TILE, TILE).clip(self.size)
                {
                    tiles.push((row as usize * columns as usize + col as usize, tile));
                }
            }
        }
        tiles
    }
}

/// The column and row of tile `index` of a grid `columns` tiles wide.
fn tile_at(index: usize, columns: u32) -> (u32, u32) {
    let columns = columns.max(1) as usize;
    // Both are below the grid's sides, which are u32s.
    ((index % columns) as u32, (index / columns) as u32)
}

/// Whether `tile` lies wholly within `rect`.
fn tile_within(tile: Rectangle, rect: Rectangle) -> bool {
    tile.x >= rect.x
        && tile.y >= rect.y
        && tile.right() <= rect.right()
        && tile.bottom() <= rect.bottom()
}

/// The pixels of `region`'s tiles, clipped to its desktop.
fn area(region: &Region) -> u64 {
    region
        .rectangles(usize::MAX)
        .iter()
        .map(Rectangle::area)
        .sum()
}

/// Turns captures into updates for one display channel's surface: it
/// keeps the shadow, and numbers the images.
pub struct UpdateEncoder {
    shadow: Option<Shadow>,
    next_image_id: u64,
}

impl Default for UpdateEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for UpdateEncoder {
    // The shadow's pixels would swamp any log or assertion message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateEncoder")
            .field("shadow", &self.size())
            .field("next_image_id", &self.next_image_id)
            .finish()
    }
}

impl UpdateEncoder {
    /// An encoder with no shadow, whose first image is id 1.
    pub fn new() -> Self {
        Self {
            shadow: None,
            next_image_id: 1,
        }
    }

    /// Drop the shadow, as for a new surface: the next capture must be
    /// complete, and is sent whole. Image ids carry on.
    pub fn reset(&mut self) {
        self.shadow = None;
    }

    /// The size of the surface the shadow holds, if there is one.
    pub fn size(&self) -> Option<Size> {
        self.shadow.as_ref().map(|shadow| shadow.size)
    }

    /// Bring the shadow up to date with `capture`, and encode the draws
    /// that bring the client up to date with it.
    ///
    /// Without a shadow, or with one of another size, `capture` must
    /// include all of the desktop in one patch: it becomes the shadow,
    /// and is sent whole. Otherwise only the tiles that differ are sent,
    /// and an update with no draws means that nothing changed.
    pub fn update(&mut self, capture: &Capture, options: EncodeOptions) -> Result<Update> {
        let captured_area = capture.patches().iter().map(|p| p.rect().area()).sum();
        let changed = match &mut self.shadow {
            Some(shadow) if shadow.size == capture.size() => shadow.apply(capture)?,
            _ => {
                // Free the old shadow before making the new one, and leave
                // none if the capture cannot make one.
                self.shadow = None;
                self.shadow = Some(Shadow::from_capture(capture)?);
                Region::full(capture.size())
            }
        };
        let changed_area = area(&changed);
        let draws = self.encode(&changed, options)?;
        Ok(Update {
            draws,
            captured_area,
            changed_area,
        })
    }

    /// The tiles where `capture` differs from the shadow, leaving the
    /// shadow as it is: for a check whose differences are to be captured
    /// again, and sent, as damage. Without a shadow of `capture`'s size,
    /// every tile differs.
    pub fn differences(&self, capture: &Capture) -> Result<Region> {
        match &self.shadow {
            Some(shadow) if shadow.size == capture.size() => shadow.compare(capture),
            _ => Ok(Region::full(capture.size())),
        }
    }

    /// Encode `region` from the shadow without capturing anything, as
    /// when an area sent lossily is sent again losslessly. The region is
    /// merged and split as an update's changed tiles are. Fails without a
    /// shadow of the region's size.
    pub fn refresh(&mut self, region: &Region, options: EncodeOptions) -> Result<Update> {
        let size = self.size().context("there is no surface to refresh")?;
        ensure!(
            region.size() == size,
            "a region of a {} desktop cannot refresh a {size} surface",
            region.size()
        );
        Ok(Update {
            draws: self.encode(region, options)?,
            captured_area: 0,
            changed_area: 0,
        })
    }

    /// The draws for `region` of the shadow: merged, split and encoded.
    fn encode(&mut self, region: &Region, options: EncodeOptions) -> Result<Vec<Draw>> {
        let Self {
            shadow,
            next_image_id,
        } = self;
        let shadow = shadow.as_ref().context("there is no shadow to encode")?;
        let mut draws = Vec::new();
        for rect in region.rectangles(MAX_RECTS_PER_FRAME) {
            for piece in split(rect, MAX_DRAW_BYTES) {
                let image_id = *next_image_id;
                *next_image_id = next_image_id.wrapping_add(1).max(1);
                draws.push(encode_draw(shadow, piece, image_id, options)?);
            }
        }
        Ok(draws)
    }
}

#[cfg(test)]
mod tests {
    use shakenfist_spice_compression::{JpegDecoder, JpegDecoderRsDecoder, decompress_spice_lz4};
    use shakenfist_spice_protocol::constants::clip_type;
    use shakenfist_spice_protocol::messages::{DrawCopy, WireType};

    use super::*;
    use crate::platform::synthetic::SyntheticPlatform;
    use crate::platform::{Frame, ScreenSource};

    /// 4 x 3 tiles, the last column 36 pixels wide and the last row 8
    /// rows tall.
    const SIZE: Size = Size {
        width: 228,
        height: 136,
    };

    /// Bytes of padding after each row of a test frame.
    const PADDING: usize = 12;

    const LZ4: EncodeOptions = EncodeOptions {
        lz4: true,
        jpeg: None,
    };
    const RAW: EncodeOptions = EncodeOptions {
        lz4: false,
        jpeg: None,
    };
    /// What `auto-glz` allows, at the default quality.
    const AUTO: EncodeOptions = EncodeOptions {
        lz4: true,
        jpeg: Some(JpegQuality::DEFAULT),
    };
    /// JPEG allowed, but not LZ4: a client without the LZ4 capability.
    const JPEG_ONLY: EncodeOptions = EncodeOptions {
        lz4: false,
        jpeg: Some(JpegQuality::DEFAULT),
    };

    fn size(width: u32, height: u32) -> Size {
        Size { width, height }
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rectangle {
        Rectangle::new(x, y, width, height)
    }

    /// The tile holding pixel (`x`, `y`), clipped to `size`.
    fn tile_of(size: Size, x: u32, y: u32) -> Rectangle {
        rect(x / TILE * TILE, y / TILE * TILE, TILE, TILE)
            .clip(size)
            .unwrap()
    }

    /// A small xorshift generator, so that tests repeat.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// A number below `n`.
        fn below(&mut self, n: u32) -> u32 {
            (self.next() % u64::from(n)) as u32
        }
    }

    /// A desktop the tests change and capture: BGRX with no padding.
    #[derive(Clone)]
    struct Desktop {
        size: Size,
        pixels: Vec<u8>,
    }

    impl Desktop {
        /// Soft diagonal stripes, which LZ4 shrinks.
        fn pattern(size: Size) -> Self {
            let mut pixels = Vec::with_capacity(size.width as usize * size.height as usize * 4);
            for y in 0..size.height {
                for x in 0..size.width {
                    let v = ((x + y) / 16 % 4) as u8 * 60;
                    pixels.extend_from_slice(&[v, 255 - v, v / 2, 0]);
                }
            }
            Self { size, pixels }
        }

        /// Something like a photograph: smooth gradients and slow waves,
        /// with a little grain, which LZ4 cannot shrink much and JPEG
        /// can.
        fn photo(size: Size) -> Self {
            let mut rng = Rng(0x5EED_0F0F_F070_0001);
            let mut pixels = Vec::with_capacity(size.width as usize * size.height as usize * 4);
            for y in 0..size.height {
                for x in 0..size.width {
                    let (fx, fy) = (f64::from(x), f64::from(y));
                    let wave = 40.0 * (fx / 23.0).sin() * (fy / 31.0).cos();
                    let mut pixel = [0; 4];
                    for (channel, base) in [(0, 60.0), (1, 110.0), (2, 160.0)] {
                        let grain = f64::from(rng.below(13)) - 6.0;
                        let value = base + fx * 0.2 + fy * 0.15 + wave + grain;
                        pixel[channel] = value.clamp(0.0, 255.0) as u8;
                    }
                    pixels.extend_from_slice(&pixel);
                }
            }
            Self { size, pixels }
        }

        /// Something like text: dark glyph strokes, one or two pixels
        /// wide, in 8 x 16 cells on a flat light background.
        fn text(size: Size) -> Self {
            let mut desktop = Self {
                size,
                pixels: [0xF0, 0xF0, 0xF0, 0].repeat(size.width as usize * size.height as usize),
            };
            let mut rng = Rng(0x7E47_7E47_7E47_7E47);
            let ink = [0x20, 0x20, 0x20, 0];
            for top in (0..size.height.saturating_sub(16)).step_by(16) {
                for left in (0..size.width.saturating_sub(8)).step_by(8) {
                    // Some cells are spaces.
                    if rng.below(5) == 0 {
                        continue;
                    }
                    for _ in 0..3 {
                        let stroke = if rng.below(2) == 0 {
                            rect(left + 1 + rng.below(5), top + 3, 1 + rng.below(2), 10)
                        } else {
                            rect(left + 1, top + 3 + rng.below(9), 6, 1 + rng.below(2))
                        };
                        desktop.fill(stroke, ink);
                    }
                }
            }
            desktop
        }

        /// Pseudo-random pixels, which LZ4 cannot shrink.
        fn noise(size: Size) -> Self {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            let pixels = (0..size.width * size.height * 4)
                .map(|_| rng.next() as u8)
                .collect();
            Self { size, pixels }
        }

        fn at(&self, x: u32, y: u32) -> usize {
            (y as usize * self.size.width as usize + x as usize) * 4
        }

        /// Paint `area`, clipped to the desktop, with `bgrx`.
        fn fill(&mut self, area: Rectangle, bgrx: [u8; 4]) {
            let area = area.clip(self.size).unwrap();
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    let at = self.at(x, y);
                    self.pixels[at..at + 4].copy_from_slice(&bgrx);
                }
            }
        }

        /// Invert pixel (`x`, `y`)'s colour, so that it surely changes.
        fn flip(&mut self, x: u32, y: u32) {
            let at = self.at(x, y);
            for byte in &mut self.pixels[at..at + 3] {
                *byte = !*byte;
            }
        }

        /// `area`'s pixels as a frame whose rows carry [`PADDING`] bytes
        /// of `pad`.
        fn frame(&self, area: Rectangle, pad: u8) -> Frame {
            let row_bytes = area.width as usize * 4;
            let mut data = Vec::new();
            for y in area.y..area.y + area.height {
                let at = self.at(area.x, y);
                data.extend_from_slice(&self.pixels[at..at + row_bytes]);
                data.extend_from_slice(&[pad; PADDING]);
            }
            Frame::new(area.width, area.height, row_bytes + PADDING, data).unwrap()
        }

        fn capture_all(&self) -> Capture {
            Capture::whole(self.frame(Rectangle::full(self.size), 0xAA)).unwrap()
        }

        /// A capture of `rects`, each clipped to the desktop.
        fn capture(&self, rects: &[Rectangle]) -> Capture {
            self.capture_padded(rects, 0xAA)
        }

        fn capture_padded(&self, rects: &[Rectangle], pad: u8) -> Capture {
            let patches = rects
                .iter()
                .filter_map(|r| r.clip(self.size))
                .map(|r| Patch::new(r, self.frame(r, pad)).unwrap())
                .collect();
            Capture::new(self.size, patches).unwrap()
        }
    }

    /// BGRX to RGBA with opaque alpha, as the LZ4 decoder produces.
    fn rgba(bgrx: &[u8]) -> Vec<u8> {
        bgrx.as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], 255])
            .collect()
    }

    /// What the client's surface holds, as RGBA, drawn from the draws it
    /// was sent.
    struct Client {
        size: Size,
        rgba: Vec<u8>,
    }

    impl Client {
        fn new(size: Size) -> Self {
            Self {
                size,
                rgba: vec![0; size.width as usize * size.height as usize * 4],
            }
        }

        /// Decode `draw` with ryll's reader and decoders, checking every
        /// field andris fixes, and put its image on the surface.
        fn apply(&mut self, draw: &Draw) {
            let decoded = DrawCopy::decode(&draw.body).unwrap();
            let r = draw.rect;
            let (w, h) = (r.width as i32, r.height as i32);
            assert!(
                r.lies_within(self.size),
                "{r} is off a {} surface",
                self.size
            );
            assert_eq!(decoded.base.surface_id, 0);
            assert_eq!(
                decoded.base.bbox,
                Rect {
                    top: r.y as i32,
                    left: r.x as i32,
                    bottom: r.y as i32 + h,
                    right: r.x as i32 + w,
                }
            );
            assert_eq!(decoded.base.clip.clip_type, clip_type::NONE);
            assert_eq!(
                decoded.src_area,
                Rect {
                    top: 0,
                    left: 0,
                    bottom: h,
                    right: w,
                }
            );
            assert_eq!(decoded.rop_descriptor, ropd::OP_PUT);
            assert_eq!(decoded.scale_mode, 0);
            assert_eq!(decoded.mask.flags, 0);
            assert!(decoded.mask.bitmap.is_none());

            let image = decoded.src_bitmap.unwrap();
            let descriptor = &image.descriptor;
            assert_eq!(descriptor.flags, 0, "no CACHE_ME");
            assert_eq!(
                (descriptor.width, descriptor.height),
                (r.width, r.height),
                "the descriptor is the rectangle's size"
            );
            assert_eq!(descriptor.image_type, draw.encoding.image_type() as u8);
            let pixels = match &image.payload {
                ImagePayload::Lz4(data) => {
                    assert_eq!(draw.encoding, Encoding::Lz4);
                    decompress_spice_lz4(&data.data, r.width as usize, r.height as usize)
                        .unwrap()
                        .pixels
                }
                ImagePayload::Jpeg(data) => {
                    assert_eq!(draw.encoding, Encoding::Jpeg);
                    let decoded = decode_jpeg(&data.data);
                    assert_eq!(
                        (decoded.width, decoded.height),
                        (r.width, r.height),
                        "the JPEG is the rectangle's size"
                    );
                    decoded.rgba
                }
                ImagePayload::Bitmap(bitmap) => {
                    assert_eq!(draw.encoding, Encoding::Bitmap);
                    assert_eq!(bitmap.header.format, bitmap_fmt::BIT32);
                    assert_eq!(bitmap.header.flags, bitmap_flags::TOP_DOWN);
                    assert_eq!((bitmap.header.x, bitmap.header.y), (r.width, r.height));
                    assert_eq!(bitmap.header.stride, r.width * 4);
                    assert_eq!(bitmap.header.palette, BitmapPalette::None);
                    rgba(&bitmap.data)
                }
                other => panic!("unexpected payload {other:?}"),
            };
            let row_bytes = r.width as usize * 4;
            for (row, y) in (r.y..r.y + r.height).enumerate() {
                let at = (y as usize * self.size.width as usize + r.x as usize) * 4;
                self.rgba[at..at + row_bytes]
                    .copy_from_slice(&pixels[row * row_bytes..(row + 1) * row_bytes]);
            }
        }

        fn apply_all(&mut self, update: &Update) {
            for draw in &update.draws {
                self.apply(draw);
            }
        }

        /// The largest difference between any channel of the surface and
        /// of `desktop`.
        fn worst_difference(&self, desktop: &Desktop) -> u8 {
            self.rgba
                .iter()
                .zip(rgba(&desktop.pixels))
                .map(|(&mine, theirs)| mine.abs_diff(theirs))
                .max()
                .unwrap_or(0)
        }

        /// Assert the surface shows exactly `desktop`.
        fn assert_shows(&self, desktop: &Desktop) {
            assert_eq!(self.size, desktop.size);
            let row_bytes = self.size.width as usize * 4;
            for y in 0..self.size.height as usize {
                let row = &desktop.pixels[y * row_bytes..(y + 1) * row_bytes];
                assert!(
                    self.rgba[y * row_bytes..(y + 1) * row_bytes] == rgba(row)[..],
                    "row {y} differs from the desktop"
                );
            }
        }
    }

    /// A JPEG, decoded with ryll's pure-Rust decoder, after checking that
    /// it starts and ends as a JPEG must.
    fn decode_jpeg(data: &[u8]) -> shakenfist_spice_compression::DecodedJpeg {
        assert!(data.starts_with(&[0xFF, 0xD8]), "no SOI marker");
        assert!(data.ends_with(&[0xFF, 0xD9]), "no EOI marker");
        JpegDecoderRsDecoder::new()
            .decode(data)
            .expect("the JPEG does not decode")
    }

    /// An encoder, and a client that has drawn `desktop`'s first,
    /// complete update.
    fn started(desktop: &Desktop) -> (UpdateEncoder, Client) {
        let mut encoder = UpdateEncoder::new();
        let mut client = Client::new(desktop.size);
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        client.apply_all(&update);
        client.assert_shows(desktop);
        (encoder, client)
    }

    fn rects(update: &Update) -> Vec<Rectangle> {
        update.draws.iter().map(|draw| draw.rect).collect()
    }

    #[test]
    fn the_first_update_is_complete() {
        let desktop = Desktop::pattern(SIZE);
        let mut encoder = UpdateEncoder::new();
        assert_eq!(encoder.size(), None);
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert_eq!(rects(&update), vec![Rectangle::full(SIZE)]);
        let area = Rectangle::full(SIZE).area();
        assert_eq!((update.captured_area, update.changed_area), (area, area));
        assert_eq!(update.raw_bytes(), area * 4);
        assert_eq!(update.encoded_bytes(), update.draws[0].body.len() as u64);
        assert_eq!(encoder.size(), Some(SIZE));
        let mut client = Client::new(SIZE);
        client.apply_all(&update);
        client.assert_shows(&desktop);
    }

    #[test]
    fn a_first_capture_that_is_not_complete_is_refused() {
        let desktop = Desktop::pattern(SIZE);
        let mut encoder = UpdateEncoder::new();
        let part = desktop.capture(&[rect(0, 0, 64, 64)]);
        let error = encoder.update(&part, LZ4).unwrap_err();
        assert!(error.to_string().contains("must be all of it"));
        assert_eq!(encoder.size(), None);
        // Complete, though in two patches, one of them whole.
        let capture = desktop.capture(&[rect(0, 0, 10, 10), Rectangle::full(SIZE)]);
        assert_eq!(encoder.update(&capture, LZ4).unwrap().draws.len(), 1);
    }

    #[test]
    fn an_unchanged_capture_sends_nothing() {
        let desktop = Desktop::pattern(SIZE);
        let (mut encoder, _) = started(&desktop);
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert!(update.is_empty());
        assert_eq!(update.changed_area, 0);
        assert_eq!(update.captured_area, Rectangle::full(SIZE).area());
        assert_eq!((update.raw_bytes(), update.encoded_bytes()), (0, 0));

        // Damage that changed nothing: a capture of part of the desktop.
        let part = rect(30, 40, 150, 90);
        let update = encoder.update(&desktop.capture(&[part]), LZ4).unwrap();
        assert!(update.is_empty());
        assert_eq!(update.captured_area, part.area());

        // A capture of nothing.
        let update = encoder.update(&desktop.capture(&[]), LZ4).unwrap();
        assert!(update.is_empty());
    }

    #[test]
    fn stride_padding_is_not_compared() {
        let desktop = Desktop::pattern(SIZE);
        let (mut encoder, _) = started(&desktop);
        let capture = desktop.capture_padded(&[Rectangle::full(SIZE)], 0x55);
        assert!(encoder.update(&capture, LZ4).unwrap().is_empty());
    }

    #[test]
    fn a_pixel_change_sends_only_its_tile() {
        let (w, h) = (SIZE.width, SIZE.height);
        let points = [
            (0, 0),
            (w - 1, 0),
            (0, h - 1),
            (w - 1, h - 1),
            (w / 2, 0),
            (w / 2, h - 1),
            (0, h / 2),
            (w - 1, h / 2),
            (w / 2, h / 2),
        ];
        for (x, y) in points {
            let tile = tile_of(SIZE, x, y);
            // Captured whole, as the one pixel, and as its tile.
            for captured in [Rectangle::full(SIZE), rect(x, y, 1, 1), tile] {
                let mut desktop = Desktop::pattern(SIZE);
                let (mut encoder, mut client) = started(&desktop);
                desktop.flip(x, y);
                let update = encoder.update(&desktop.capture(&[captured]), LZ4).unwrap();
                assert_eq!(
                    rects(&update),
                    vec![tile],
                    "a change at ({x}, {y}) captured as {captured}"
                );
                assert_eq!(update.changed_area, tile.area());
                client.apply_all(&update);
                client.assert_shows(&desktop);
            }
        }
    }

    #[test]
    fn adjacent_changed_tiles_merge() {
        let mut desktop = Desktop::pattern(SIZE);
        let (mut encoder, mut client) = started(&desktop);
        // Tiles (1, 0), (2, 0), (1, 1) and (2, 1), and (3, 1) beside
        // them, which has another span so does not merge down.
        for (x, y) in [(70, 10), (130, 60), (65, 100), (140, 70), (200, 70)] {
            desktop.flip(x, y);
        }
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert_eq!(
            rects(&update),
            vec![rect(64, 0, 128, 64), rect(64, 64, 164, 64)]
        );
        client.apply_all(&update);
        client.assert_shows(&desktop);
    }

    #[test]
    fn two_distant_changes_are_two_rectangles() {
        let size = size(1024, 768);
        let mut desktop = Desktop::pattern(size);
        let (mut encoder, mut client) = started(&desktop);
        desktop.flip(5, 5);
        desktop.flip(1000, 700);
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert_eq!(
            rects(&update),
            vec![rect(0, 0, 64, 64), rect(960, 640, 64, 64)]
        );
        assert_eq!(update.changed_area, 2 * 64 * 64);
        client.apply_all(&update);
        client.assert_shows(&desktop);
    }

    #[test]
    fn over_the_cap_tiles_collapse_and_still_reproduce_the_desktop() {
        // 16 x 12 tiles. Every other tile of every other tile row is 48
        // tiles, none adjacent, so over the cap.
        let size = size(1024, 768);
        let mut desktop = Desktop::pattern(size);
        let (mut encoder, mut client) = started(&desktop);
        let mut points = Vec::new();
        let mut expected = Region::empty(size);
        for row in (0..12).step_by(2) {
            for col in (0..16).step_by(2) {
                let point = rect(col * TILE + 3, row * TILE + 5, 1, 1);
                desktop.flip(point.x, point.y);
                expected.add(point);
                points.push(point);
            }
        }
        assert_eq!(expected.tile_count(), 48);

        // Captured as the changed pixels alone, so the collapsed spans
        // cover tiles that were neither changed nor captured. A change in
        // one of those, not captured, must not reach the client yet.
        let captured = desktop.clone();
        desktop.flip(64 + 7, 9);
        let update = encoder.update(&desktop.capture(&points), LZ4).unwrap();
        assert_eq!(rects(&update), expected.rectangles(MAX_RECTS_PER_FRAME));
        assert_eq!(update.draws.len(), 6, "one span per changed tile row");
        assert_eq!(update.changed_area, 48 * 64 * 64);
        assert_eq!(update.captured_area, 48);
        client.apply_all(&update);
        client.assert_shows(&captured);

        // A complete capture then finds the one change left.
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert_eq!(rects(&update), vec![rect(64, 0, 64, 64)]);
        client.apply_all(&update);
        client.assert_shows(&desktop);
    }

    #[test]
    fn split_cuts_whole_rows_within_the_limit() {
        let r = rect(3, 5, 10, 25);
        // 40 bytes a row: 10 rows a piece.
        assert_eq!(
            split(r, 400).collect::<Vec<_>>(),
            vec![rect(3, 5, 10, 10), rect(3, 15, 10, 10), rect(3, 25, 10, 5)]
        );
        assert_eq!(split(r, 1000).collect::<Vec<_>>(), vec![r], "exactly fits");
        assert_eq!(split(r, 999).count(), 2, "one byte over");
        assert_eq!(split(r, 10).count(), 25, "a row each when a row is over");
        assert_eq!(split(rect(0, 0, 10, 0), 400).count(), 0);
        // 100 rows fit, but pieces are whole tiles.
        assert_eq!(
            split(rect(0, 64, 1, 200), 400).collect::<Vec<_>>(),
            vec![
                rect(0, 64, 1, 64),
                rect(0, 128, 1, 64),
                rect(0, 192, 1, 64),
                rect(0, 256, 1, 8)
            ]
        );
    }

    #[test]
    fn a_rectangle_over_the_limit_is_split_by_rows() {
        // 16 KiB a row, so 256 rows a piece.
        let size = size(4096, 600);
        let desktop = Desktop::pattern(size);
        let mut encoder = UpdateEncoder::new();
        let update = encoder.update(&desktop.capture_all(), RAW).unwrap();
        assert_eq!(
            rects(&update),
            vec![
                rect(0, 0, 4096, 256),
                rect(0, 256, 4096, 256),
                rect(0, 512, 4096, 88),
            ]
        );
        for draw in &update.draws {
            assert!(draw.raw_bytes() <= MAX_DRAW_BYTES as u64);
            assert_eq!(draw.encoding, Encoding::Bitmap);
        }
        assert_eq!(update.raw_bytes(), Rectangle::full(size).area() * 4);
        let mut client = Client::new(size);
        client.apply_all(&update);
        client.assert_shows(&desktop);
    }

    #[test]
    fn a_partial_tile_capture_compares_only_what_it_covers() {
        let mut desktop = Desktop::pattern(SIZE);
        let (mut encoder, mut client) = started(&desktop);
        // A band of rows 30 to 99, which covers part of tile rows 0 and 1.
        // Tile (0, 0) changes within the band and above it; tile (2, 1)
        // changes only below it.
        let mut seen = desktop.clone();
        desktop.flip(10, 50);
        seen.flip(10, 50);
        desktop.flip(10, 10);
        desktop.flip(140, 120);
        let band = rect(0, 30, SIZE.width, 70);
        let update = encoder.update(&desktop.capture(&[band]), LZ4).unwrap();
        assert_eq!(rects(&update), vec![rect(0, 0, 64, 64)]);
        assert_eq!(update.captured_area, band.area());
        // The tile is sent whole, from the shadow, so the part outside the
        // band is what the client already had.
        client.apply_all(&update);
        client.assert_shows(&seen);

        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert_eq!(
            rects(&update),
            vec![rect(0, 0, 64, 64), rect(128, 64, 64, 64)]
        );
        client.apply_all(&update);
        client.assert_shows(&desktop);
    }

    #[test]
    fn any_sequence_of_captures_reproduces_the_desktop() {
        let mut rng = Rng(0x0123_4567_89AB_CDEF);
        let mut desktop = Desktop::pattern(SIZE);
        let (mut encoder, mut client) = started(&desktop);
        let colours = [[0, 0, 0, 0], [255, 255, 255, 0], [10, 200, 30, 0]];
        for step in 0..60 {
            let mut painted = Vec::new();
            for _ in 0..=rng.below(3) {
                let x = rng.below(SIZE.width);
                let y = rng.below(SIZE.height);
                let area = rect(x, y, 1 + rng.below(90), 1 + rng.below(70));
                desktop.fill(area, colours[rng.below(3) as usize]);
                painted.push(area);
            }
            // Capture everything, exactly what was painted, or what was
            // painted rounded out to tiles, as damage would be.
            let capture = match rng.below(3) {
                0 => desktop.capture_all(),
                1 => desktop.capture(&painted),
                _ => {
                    let mut region = Region::empty(SIZE);
                    for &area in &painted {
                        region.add(area);
                    }
                    desktop.capture(&region.rectangles(usize::MAX))
                }
            };
            let options = if rng.below(2) == 0 { LZ4 } else { RAW };
            let update = encoder.update(&capture, options).unwrap();
            assert!(update.draws.len() <= MAX_RECTS_PER_FRAME);
            assert!(update.draws.iter().all(|draw| !draw.is_lossy()));
            client.apply_all(&update);
            client.assert_shows(&desktop);
            assert!(
                encoder.update(&capture, options).unwrap().is_empty(),
                "step {step}: the same capture again changes nothing"
            );
        }
    }

    #[test]
    fn a_new_size_drops_the_shadow() {
        let desktop = Desktop::pattern(SIZE);
        let (mut encoder, _) = started(&desktop);

        let larger = Desktop::pattern(size(300, 200));
        let update = encoder.update(&larger.capture_all(), LZ4).unwrap();
        assert_eq!(rects(&update), vec![rect(0, 0, 300, 200)]);
        assert_eq!(encoder.size(), Some(larger.size));
        let mut client = Client::new(larger.size);
        client.apply_all(&update);
        client.assert_shows(&larger);

        // A capture of another size must be complete.
        let smaller = Desktop::pattern(size(100, 100));
        let part = smaller.capture(&[rect(0, 0, 10, 10)]);
        assert!(encoder.update(&part, LZ4).is_err());
        assert_eq!(encoder.size(), None, "the old shadow is gone");
        let update = encoder.update(&smaller.capture_all(), LZ4).unwrap();
        assert_eq!(rects(&update), vec![rect(0, 0, 100, 100)]);
    }

    #[test]
    fn reset_sends_the_next_capture_whole() {
        let desktop = Desktop::pattern(SIZE);
        let (mut encoder, _) = started(&desktop);
        encoder.reset();
        assert_eq!(encoder.size(), None);
        let part = desktop.capture(&[rect(0, 0, 1, 1)]);
        assert!(encoder.update(&part, LZ4).is_err());
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert_eq!(rects(&update), vec![Rectangle::full(SIZE)]);
    }

    #[test]
    fn lz4_only_when_allowed_and_smaller() {
        let desktop = Desktop::pattern(SIZE);
        let mut client = Client::new(SIZE);
        let update = UpdateEncoder::new()
            .update(&desktop.capture_all(), LZ4)
            .unwrap();
        assert_eq!(update.draws[0].encoding, Encoding::Lz4);
        assert!(update.encoded_bytes() < update.raw_bytes());
        client.apply_all(&update);
        client.assert_shows(&desktop);

        let update = UpdateEncoder::new()
            .update(&desktop.capture_all(), RAW)
            .unwrap();
        assert_eq!(update.draws[0].encoding, Encoding::Bitmap);
        client.apply_all(&update);
        client.assert_shows(&desktop);

        let noise = Desktop::noise(SIZE);
        let update = UpdateEncoder::new()
            .update(&noise.capture_all(), LZ4)
            .unwrap();
        assert_eq!(update.draws[0].encoding, Encoding::Bitmap);
        client.apply_all(&update);
        client.assert_shows(&noise);

        assert!(!Encoding::Lz4.is_lossy());
        assert!(!Encoding::Bitmap.is_lossy());
        assert!(Encoding::Jpeg.is_lossy());
    }

    /// The encodings of the draws of `desktop`'s first, complete update,
    /// encoded under `options`.
    fn first_encodings(desktop: &Desktop, options: EncodeOptions) -> Vec<Encoding> {
        UpdateEncoder::new()
            .update(&desktop.capture_all(), options)
            .unwrap()
            .draws
            .iter()
            .map(|draw| draw.encoding)
            .collect()
    }

    #[test]
    fn jpeg_quality_is_one_to_one_hundred() {
        assert_eq!(JpegQuality::new(0), None);
        assert_eq!(JpegQuality::new(1).map(JpegQuality::get), Some(1));
        assert_eq!(JpegQuality::new(100).map(JpegQuality::get), Some(100));
        assert_eq!(JpegQuality::new(101), None);
        assert_eq!(JpegQuality::DEFAULT.get(), 85);
        assert_eq!(AUTO.lossless(), LZ4);
    }

    #[test]
    fn a_photographic_rectangle_is_jpeg_when_allowed() {
        let desktop = Desktop::photo(size(160, 128));
        for options in [AUTO, JPEG_ONLY] {
            let update = UpdateEncoder::new()
                .update(&desktop.capture_all(), options)
                .unwrap();
            assert_eq!(rects(&update), vec![Rectangle::full(desktop.size)]);
            let draw = &update.draws[0];
            assert_eq!(draw.encoding, Encoding::Jpeg, "{options:?}");
            assert!(draw.is_lossy());
            // Far smaller than lossless would have been.
            assert!(draw.encoded_bytes() * 4 < draw.raw_bytes());

            // It decodes to the rectangle's size, and close to the desktop.
            let mut client = Client::new(desktop.size);
            client.apply_all(&update);
            let worst = client.worst_difference(&desktop);
            assert!(worst > 0, "a JPEG of this is not exact");
            assert!(worst < 64, "the JPEG is off by {worst}");
        }
    }

    #[test]
    fn a_photographic_rectangle_is_lossless_unless_allowed() {
        let desktop = Desktop::photo(size(160, 128));
        // lz4 is an explicit choice, OFF is RAW, and --jpeg-quality 0 is
        // LZ4 with no JPEG: none of them is ever JPEG.
        for options in [LZ4, RAW] {
            let update = UpdateEncoder::new()
                .update(&desktop.capture_all(), options)
                .unwrap();
            assert!(update.draws.iter().all(|draw| !draw.is_lossy()));
            let mut client = Client::new(desktop.size);
            client.apply_all(&update);
            client.assert_shows(&desktop);
        }
    }

    #[test]
    fn text_is_never_jpeg() {
        let desktop = Desktop::text(size(512, 384));
        for options in [AUTO, JPEG_ONLY] {
            let encodings = first_encodings(&desktop, options);
            assert!(
                !encodings.contains(&Encoding::Jpeg),
                "{options:?}: {encodings:?}"
            );
        }
        // Tile by tile too, as a frame of small changes would send it.
        let (mut encoder, mut client) = started(&desktop);
        let mut changed = desktop.clone();
        for row in 0..6 {
            for col in 0..8 {
                changed.flip(col * TILE + 9, row * TILE + 9);
            }
        }
        let update = encoder.update(&changed.capture_all(), AUTO).unwrap();
        assert!(update.draws.iter().all(|draw| !draw.is_lossy()));
        client.apply_all(&update);
        client.assert_shows(&changed);
    }

    #[test]
    fn small_rectangles_are_never_jpeg() {
        // Under a tile's area, or too thin, however photographic.
        for small in [size(63, 64), size(64, 63), size(400, 10), size(10, 400)] {
            let desktop = Desktop::photo(small);
            assert_eq!(
                first_encodings(&desktop, AUTO),
                vec![Encoding::Lz4],
                "{small}"
            );
        }
        // A tile clipped by the surface's edge: SIZE's last column is 36
        // pixels wide.
        let mut desktop = Desktop::photo(SIZE);
        let mut encoder = UpdateEncoder::new();
        encoder.update(&desktop.capture_all(), LZ4).unwrap();
        desktop.flip(SIZE.width - 1, 0);
        let update = encoder.update(&desktop.capture_all(), AUTO).unwrap();
        assert_eq!(rects(&update), vec![rect(192, 0, 36, 64)]);
        assert!(!update.draws[0].is_lossy());
        assert!(!looks_photographic(TILE - 1, TILE, usize::MAX));
        assert!(!looks_photographic(4096, JPEG_MIN_SIDE - 1, usize::MAX));
        assert!(looks_photographic(TILE, TILE, usize::MAX));
    }

    #[test]
    fn the_classifier_needs_lz4_to_save_less_than_half() {
        let raw = (TILE * TILE * 4) as usize;
        assert!(!looks_photographic(TILE, TILE, raw / 2));
        assert!(looks_photographic(TILE, TILE, raw / 2 + 1));
    }

    #[test]
    fn a_refresh_makes_a_jpeg_exact() {
        let desktop = Desktop::photo(size(256, 192));
        let mut encoder = UpdateEncoder::new();
        let mut client = Client::new(desktop.size);
        let update = encoder.update(&desktop.capture_all(), AUTO).unwrap();
        assert!(update.draws.iter().all(Draw::is_lossy));
        client.apply_all(&update);
        assert!(client.worst_difference(&desktop) > 0);

        let start = Instant::now();
        let delay = Duration::from_millis(500);
        let mut lossy = LossyTiles::new(delay);
        lossy.reset(desktop.size);
        lossy.record(&update.draws, start);
        assert_eq!(lossy.region(), Region::full(desktop.size));
        assert_eq!(lossy.next_due(), Some(start + delay));

        let due = lossy.due(start + delay);
        let update = encoder.refresh(&due, AUTO.lossless()).unwrap();
        assert!(update.draws.iter().all(|draw| !draw.is_lossy()));
        client.apply_all(&update);
        client.assert_shows(&desktop);
        lossy.clear(&due);
        assert!(lossy.is_empty());
    }

    /// A draw of `rect` encoded as `encoding`, with no body.
    fn draw(rect: Rectangle, encoding: Encoding) -> Draw {
        Draw {
            rect,
            encoding,
            body: Vec::new(),
        }
    }

    fn region(size: Size, rects: &[Rectangle]) -> Region {
        let mut region = Region::empty(size);
        for &r in rects {
            region.add(r);
        }
        region
    }

    #[test]
    fn lossy_tiles_fall_due_after_the_delay() {
        let delay = Duration::from_millis(500);
        let start = Instant::now();
        let mut lossy = LossyTiles::new(delay);
        assert!(lossy.is_empty());
        assert_eq!(lossy.next_due(), None);
        lossy.reset(SIZE);
        assert_eq!(lossy.size(), SIZE);

        // A lossy draw across four tiles, partly.
        lossy.record(&[draw(rect(60, 60, 10, 10), Encoding::Jpeg)], start);
        let four = region(SIZE, &[rect(0, 0, 128, 128)]);
        assert_eq!(lossy.region(), four);
        assert_eq!(lossy.next_due(), Some(start + delay));
        assert!(
            lossy
                .due(start + delay - Duration::from_millis(1))
                .is_empty()
        );
        assert_eq!(lossy.due(start + delay), four);

        // Damage on one of them postpones it alone.
        let later = start + Duration::from_millis(300);
        lossy.touch(&region(SIZE, &[rect(70, 70, 1, 1)]), later);
        assert_eq!(
            lossy.due(start + delay),
            region(SIZE, &[rect(0, 0, 128, 64), rect(0, 64, 64, 64)])
        );
        assert_eq!(lossy.due(later + delay), four);
        // Damage elsewhere touches nothing; so does damage of another size.
        lossy.touch(&region(SIZE, &[rect(200, 100, 1, 1)]), later);
        lossy.touch(&Region::full(size(64, 64)), later + delay);
        assert_eq!(lossy.region(), four);
        assert_eq!(lossy.next_due(), Some(start + delay));

        lossy.clear(&region(SIZE, &[rect(0, 0, 1, 1)]));
        assert_eq!(lossy.region().tile_count(), 3);
        lossy.reset(SIZE);
        assert!(lossy.is_empty());
    }

    #[test]
    fn lossless_draws_clear_only_the_tiles_they_cover() {
        let start = Instant::now();
        let mut lossy = LossyTiles::new(Duration::from_millis(500));
        lossy.reset(SIZE);
        lossy.record(&[draw(Rectangle::full(SIZE), Encoding::Jpeg)], start);
        assert_eq!(lossy.region(), Region::full(SIZE));

        // Tile (1, 0) wholly, and tile (2, 0) in part; the clipped tile
        // (3, 2) wholly, though it is 36 x 8.
        lossy.record(
            &[
                draw(rect(64, 0, 100, 64), Encoding::Lz4),
                draw(rect(192, 128, 36, 8), Encoding::Bitmap),
            ],
            start,
        );
        assert!(!lossy.region().contains_tile(1, 0));
        assert!(lossy.region().contains_tile(2, 0));
        assert!(!lossy.region().contains_tile(3, 2));
        assert_eq!(lossy.region().tile_count(), 10);

        // A frame with lossless and lossy draws on one tile leaves it
        // lossy; a lossy draw re-dates the tiles it touches.
        let later = start + Duration::from_millis(100);
        lossy.record(
            &[
                draw(rect(64, 0, 64, 64), Encoding::Jpeg),
                draw(rect(64, 0, 64, 64), Encoding::Lz4),
            ],
            later,
        );
        assert!(lossy.region().contains_tile(1, 0));
        assert_eq!(
            lossy.due(start + Duration::from_millis(500)).tile_count(),
            10
        );
    }

    /// The image id of each draw.
    fn image_ids(update: &Update) -> Vec<u64> {
        update
            .draws
            .iter()
            .map(|draw| {
                DrawCopy::decode(&draw.body)
                    .unwrap()
                    .src_bitmap
                    .unwrap()
                    .descriptor
                    .image_id
            })
            .collect()
    }

    #[test]
    fn image_ids_count_from_one_across_updates_and_resets() {
        let mut desktop = Desktop::pattern(size(1024, 768));
        let mut encoder = UpdateEncoder::new();
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert_eq!(image_ids(&update), vec![1]);
        desktop.flip(5, 5);
        desktop.flip(1000, 700);
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert_eq!(image_ids(&update), vec![2, 3]);
        encoder.reset();
        let update = encoder.update(&desktop.capture_all(), LZ4).unwrap();
        assert_eq!(image_ids(&update), vec![4]);
    }

    #[test]
    fn differences_leave_the_shadow_alone() {
        let mut desktop = Desktop::pattern(SIZE);
        let encoder = UpdateEncoder::new();
        assert_eq!(
            encoder
                .differences(&desktop.capture(&[rect(0, 0, 5, 5)]))
                .unwrap(),
            Region::full(SIZE),
            "without a shadow, everything differs"
        );

        let (mut encoder, mut client) = started(&desktop);
        desktop.flip(200, 130);
        let band = desktop.capture(&[rect(0, 100, SIZE.width, 36)]);
        let mut expected = Region::empty(SIZE);
        expected.add(rect(200, 130, 1, 1));
        assert_eq!(encoder.differences(&band).unwrap(), expected);
        assert_eq!(
            encoder.differences(&band).unwrap(),
            expected,
            "the shadow did not take the change"
        );
        let update = encoder.update(&band, LZ4).unwrap();
        assert_eq!(rects(&update), vec![rect(192, 128, 36, 8)]);
        client.apply_all(&update);
        client.assert_shows(&desktop);
        assert!(encoder.differences(&band).unwrap().is_empty());
    }

    #[test]
    fn refresh_sends_the_shadow_again() {
        let desktop = Desktop::pattern(SIZE);
        assert!(
            UpdateEncoder::new()
                .refresh(&Region::full(SIZE), LZ4)
                .is_err()
        );

        let (mut encoder, mut client) = started(&desktop);
        let mut region = Region::empty(SIZE);
        region.add(rect(70, 70, 100, 10));
        let update = encoder.refresh(&region, RAW).unwrap();
        assert_eq!(rects(&update), vec![rect(64, 64, 128, 64)]);
        assert_eq!((update.captured_area, update.changed_area), (0, 0));
        assert_eq!(update.draws[0].encoding, Encoding::Bitmap);
        client.apply_all(&update);
        client.assert_shows(&desktop);

        let other = Region::full(size(10, 10));
        assert!(encoder.refresh(&other, LZ4).is_err());
    }

    #[test]
    fn synthetic_damage_captures_send_only_damaged_tiles() {
        // The synthetic platform's own captures, padding and all, driven
        // by the damage its square reports as it moves.
        let platform = SyntheticPlatform::new(size(256, 200)).unwrap();
        let subscription = platform.subscribe_damage().unwrap();
        let mut encoder = UpdateEncoder::new();
        subscription.take().unwrap();
        let update = encoder
            .update(&platform.capture_all().unwrap(), LZ4)
            .unwrap();
        assert_eq!(update.draws.len(), 1);
        // Enough steps for the square to wrap around.
        for _ in 0..30 {
            platform.advance().unwrap();
            let damage = subscription.take().unwrap().rectangles(usize::MAX);
            let update = encoder
                .update(&platform.capture(&damage).unwrap(), LZ4)
                .unwrap();
            assert!(!update.is_empty());
            for draw in &update.draws {
                assert!(
                    damage.iter().any(|r| r.intersects(&draw.rect)),
                    "{} was not damaged",
                    draw.rect
                );
            }
        }
        let update = encoder
            .update(&platform.capture_all().unwrap(), LZ4)
            .unwrap();
        assert!(update.is_empty(), "the shadow holds the desktop");
    }
}
