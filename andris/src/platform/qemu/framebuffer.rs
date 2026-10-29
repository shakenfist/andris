//! The qemu platform's copy of the guest's screen, and the pixel formats
//! qemu may describe it in.
//!
//! qemu pushes the screen as it changes, and andris pulls captures when
//! it is ready for them, so the platform keeps a [`Framebuffer`] of its
//! own: each update qemu sends is copied into it as it arrives, and
//! captures read from it. A capture therefore sees each update whole or
//! not at all. On the shared-memory path the rows are first copied out
//! of memory the guest may still be drawing into (by the `shm` module),
//! so a rectangle can hold a mix of two guest frames; the copy keeps that
//! to the moment it was taken, and the next update of the area replaces
//! it.
//!
//! qemu names formats with pixman's format codes. The framebuffer holds
//! BGRX, which is pixman's `x8r8g8b8` on a little-endian host and what
//! virtio-gpu and std-vga use in their 32-bit modes, so those copy
//! straight across. [`PixelFormat`] converts the other packed RGB
//! formats of 16, 24 and 32 bits a pixel, and refuses anything else.

use anyhow::{Context, Result, bail, ensure};

use crate::platform::{Capture, Frame, Patch, Rectangle, Size};

/// pixman's `PIXMAN_TYPE_ARGB`: channels packed from the least
/// significant bit as blue, green, red, then alpha.
const TYPE_ARGB: u32 = 2;
/// pixman's `PIXMAN_TYPE_ABGR`: red, green, blue, then alpha.
const TYPE_ABGR: u32 = 3;
/// pixman's `PIXMAN_TYPE_BGRA`: alpha, red, green, then blue.
const TYPE_BGRA: u32 = 8;
/// pixman's `PIXMAN_TYPE_RGBA`: alpha, blue, green, then red.
const TYPE_RGBA: u32 = 9;

/// pixman's `x8r8g8b8`: BGRX in memory on a little-endian host.
pub const X8R8G8B8: u32 = 0x2002_0888;
/// pixman's `a8r8g8b8`: BGRA in memory; andris ignores the alpha.
pub const A8R8G8B8: u32 = 0x2002_8888;

/// Where one channel sits in a pixel word, and how wide it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Channel {
    shift: u32,
    bits: u32,
}

impl Channel {
    /// This channel of `word`, scaled to 8 bits by repeating its top bits.
    fn of(self, word: u32) -> u8 {
        let value = (word >> self.shift) & ((1 << self.bits) - 1);
        // Channels are 1 to 8 bits wide (see PixelFormat::from_pixman), so
        // this fits a byte. Repeating the top bits maps all ones to 0xff.
        let mut scaled = value << (8 - self.bits);
        let mut filled = self.bits;
        while filled < 8 {
            scaled |= scaled >> filled;
            filled *= 2;
        }
        (scaled & 0xff) as u8
    }
}

/// A pixel format qemu described with a pixman format code, which andris
/// can convert to BGRX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    /// Bytes a pixel.
    bytes: usize,
    red: Channel,
    green: Channel,
    blue: Channel,
}

