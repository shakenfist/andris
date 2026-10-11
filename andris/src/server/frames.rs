//! Turning captured frames into `DRAW_COPY` bodies.
//!
//! A frame is cut into bands of [`BAND_ROWS`] rows, the last of which may
//! be shorter. Each band becomes one `DRAW_COPY` of surface 0: the band's
//! rectangle as the box, no clip, `OP_PUT`, no mask, and one image whose
//! descriptor is exactly the band's size. A band of a 16384-pixel-wide
//! desktop is 4 MiB raw, so every message stays far below kerbside's
//! 16 MiB cap, and the ack window bounds how much is in flight.
//!
//! After the first, complete frame, [`FrameEncoder`] sends only the bands
//! whose pixels differ from the frame it saw last. That is a byte
//! comparison of two captures, not damage tracking, which XDamage is to
//! replace.
//!
//! A band is LZ4 when the client may take LZ4 and the result is smaller
//! than the raw pixels, and an uncompressed top-down 32-bit `BITMAP`
//! otherwise. Image ids count from 1, and no image asks to be cached.
//!
//! This module knows nothing of sockets, so it is tested on its own.

use anyhow::{Context, Result, ensure};
use shakenfist_spice_compression::{Bgrx, encode_spice_lz4};
use shakenfist_spice_protocol::ImageType;
use shakenfist_spice_protocol::constants::{bitmap_flags, bitmap_fmt, ropd};
use shakenfist_spice_protocol::messages::{
    BinaryData, BitmapHeader, BitmapPalette, BitmapPayload, Clip, DrawBase, DrawCopyBuilder,
    ImageDescriptor, ImagePayload, Rect, SpiceImage,
};

use crate::platform::Frame;

/// Rows in a band. The last band of a frame may have fewer.
pub const BAND_ROWS: u32 = 64;

/// The surface every band is drawn on: the primary surface.
pub const SURFACE_ID: u32 = 0;

/// One horizontal strip of a frame: `rows` rows from row `top`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Band {
    pub top: u32,
    pub rows: u32,
}

/// The bands of a frame `height` rows tall, top first.
pub fn bands(height: u32) -> impl Iterator<Item = Band> {
    (0..height.div_ceil(BAND_ROWS)).map(move |index| {
        let top = index * BAND_ROWS;
        Band {
            top,
            rows: BAND_ROWS.min(height - top),
        }
    })
}

/// Whether `band` of `frame` differs from the same band of `previous`.
/// Rows are compared without their stride padding, which holds no
/// pixels. Frames of different sizes differ everywhere.
fn band_changed(previous: &Frame, frame: &Frame, band: Band) -> bool {
    if previous.size() != frame.size() {
        return true;
    }
    (band.top..band.top + band.rows).any(|y| previous.row(y) != frame.row(y))
}

/// The bands of `frame` to send: those that differ from `previous`, or
/// every band if there is no previous frame or it is another size.
pub fn changed_bands(previous: Option<&Frame>, frame: &Frame) -> Vec<Band> {
    bands(frame.height())
        .filter(|&band| previous.is_none_or(|previous| band_changed(previous, frame, band)))
        .collect()
}

/// A rectangle `width` wide over `band`, at `top` (0 for an image's own
/// coordinates, the band's top for the surface's).
fn band_rect(top: u32, width: u32, rows: u32) -> Result<Rect> {
    let as_i32 = |v: u32| i32::try_from(v).context("a coordinate does not fit in an i32");
    Ok(Rect {
        top: as_i32(top)?,
        left: 0,
        bottom: as_i32(top + rows)?,
        right: as_i32(width)?,
    })
}

