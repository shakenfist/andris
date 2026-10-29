//! The screen qemu shares with `ScanoutMap`: a memfd andris maps
//! read-only and copies rows out of.
//!
//! qemu passes a memfd holding the whole screen from some offset on,
//! which need not be page aligned; `memmap2` maps from the page boundary
//! below it and hands out a pointer to the offset, rather than a slice.
//!
//! This, the X11 platform's MIT-SHM mapping (`platform/x11/shm.rs`) and
//! the libpulse bindings (`platform/pulse/ffi.rs`) are andris's only
//! `unsafe` code; the crate denies it everywhere else. Its trust
//! boundary is this: the screen is memory qemu and the guest's device
//! write while andris reads it, since a virtio-gpu guest draws into its
//! resources whenever it likes. So andris treats the bytes as untrusted
//! pixels, and never relies on their stability. It never makes a Rust
//! reference into the mapping: [`Mapping::rows`] copies bytes out
//! through raw pointers into memory andris owns, and only that copy is
//! ever read, so a concurrent writer can only tear the pixels, never
//! make the copy unsound.
//!
//! Reading a mapping past the end of its file faults, which would kill
//! andris, so [`Mapping::new`] maps a screen only once the file is known
//! to hold it. qemu does not seal its screens' memfds against shrinking,
//! so a qemu that truncated one later could still fault andris; the
//! guest cannot.

use std::fs::File;
use std::os::fd::OwnedFd;

use anyhow::{Context, Result, ensure};
use memmap2::{MmapOptions, MmapRaw};

/// Andris's read-only view of a screen qemu shared: `rows` rows of
/// `stride` bytes. It never hands out a reference into the memory, which
/// qemu and the guest write at any time; bytes leave it only as an owned
/// copy. See the module's documentation.
pub struct Mapping {
    map: MmapRaw,
    stride: usize,
    rows: usize,
}

impl Mapping {
    /// Map the `rows` rows of `stride` bytes that `fd` holds from
    /// `offset` on. Fails if the file is too small to hold them, or they
    /// are too large to address.
    pub fn new(fd: OwnedFd, offset: u32, rows: u32, stride: u32) -> Result<Self> {
        let len = u64::from(rows) * u64::from(stride);
        let needed = u64::from(offset) + len;
        let file = File::from(fd);
        let length = file
            .metadata()
            .context("cannot read the size of the screen's file")?
            .len();
        ensure!(
            length >= needed,
            "qemu shared a screen of {needed} bytes in a file of {length}"
        );
        let len = usize::try_from(len)
            .with_context(|| format!("a screen of {len} bytes is too large to map"))?;
        let map = MmapOptions::new()
            .offset(u64::from(offset))
            .len(len)
            .map_raw_read_only(&file)
            .context("cannot map the screen")?;
        Ok(Self {
            map,
            stride: stride as usize,
            rows: rows as usize,
        })
    }