impl PixelFormat {
    /// The format pixman's `code` names. Fails, naming the code, for
    /// anything but packed RGB of 16, 24 or 32 bits a pixel with channels
    /// of 1 to 8 bits: indexed, greyscale and YUV formats among them.
    pub fn from_pixman(code: u32) -> Result<Self> {
        let bpp = code >> 24;
        let kind = (code >> 16) & 0xff;
        let (a, r, g, b) = (
            (code >> 12) & 0xf,
            (code >> 8) & 0xf,
            (code >> 4) & 0xf,
            code & 0xf,
        );
        let refused = || format!("qemu described the screen in pixman format {code:#010x}");
        ensure!(
            matches!(bpp, 16 | 24 | 32),
            "{}, of {bpp} bits a pixel; andris takes 16, 24 or 32",
            refused()
        );
        ensure!(
            [r, g, b].iter().all(|&bits| (1..=8).contains(&bits)) && a + r + g + b <= bpp,
            "{}, whose channels andris cannot read",
            refused()
        );
        let (red, green, blue) = match kind {
            TYPE_ARGB => (b + g, b, 0),
            TYPE_ABGR => (0, r, r + g),
            TYPE_BGRA => (bpp - b - g - r, bpp - b - g, bpp - b),
            TYPE_RGBA => (bpp - r, bpp - r - g, bpp - r - g - b),
            _ => bail!("{}, which is not packed RGB", refused()),
        };
        Ok(Self {
            bytes: bpp as usize / 8,
            red: Channel {
                shift: red,
                bits: r,
            },
            green: Channel {
                shift: green,
                bits: g,
            },
            blue: Channel {
                shift: blue,
                bits: b,
            },
        })
    }

    /// Bytes a pixel.
    pub fn bytes_per_pixel(&self) -> usize {
        self.bytes
    }

    /// Whether this is BGRX or BGRA in memory, which copies as it is.
    fn is_bgrx(&self) -> bool {
        self.bytes == 4
            && self.blue == (Channel { shift: 0, bits: 8 })
            && self.green == (Channel { shift: 8, bits: 8 })
            && self.red == (Channel { shift: 16, bits: 8 })
    }

    /// Convert one row of `src` pixels into BGRX in `dst`, which holds
    /// exactly as many pixels.
    fn convert_row(&self, src: &[u8], dst: &mut [u8]) {
        if self.is_bgrx() {
            dst.copy_from_slice(src);
            for pixel in dst.as_chunks_mut::<4>().0 {
                pixel[3] = 0;
            }
            return;
        }
        for (pixel, out) in src.chunks_exact(self.bytes).zip(dst.as_chunks_mut::<4>().0) {
            let word = pixel
                .iter()
                .rev()
                .fold(0u32, |word, &byte| (word << 8) | u32::from(byte));
            *out = [
                self.blue.of(word),
                self.green.of(word),
                self.red.of(word),
                0,
            ];
        }
    }
}

/// Pixels qemu sent, as it describes them: `stride` bytes from one row to
/// the next, in pixman format `format`.
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub format: u32,
}

/// The platform's copy of the guest's screen: BGRX, with no row padding.
#[derive(Debug, Clone)]
pub struct Framebuffer {
    size: Size,
    data: Vec<u8>,
}

impl Framebuffer {
    /// A black screen of `size`. Fails if andris refuses that size (see
    /// [`Size::check`]).
    pub fn new(size: Size) -> Result<Self> {
        size.check()?;
        Ok(Self {
            size,
            data: vec![0; size.width as usize * size.height as usize * 4],
        })
    }

    /// The screen's size.
    pub fn size(&self) -> Size {
        self.size
    }

    /// Replace the whole screen with `source`, a `size` image. Fails,
    /// leaving the screen as it was, if andris refuses the size or the
    /// format, or `source` is too short for the image.
    pub fn replace(&mut self, size: Size, source: Source<'_>) -> Result<()> {
        let mut next = Framebuffer::new(size)?;
        next.copy(Rectangle::full(size), source, (0, 0))?;
        *self = next;
        Ok(())
    }

    /// Copy `rect` of the screen from `source`, an image whose pixel
    /// `origin` is `rect`'s top left: (0, 0) unless the image holds more
    /// than the rectangle, as when qemu's rectangle started off the top
    /// or left of the screen and was cut. Returns the part of `rect` that
    /// lies on the screen, which may be empty.
    pub fn update_from_patch(
        &mut self,
        rect: Rectangle,
        source: Source<'_>,
        origin: (u32, u32),
    ) -> Result<Option<Rectangle>> {
        let Some(clipped) = rect.clip(self.size) else {
            return Ok(None);
        };
        // Clipping to the screen keeps the top left corner, so `origin`
        // still names its pixel.
        self.copy(clipped, source, origin)?;
        Ok(Some(clipped))
    }