/// Encode `band` of `frame` as an image with id `image_id`: LZ4 if `lz4`
/// is allowed and comes out smaller than the raw pixels, otherwise a
/// top-down 32-bit `BITMAP` with no row padding.
fn encode_image(frame: &Frame, band: Band, image_id: u64, lz4: bool) -> Result<SpiceImage> {
    let width = frame.width();
    let pixels = frame
        .band(band.top, band.rows)
        .with_context(|| format!("{band:?} lies outside a {} frame", frame.size()))?;
    let row_bytes = width as usize * 4;
    let raw_len = row_bytes * band.rows as usize;

    let compressed = if lz4 {
        let image = Bgrx::new(pixels, width, band.rows, frame.stride()).with_context(|| {
            format!("{band:?} of a {} frame is not a valid image", frame.size())
        })?;
        Some(encode_spice_lz4(&image)).filter(|data| data.len() < raw_len)
    } else {
        None
    };

    let (image_type, payload) = match compressed {
        Some(data) => (ImageType::Lz4, ImagePayload::Lz4(BinaryData { data })),
        None => {
            let mut data = Vec::with_capacity(raw_len);
            for y in band.top..band.top + band.rows {
                let row = frame
                    .row(y)
                    .with_context(|| format!("row {y} lies outside a {} frame", frame.size()))?;
                data.extend_from_slice(row);
            }
            let stride = u32::try_from(row_bytes).context("a band's row is too wide")?;
            let header = BitmapHeader {
                format: bitmap_fmt::BIT32,
                flags: bitmap_flags::TOP_DOWN,
                x: width,
                y: band.rows,
                stride,
                palette: BitmapPalette::None,
            };
            (
                ImageType::Pixmap,
                ImagePayload::Bitmap(BitmapPayload { header, data }),
            )
        }
    };
    Ok(SpiceImage {
        descriptor: ImageDescriptor {
            image_id,
            image_type: image_type as u8,
            flags: 0,
            width,
            height: band.rows,
        },
        payload,
    })
}

/// The `DRAW_COPY` body that puts `band` of `frame` on the surface, its
/// image carrying id `image_id`.
pub fn encode_band(frame: &Frame, band: Band, image_id: u64, lz4: bool) -> Result<Vec<u8>> {
    ensure!(
        band.rows > 0
            && band
                .top
                .checked_add(band.rows)
                .is_some_and(|end| end <= frame.height()),
        "{band:?} lies outside a {} frame",
        frame.size()
    );
    let image = encode_image(frame, band, image_id, lz4)?;
    let base = DrawBase {
        surface_id: SURFACE_ID,
        bbox: band_rect(band.top, frame.width(), band.rows)?,
        clip: Clip::none(),
    };
    let src_area = band_rect(0, frame.width(), band.rows)?;
    Ok(DrawCopyBuilder::new(&base, &image, src_area)
        .rop_descriptor(ropd::OP_PUT)
        .build())
}

/// Encodes a stream of frames for one display channel: it remembers the
/// last frame, to find what changed, and numbers the images.
#[derive(Debug)]
pub struct FrameEncoder {
    previous: Option<Frame>,
    next_image_id: u64,
}

impl Default for FrameEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameEncoder {
    pub fn new() -> Self {
        Self {
            previous: None,
            next_image_id: 1,
        }
    }

    /// The `DRAW_COPY` bodies for `frame`, top band first, and remember
    /// it as the last frame.
    ///
    /// With `complete`, every band is encoded. Otherwise only the bands
    /// that differ from the last frame are, or every band if there was
    /// none or it was another size. `lz4` says whether the client may be
    /// sent LZ4 images.
    pub fn encode(&mut self, frame: Frame, complete: bool, lz4: bool) -> Result<Vec<Vec<u8>>> {
        let previous = if complete {
            None
        } else {
            self.previous.as_ref()
        };
        let bands = changed_bands(previous, &frame);
        let mut bodies = Vec::with_capacity(bands.len());
        for band in bands {
            let image_id = self.next_image_id;
            self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
            bodies.push(encode_band(&frame, band, image_id, lz4)?);
        }
        self.previous = Some(frame);
        Ok(bodies)
    }
}

#[cfg(test)]
mod tests {
    use shakenfist_spice_compression::decompress_spice_lz4;
    use shakenfist_spice_protocol::constants::clip_type;
    use shakenfist_spice_protocol::messages::{DrawCopy, WireType};

    use super::*;
    use crate::platform::Size;
    use crate::platform::synthetic::{STRIDE_PADDING, SyntheticPlatform};

    const SIZE: Size = Size {
        width: 256,
        height: 200,
    };

    /// `band`'s pixels as BGRX rows with no padding.
    fn band_bgrx(frame: &Frame, band: Band) -> Vec<u8> {
        (band.top..band.top + band.rows)
            .flat_map(|y| frame.row(y).unwrap().to_vec())
            .collect()
    }