    /// A copy of bytes `left..left + span` of each of the `count` rows
    /// from row `top`, packed `span` bytes a row; `None` unless they all
    /// lie within the screen. Others may change them during the copy,
    /// which can tear the copy but not make it unsound.
    pub fn rows(&self, top: usize, count: usize, left: usize, span: usize) -> Option<Vec<u8>> {
        if top.checked_add(count)? > self.rows || left.checked_add(span)? > self.stride {
            return None;
        }
        let len = count.checked_mul(span)?;
        let mut copy = Vec::<u8>::with_capacity(len);
        for row in 0..count {
            // Within the mapping: (top + row) < self.rows and
            // left + span <= self.stride, so the row's last byte lies
            // before rows * stride, the mapping's length.
            let start = (top + row) * self.stride + left;
            // SAFETY:
            // - Source: `start..start + span` lies within the mapping,
            //   checked above, and the mapping lives as long as `self`, so
            //   the source is valid for `span` bytes of reads. The file
            //   held the whole mapping when it was mapped (see `new`).
            // - Destination: `copy` was allocated with room for `len`
            //   bytes, of which this row's are `row * span..(row + 1) *
            //   span`, and nothing else can see it, so the two do not
            //   overlap. The rows together initialise all `len` bytes
            //   before `set_len` exposes them.
            // - Other writers: qemu and the guest may write these bytes at
            //   any time, even during the copy. No Rust reference ever
            //   claims they are immutable: they are read only through the
            //   raw source pointer, and any byte is a valid u8, so a
            //   concurrent write can only make the copy a mix of old and
            //   new pixels.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.map.as_ptr().add(start),
                    copy.as_mut_ptr().add(row * span),
                    span,
                );
            }
        }
        // SAFETY: every one of the `len` bytes was written by the loop
        // above, and `len` is within the capacity.
        unsafe {
            copy.set_len(len);
        }
        Some(copy)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;

    use super::*;

    /// A file of `len` bytes, each its own index modulo 251.
    fn file(len: usize) -> File {
        let file = tempfile::tempfile().unwrap();
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        file.write_all_at(&bytes, 0).unwrap();
        file
    }

    fn fd(file: &File) -> OwnedFd {
        OwnedFd::from(file.try_clone().unwrap())
    }

    #[test]
    fn a_screen_must_fit_its_file() {
        let screen = file(100);
        Mapping::new(fd(&screen), 0, 10, 10).unwrap();
        Mapping::new(fd(&screen), 4, 8, 12).unwrap();
        let refused = |offset, rows, stride| {
            format!(
                "{:#}",
                Mapping::new(fd(&screen), offset, rows, stride)
                    .err()
                    .unwrap()
            )
        };
        assert!(refused(0, 101, 1).contains("101 bytes in a file of 100"));
        assert!(refused(1, 10, 10).contains("101 bytes in a file of 100"));
        assert!(refused(u32::MAX, u32::MAX, u32::MAX).contains("in a file of 100"));
    }

    #[test]
    fn rows_are_copied_out_of_what_lies_within_the_screen() {
        // Four rows of 8 bytes.
        let map = Mapping::new(fd(&file(32)), 0, 4, 8).unwrap();
        assert_eq!(map.rows(0, 4, 0, 8), Some((0..32).collect::<Vec<u8>>()));
        assert_eq!(map.rows(1, 2, 2, 3), Some(vec![10, 11, 12, 18, 19, 20]));
        assert_eq!(map.rows(3, 1, 7, 1), Some(vec![31]));
        assert_eq!(map.rows(4, 0, 0, 8), Some(Vec::new()));
        assert_eq!(map.rows(0, 4, 8, 0), Some(Vec::new()));
        assert_eq!(map.rows(3, 2, 0, 8), None, "runs past the last row");
        assert_eq!(map.rows(5, 0, 0, 0), None, "starts past the last row");
        assert_eq!(map.rows(0, 1, 4, 5), None, "runs past the stride");
        assert_eq!(map.rows(0, 1, 9, 0), None, "starts past the stride");
        assert_eq!(map.rows(usize::MAX, 2, 0, 1), None, "overflows");
        assert_eq!(map.rows(0, 1, usize::MAX, 2), None, "overflows");
    }

    #[test]
    fn an_offset_need_not_be_page_aligned() {
        let screen = file(10_000);
        let map = Mapping::new(fd(&screen), 4099, 3, 100).unwrap();
        let expected: Vec<u8> = (4099 + 100..4099 + 200).map(|i| (i % 251) as u8).collect();
        assert_eq!(map.rows(1, 1, 0, 100), Some(expected));
        // A later write is seen by a later copy: the mapping is shared.
        screen.write_all_at(&[0xaa], 4099 + 205).unwrap();
        assert_eq!(map.rows(2, 1, 5, 1), Some(vec![0xaa]));
    }
}