    /// Copy `rect`, which lies on the screen, from `source`, whose pixel
    /// `origin` is the rectangle's top left pixel.
    fn copy(&mut self, rect: Rectangle, source: Source<'_>, origin: (u32, u32)) -> Result<()> {
        let format = PixelFormat::from_pixman(source.format)?;
        let src_row_bytes = rect.width as usize * format.bytes_per_pixel();
        let (left, top) = (origin.0 as usize, origin.1 as usize);
        let Some(last) = (top + rect.height as usize - 1)
            .checked_mul(source.stride)
            .and_then(|start| start.checked_add((left * format.bytes_per_pixel()) + src_row_bytes))
        else {
            bail!(
                "{rect} with a stride of {} bytes is too large to address",
                source.stride
            );
        };
        ensure!(
            source.data.len() >= last,
            "qemu sent {} bytes for {rect} with a stride of {} bytes, which needs {last}",
            source.data.len(),
            source.stride
        );
        let dst_stride = self.size.width as usize * 4;
        let dst_row_bytes = rect.width as usize * 4;
        for row in 0..rect.height as usize {
            let src_start = (top + row) * source.stride + left * format.bytes_per_pixel();
            let dst_start = (rect.y as usize + row) * dst_stride + rect.x as usize * 4;
            format.convert_row(
                &source.data[src_start..src_start + src_row_bytes],
                &mut self.data[dst_start..dst_start + dst_row_bytes],
            );
        }
        Ok(())
    }

    /// The screen's pixels in `rect`, which must lie on it.
    fn crop(&self, rect: Rectangle) -> Result<Frame> {
        let stride = self.size.width as usize * 4;
        let row_bytes = rect.width as usize * 4;
        let mut data = Vec::with_capacity(row_bytes * rect.height as usize);
        for y in rect.y as usize..(rect.y + rect.height) as usize {
            let start = y * stride + rect.x as usize * 4;
            data.extend_from_slice(&self.data[start..start + row_bytes]);
        }
        Frame::new(rect.width, rect.height, row_bytes, data)
            .with_context(|| format!("cropping {rect} from a {} screen", self.size))
    }

    /// `rects`, each clipped to the screen, as
    /// [`crate::platform::ScreenSource::capture`] returns them.
    pub fn capture(&self, rects: &[Rectangle]) -> Result<Capture> {
        let patches = rects
            .iter()
            .filter_map(|rect| rect.clip(self.size))
            .map(|rect| Patch::new(rect, self.crop(rect)?))
            .collect::<Result<_>>()?;
        Capture::new(self.size, patches)
    }