    /// BGRX to RGBA with opaque alpha, as the LZ4 decoder produces.
    fn bgrx_to_rgba(bgrx: &[u8]) -> Vec<u8> {
        bgrx.as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], 255])
            .collect()
    }

    /// Check `body` is the documented DRAW_COPY of `band` of `frame` and
    /// decodes to its pixels. Returns the image's type and id.
    fn check_draw(body: &[u8], frame: &Frame, band: Band) -> (u8, u64) {
        let draw = DrawCopy::decode(body).unwrap();
        let width = frame.width() as i32;
        let (top, rows) = (band.top as i32, band.rows as i32);
        assert_eq!(draw.base.surface_id, 0);
        assert_eq!(
            draw.base.bbox,
            Rect {
                top,
                left: 0,
                bottom: top + rows,
                right: width
            }
        );
        assert_eq!(draw.base.clip.clip_type, clip_type::NONE);
        assert_eq!(
            draw.src_area,
            Rect {
                top: 0,
                left: 0,
                bottom: rows,
                right: width
            }
        );
        assert_eq!(draw.rop_descriptor, ropd::OP_PUT);
        assert_eq!(draw.scale_mode, 0);
        assert_eq!(draw.mask.flags, 0);
        assert!(draw.mask.bitmap.is_none());

        let image = draw.src_bitmap.unwrap();
        let descriptor = &image.descriptor;
        assert_eq!(descriptor.flags, 0, "no CACHE_ME");
        assert_eq!(
            (descriptor.width, descriptor.height),
            (frame.width(), band.rows),
            "the descriptor is the band's size"
        );
        let expected = band_bgrx(frame, band);
        match &image.payload {
            ImagePayload::Lz4(data) => {
                let decoded = decompress_spice_lz4(
                    &data.data,
                    descriptor.width as usize,
                    descriptor.height as usize,
                )
                .unwrap();
                assert_eq!(decoded.pixels, bgrx_to_rgba(&expected));
            }
            ImagePayload::Bitmap(bitmap) => {
                assert_eq!(bitmap.header.format, bitmap_fmt::BIT32);
                assert_eq!(bitmap.header.flags, bitmap_flags::TOP_DOWN);
                assert_eq!(bitmap.header.x, frame.width());
                assert_eq!(bitmap.header.y, band.rows);
                assert_eq!(bitmap.header.stride, frame.width() * 4);
                assert_eq!(bitmap.header.palette, BitmapPalette::None);
                assert_eq!(bitmap.data, expected);
            }
            other => panic!("unexpected payload {other:?}"),
        }
        (descriptor.image_type, descriptor.image_id)
    }

    fn platform() -> std::sync::Arc<SyntheticPlatform> {
        SyntheticPlatform::new(SIZE).unwrap()
    }

    /// A frame of pseudo-random pixels, which LZ4 cannot shrink.
    fn noise(width: u32, height: u32) -> Frame {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let data = (0..width * height * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        Frame::new(width, height, width as usize * 4, data).unwrap()
    }

    #[test]
    fn bands_cover_the_frame_with_a_short_last_band() {
        assert_eq!(
            bands(200).collect::<Vec<_>>(),
            vec![
                Band { top: 0, rows: 64 },
                Band { top: 64, rows: 64 },
                Band { top: 128, rows: 64 },
                Band { top: 192, rows: 8 },
            ]
        );
        assert_eq!(bands(128).count(), 2);
        assert_eq!(bands(1).collect::<Vec<_>>(), vec![Band { top: 0, rows: 1 }]);
        assert_eq!(bands(0).count(), 0);
    }

    #[test]
    fn without_a_previous_frame_every_band_changed() {
        let frame = platform().desktop().unwrap();
        assert_eq!(changed_bands(None, &frame), bands(200).collect::<Vec<_>>());
    }

    #[test]
    fn an_unchanged_frame_has_no_changed_bands() {
        let platform = platform();
        let a = platform.desktop().unwrap();
        let b = platform.desktop().unwrap();
        assert!(changed_bands(Some(&a), &b).is_empty());
    }

    #[test]
    fn advance_changes_the_two_bands_the_square_crosses() {
        let platform = platform();
        let before = platform.desktop().unwrap();
        platform.advance().unwrap();
        let after = platform.desktop().unwrap();
        let tops: Vec<u32> = changed_bands(Some(&before), &after)
            .iter()
            .map(|band| band.top)
            .collect();
        assert_eq!(tops, vec![64, 128]);
    }

    #[test]
    fn a_size_change_changes_every_band() {
        let platform = platform();
        let before = platform.desktop().unwrap();
        platform
            .set_size(Size {
                width: 256,
                height: 128,
            })
            .unwrap();
        let after = platform.desktop().unwrap();
        assert_eq!(changed_bands(Some(&before), &after).len(), 2);
    }

    #[test]
    fn stride_padding_is_not_compared() {
        let row: Vec<u8> = (0..8).collect();
        let padded = |pad: u8| {
            let mut data = row.clone();
            data.extend_from_slice(&[pad; 4]);
            data.extend_from_slice(&row);
            Frame::new(2, 2, 12, data).unwrap()
        };
        assert!(changed_bands(Some(&padded(0)), &padded(0xFF)).is_empty());
    }

    #[test]
    fn lz4_bands_decode_to_the_frame() {
        let frame = platform().desktop().unwrap();
        assert_eq!(frame.stride(), 256 * 4 + STRIDE_PADDING);
        for (index, band) in bands(frame.height()).enumerate() {
            let body = encode_band(&frame, band, index as u64 + 1, true).unwrap();
            let (image_type, image_id) = check_draw(&body, &frame, band);
            assert_eq!(image_type, ImageType::Lz4 as u8);
            assert_eq!(image_id, index as u64 + 1);
        }
    }

    #[test]
    fn bitmap_bands_when_lz4_is_not_allowed() {
        let frame = platform().desktop().unwrap();
        for band in bands(frame.height()) {
            let body = encode_band(&frame, band, 7, false).unwrap();
            let (image_type, _) = check_draw(&body, &frame, band);
            assert_eq!(image_type, ImageType::Pixmap as u8);
        }
    }

    #[test]
    fn bitmap_bands_when_lz4_is_not_smaller() {
        let frame = noise(100, 70);
        for band in bands(frame.height()) {
            let body = encode_band(&frame, band, 1, true).unwrap();
            let (image_type, _) = check_draw(&body, &frame, band);
            assert_eq!(image_type, ImageType::Pixmap as u8);
        }
    }

    #[test]
    fn a_band_outside_the_frame_is_refused() {
        let frame = platform().desktop().unwrap();
        assert!(encode_band(&frame, Band { top: 192, rows: 9 }, 1, true).is_err());
        assert!(encode_band(&frame, Band { top: 0, rows: 0 }, 1, true).is_err());
        assert!(
            encode_band(
                &frame,
                Band {
                    top: u32::MAX,
                    rows: 2
                },
                1,
                true
            )
            .is_err()
        );
    }

    #[test]
    fn encoder_sends_changed_bands_with_increasing_ids() {
        let platform = platform();
        let mut encoder = FrameEncoder::new();

        let first = platform.desktop().unwrap();
        let bodies = encoder.encode(first.clone(), false, true).unwrap();
        assert_eq!(bodies.len(), 4, "the first frame is complete");
        let ids: Vec<u64> = bodies
            .iter()
            .zip(bands(first.height()))
            .map(|(body, band)| check_draw(body, &first, band).1)
            .collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);

        let same = platform.desktop().unwrap();
        assert!(encoder.encode(same, false, true).unwrap().is_empty());

        platform.advance().unwrap();
        let moved = platform.desktop().unwrap();
        let bodies = encoder.encode(moved.clone(), false, false).unwrap();
        assert_eq!(bodies.len(), 2);
        let changed = [Band { top: 64, rows: 64 }, Band { top: 128, rows: 64 }];
        let ids: Vec<u64> = bodies
            .iter()
            .zip(changed)
            .map(|(body, band)| check_draw(body, &moved, band).1)
            .collect();
        assert_eq!(ids, vec![5, 6]);

        let again = platform.desktop().unwrap();
        assert_eq!(
            encoder.encode(again, true, true).unwrap().len(),
            4,
            "a complete frame sends every band"
        );
    }
}