    /// The whole screen, as one patch.
    pub fn capture_all(&self) -> Result<Capture> {
        Capture::whole(self.crop(Rectangle::full(self.size))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Size = Size {
        width: 4,
        height: 3,
    };

    /// A `size` x8r8g8b8 image whose pixel (x, y) is BGRX (x, y, 7, 0xee),
    /// rows `stride` bytes apart with the padding set to 0xaa.
    fn image(size: Size, stride: usize) -> Vec<u8> {
        let mut data = vec![0xaa; stride * size.height as usize];
        for y in 0..size.height as usize {
            for x in 0..size.width as usize {
                let at = y * stride + x * 4;
                data[at..at + 4].copy_from_slice(&[x as u8, y as u8, 7, 0xee]);
            }
        }
        data
    }

    fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
        let row = frame.row(y).unwrap();
        let at = x as usize * 4;
        row[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn x8r8g8b8_copies_across_and_clears_the_unused_byte() {
        let mut fb = Framebuffer::new(SIZE).unwrap();
        let data = image(SIZE, 20);
        fb.replace(
            SIZE,
            Source {
                data: &data,
                stride: 20,
                format: X8R8G8B8,
            },
        )
        .unwrap();
        let whole = fb.capture_all().unwrap().into_whole().unwrap();
        assert_eq!(whole.stride(), 16, "the copy has no padding");
        assert_eq!(pixel(&whole, 3, 2), [3, 2, 7, 0]);
        assert_eq!(pixel(&whole, 0, 1), [0, 1, 7, 0]);
    }

    #[test]
    fn the_other_formats_convert_to_bgrx() {
        // Red 0xc0, green 0x80 and blue 0x40 in each 8-bit format, and
        // pure red in the narrow ones, as the bytes qemu sends.
        const MIXED: [u8; 4] = [0x40, 0x80, 0xc0, 0];
        const RED: [u8; 4] = [0, 0, 0xff, 0];
        let cases: &[(u32, &[u8], [u8; 4])] = &[
            (A8R8G8B8, &[0x40, 0x80, 0xc0, 0x80], MIXED),
            (0x2003_0888, &[0xc0, 0x80, 0x40, 0x00], MIXED), // x8b8g8r8
            (0x2008_0888, &[0x00, 0xc0, 0x80, 0x40], MIXED), // b8g8r8x8
            (0x2009_0888, &[0x00, 0x40, 0x80, 0xc0], MIXED), // r8g8b8x8
            (0x1802_0888, &[0x40, 0x80, 0xc0], MIXED),       // r8g8b8
            (0x1803_0888, &[0xc0, 0x80, 0x40], MIXED),       // b8g8r8
            (0x1002_0565, &[0x00, 0xf8], RED),               // r5g6b5
            (0x1002_0555, &[0x00, 0x7c], RED),               // x1r5g5b5
        ];
        let size = Size {
            width: 1,
            height: 1,
        };
        for &(format, bytes, expected) in cases {
            let mut fb = Framebuffer::new(size).unwrap();
            fb.replace(
                size,
                Source {
                    data: bytes,
                    stride: bytes.len(),
                    format,
                },
            )
            .unwrap();
            let whole = fb.capture_all().unwrap().into_whole().unwrap();
            assert_eq!(pixel(&whole, 0, 0), expected, "format {format:#010x}");
        }
    }

    #[test]
    fn narrow_channels_scale_to_the_full_range() {
        // r5g6b5 with green at its midpoint and blue at its smallest step.
        let word: u16 = (0b100000 << 5) | 0b00001;
        let format = PixelFormat::from_pixman(0x1002_0565).unwrap();
        let mut out = [0u8; 4];
        format.convert_row(&word.to_le_bytes(), &mut out);
        assert_eq!(out, [0b0000_1000, 0b1000_0010, 0, 0]);
    }

    #[test]
    fn unreadable_formats_are_refused_by_code() {
        for code in [
            0x0804_0000_u32, // c8, indexed
            0x2001_0800,     // a8, alpha only
            0x2002_0aaa,     // 10-bit channels: not 1 to 8
            0x0801_0332,     // r3g3b2: 8 bits a pixel
        ] {
            let error = PixelFormat::from_pixman(code).unwrap_err().to_string();
            assert!(error.contains(&format!("{code:#010x}")), "{error}");
        }
    }

    #[test]
    fn updates_from_a_screen_and_from_a_patch_land_in_place() {
        let mut fb = Framebuffer::new(SIZE).unwrap();
        let screen = image(SIZE, 16);
        let rect = Rectangle::new(1, 1, 2, 2);
        assert_eq!(
            fb.update_from_patch(
                rect,
                Source {
                    data: &screen,
                    stride: 16,
                    format: X8R8G8B8,
                },
                (1, 1),
            )
            .unwrap(),
            Some(rect)
        );
        let whole = fb.capture_all().unwrap().into_whole().unwrap();
        assert_eq!(pixel(&whole, 0, 0), [0, 0, 0, 0], "outside the rectangle");
        assert_eq!(pixel(&whole, 2, 2), [2, 2, 7, 0]);

        // A 2x1 patch with padding, landing at (2, 0).
        let patch = [9, 9, 9, 9, 8, 8, 8, 8, 0xaa, 0xaa];
        fb.update_from_patch(
            Rectangle::new(2, 0, 2, 1),
            Source {
                data: &patch,
                stride: 10,
                format: X8R8G8B8,
            },
            (0, 0),
        )
        .unwrap();
        let whole = fb.capture_all().unwrap().into_whole().unwrap();
        assert_eq!(pixel(&whole, 2, 0), [9, 9, 9, 0]);
        assert_eq!(pixel(&whole, 3, 0), [8, 8, 8, 0]);
    }

    #[test]
    fn updates_clip_to_the_screen_and_refuse_short_data() {
        let mut fb = Framebuffer::new(SIZE).unwrap();
        let screen = image(SIZE, 16);
        let source = Source {
            data: &screen,
            stride: 16,
            format: X8R8G8B8,
        };
        assert_eq!(
            fb.update_from_patch(Rectangle::new(3, 2, 5, 5), source, (3, 2))
                .unwrap(),
            Some(Rectangle::new(3, 2, 1, 1))
        );
        assert_eq!(
            fb.update_from_patch(Rectangle::new(9, 9, 1, 1), source, (0, 0))
                .unwrap(),
            None
        );
        // With padded rows, the last row needs only its pixels: 56 bytes
        // hold the screen, and 55 do not.
        let padded = image(SIZE, 20);
        let cut = |len: usize| Source {
            data: &padded[..len],
            stride: 20,
            format: X8R8G8B8,
        };
        assert!(
            fb.update_from_patch(Rectangle::full(SIZE), cut(56), (0, 0))
                .is_ok()
        );
        assert!(
            fb.update_from_patch(Rectangle::full(SIZE), cut(55), (0, 0))
                .is_err()
        );
        let unaddressable = Source {
            data: &screen,
            stride: usize::MAX,
            format: X8R8G8B8,
        };
        let error = fb
            .update_from_patch(Rectangle::full(SIZE), unaddressable, (0, 0))
            .unwrap_err();
        assert!(
            error.to_string().contains("too large to address"),
            "{error}"
        );
        let refused = Source {
            data: &screen,
            stride: 16,
            format: 0x0804_0000,
        };
        assert!(
            fb.update_from_patch(Rectangle::full(SIZE), refused, (0, 0))
                .is_err()
        );
    }

    #[test]
    fn a_refused_replacement_leaves_the_screen_as_it_was() {
        let mut fb = Framebuffer::new(SIZE).unwrap();
        let bigger = Size {
            width: 8,
            height: 8,
        };
        let short = vec![0; 16];
        assert!(
            fb.replace(
                bigger,
                Source {
                    data: &short,
                    stride: 32,
                    format: X8R8G8B8,
                },
            )
            .is_err()
        );
        assert_eq!(fb.size(), SIZE);
    }

    #[test]
    fn captures_clip_and_drop_rectangles() {
        let mut fb = Framebuffer::new(SIZE).unwrap();
        let screen = image(SIZE, 16);
        fb.replace(
            SIZE,
            Source {
                data: &screen,
                stride: 16,
                format: X8R8G8B8,
            },
        )
        .unwrap();
        let capture = fb
            .capture(&[Rectangle::new(2, 1, 9, 9), Rectangle::new(7, 7, 1, 1)])
            .unwrap();
        assert_eq!(capture.size(), SIZE);
        assert_eq!(capture.patches().len(), 1);
        let patch = &capture.patches()[0];
        assert_eq!(patch.rect(), Rectangle::new(2, 1, 2, 2));
        assert_eq!(pixel(patch.pixels(), 1, 1), [3, 2, 7, 0]);
    }
}
