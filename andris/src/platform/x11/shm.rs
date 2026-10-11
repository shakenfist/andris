//! MIT-SHM capture: `ShmGetImage` into a segment andris shares with the
//! X server, so the pixels do not cross the X socket.
//!
//! The segment is a memfd, passed to the server with `ShmAttachFd`
//! (MIT-SHM 1.2), so SysV IPC namespaces do not matter. It is sealed
//! against shrinking and growing, and mapped read-only on andris's side
//! with `memmap2`.
//!
//! This is andris's only `unsafe` code; the crate denies it everywhere
//! else. The mapping is sound because of one rule, which
//! [`Segment::capture`] keeps: the X server writes the segment only
//! while serving a `ShmGetImage`, and andris reads it only once the
//! reply to every `ShmGetImage` it has sent has arrived, copying the
//! pixels out at once. No borrow of the mapping outlives that copy.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result, anyhow, ensure};
use memmap2::Mmap;
use x11rb::connection::Connection;
use x11rb::protocol::shm;
use x11rb::protocol::xproto::{ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

use super::Fault;
use super::capture::x_rect;
use crate::platform::{Frame, Rectangle};

/// The bytes of a BGRX image of `rect`: rows of `width * 4` bytes with
/// no padding, as `ShmGetImage` writes a 32 bit `ZPixmap`. `None` if
/// that does not fit a `usize`.
pub(super) fn image_bytes(rect: &Rectangle) -> Option<usize> {
    usize::try_from(rect.area().checked_mul(4)?).ok()
}

/// Where one rectangle's image goes in the segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Slot {
    /// The rectangle's index in the list being captured.
    pub index: usize,
    /// Its first byte in the segment.
    pub offset: usize,
    /// Its image's length in bytes.
    pub len: usize,
}

/// Lay `rects` out in a segment of `capacity` bytes: one after another,
/// in order, starting a new batch whenever the next would not fit. A
/// batch's images never overlap, so its requests can all be in flight at
/// once. Fails if one image alone is larger than the segment.
pub(super) fn layout(rects: &[Rectangle], capacity: usize) -> Result<Vec<Vec<Slot>>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut offset = 0;
    for (index, rect) in rects.iter().enumerate() {
        let len = image_bytes(rect)
            .with_context(|| format!("an image of {rect} is too large to address"))?;
        ensure!(
            len <= capacity,
            "an image of {rect} is {len} bytes, larger than the {capacity} byte segment"
        );
        if capacity - offset < len {
            batches.push(std::mem::take(&mut batch));
            offset = 0;
        }
        batch.push(Slot { index, offset, len });
        offset += len;
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    Ok(batches)
}

/// Whether `fd` is a Unix domain socket, over which a file descriptor
/// can be passed. A TCP connection to the X server cannot carry one, and
/// x11rb's attempt to send one fails the whole connection, so andris
/// checks the connection it actually has: x11rb falls back to TCP on
/// `localhost` when a local display's socket cannot be reached.
pub(super) fn is_unix_socket(fd: BorrowedFd<'_>) -> bool {
    // getsockname on a socket that is not AF_UNIX makes std refuse the
    // address. The duplicate is closed again when the stream drops.
    fd.try_clone_to_owned()
        .map(UnixStream::from)
        .and_then(|stream| stream.local_addr())
        .is_ok()
}

/// Why andris will not try MIT-SHM, or `None` if it may: `allowed` is
/// whether the caller permits it, `shm_fd` whether the server has
/// MIT-SHM 1.2, and `unix` whether the connection can pass a file
/// descriptor. The reason completes "capturing with GetImage because".
pub(super) fn refusal(allowed: bool, shm_fd: bool, unix: bool) -> Option<&'static str> {
    if !allowed {
        Some("MIT-SHM was turned off")
    } else if !shm_fd {
        Some("the X server lacks MIT-SHM 1.2, which attaches a segment by file descriptor")
    } else if !unix {
        Some("the X display is reached over TCP, which cannot pass a file descriptor")
    } else {
        None
    }
}

/// A memfd of `len` bytes, sealed so that it can no longer shrink or
/// grow.
fn memfd(len: usize) -> Result<File> {
    // SAFETY: memfd_create reads only `name`, a NUL-terminated string
    // that lives for the whole call, and returns a new descriptor that
    // nothing else refers to, or -1.
    let fd = unsafe {
        libc::memfd_create(
            c"andris-capture".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("creating a memfd");
    }
    // SAFETY: `fd` was just returned by memfd_create, so it is open and
    // owned by nothing else; the File takes sole ownership and closes it.
    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    file.set_len(len as u64)
        .with_context(|| format!("sizing a memfd to {len} bytes"))?;
    // SAFETY: F_ADD_SEALS takes an int argument and touches no memory of
    // this process; the descriptor is open for as long as `file` lives.
    let sealed = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
        )
    };
    if sealed < 0 {
        return Err(io::Error::last_os_error()).context("sealing a memfd");
    }
    Ok(file)
}

/// A segment attached to the X server on one connection. It does not
/// own the connection, so the owner detaches it with [`Segment::detach`]
/// before dropping it; dropping it unmaps andris's side and closes the
/// memfd.
pub(super) struct Segment {
    id: shm::Seg,
    map: Mmap,
}

impl Segment {
    /// Create a `len` byte segment and attach it on `conn`. The attach is
    /// checked, so a server that refuses it fails here with
    /// [`Fault::Failed`] rather than at the first capture.
    pub(super) fn attach(conn: &RustConnection, len: usize) -> Result<Self, Fault> {
        let file = memfd(len)
            .context("creating the MIT-SHM segment")
            .map_err(Fault::Failed)?;
        // SAFETY: memmap2 requires that nothing truncate the file, or
        // write it while Rust holds a reference into the mapping.
        // - The memfd has no name in any filesystem, and is sealed
        //   against shrinking, so the mapping stays backed for its whole
        //   life.
        // - Its only other holder is the X server, which writes it only
        //   while serving a ShmGetImage that andris sent on `conn`.
        //   Segment::capture borrows the mapping only once the reply to
        //   every ShmGetImage it sent has arrived, and copies out before
        //   the borrow ends; no other code reaches `map`.
        let map = unsafe { Mmap::map(&file) }
            .context("mapping the MIT-SHM segment")
            .map_err(Fault::Failed)?;
        let id = conn.generate_id()?;
        // The server gets its own copy of the descriptor; ours closes when
        // `file` drops, and the mapping keeps the memory alive.
        let fd = file
            .try_clone()
            .context("duplicating the MIT-SHM segment's descriptor")
            .map_err(Fault::Failed)?;
        shm::attach_fd(conn, id, OwnedFd::from(fd), false)?.check()?;
        Ok(Self { id, map })
    }

    /// The segment's size in bytes.
    pub(super) fn len(&self) -> usize {
        self.map.len()
    }

    /// Ask the server to detach the segment. The request is not flushed
    /// or checked; the caller is finished with the segment either way.
    pub(super) fn detach(self, conn: &RustConnection) {
        // A failure means the connection has gone, which detaches it too.
        let _ = shm::detach(conn, self.id);
    }

    /// Capture each of `rects`, which must lie within `root`, with
    /// `ShmGetImage`. The requests of each batch [`layout`] makes are
    /// all sent before their replies are read, so a batch costs one
    /// round trip.
    pub(super) fn capture(
        &self,
        conn: &RustConnection,
        root: Window,
        rects: &[Rectangle],
    ) -> Result<Vec<Frame>, Fault> {
        let mut frames = Vec::with_capacity(rects.len());
        for batch in layout(rects, self.len()).map_err(Fault::Failed)? {
            // Every request's arguments first, so that once one request is
            // sent, only a lost connection stops the rest.
            let requests = batch
                .iter()
                .map(|slot| {
                    let offset = u32::try_from(slot.offset).map_err(|_| {
                        Fault::Failed(anyhow!("offset {} is past 4 GiB", slot.offset))
                    })?;
                    Ok((x_rect(&rects[slot.index])?, offset))
                })
                .collect::<Result<Vec<_>, Fault>>()?;
            let mut cookies = Vec::with_capacity(batch.len());
            for ((x, y, width, height), offset) in requests {
                cookies.push(shm::get_image(
                    conn,
                    root,
                    x,
                    y,
                    width,
                    height,
                    !0,
                    ImageFormat::Z_PIXMAP.into(),
                    self.id,
                    offset,
                )?);
            }
            // Wait for every reply, even after a failure, so that no
            // request of this batch is still being served when the
            // mapping is read, here or by a later capture.
            let mut failure = None;
            for (slot, cookie) in batch.iter().zip(cookies) {
                match cookie.reply() {
                    Ok(reply) if reply.size as usize == slot.len => {}
                    Ok(reply) => {
                        failure.get_or_insert(Fault::Failed(anyhow!(
                            "the X server wrote {} bytes for {}, not {}",
                            reply.size,
                            rects[slot.index],
                            slot.len
                        )));
                    }
                    Err(e) => match Fault::from(e) {
                        gone @ Fault::Gone(_) => return Err(gone),
                        failed => {
                            failure.get_or_insert(failed);
                        }
                    },
                }
            }
            if let Some(failure) = failure {
                return Err(failure);
            }
            for slot in &batch {
                frames.push(self.copy_out(&rects[slot.index], slot)?);
            }
        }
        Ok(frames)
    }

    /// Copy `slot`'s image of `rect` out of the mapping. Called only once
    /// the reply to the request that wrote it, and to every other request
    /// sent, has arrived; see the SAFETY comment in [`Segment::attach`].
    fn copy_out(&self, rect: &Rectangle, slot: &Slot) -> Result<Frame, Fault> {
        let bytes = slot
            .offset
            .checked_add(slot.len)
            .and_then(|end| self.map.get(slot.offset..end))
            .ok_or_else(|| Fault::Failed(anyhow!("{rect}'s image lies outside the segment")))?;
        Frame::new(
            rect.width,
            rect.height,
            rect.width as usize * 4,
            bytes.to_vec(),
        )
        .map_err(Fault::Failed)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsFd;

    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rectangle {
        Rectangle::new(x, y, width, height)
    }

    #[test]
    fn image_bytes_are_four_a_pixel_with_no_padding() {
        assert_eq!(image_bytes(&rect(5, 7, 3, 2)), Some(24));
        assert_eq!(image_bytes(&rect(0, 0, 0, 9)), Some(0));
        assert_eq!(image_bytes(&rect(0, 0, 2212, 1308)), Some(2212 * 1308 * 4));
        assert_eq!(image_bytes(&rect(0, 0, u32::MAX, u32::MAX)), None);
    }

    #[test]
    fn layout_packs_images_back_to_back() {
        // 16, 8 and 32 bytes.
        let rects = [rect(0, 0, 2, 2), rect(9, 9, 2, 1), rect(3, 3, 4, 2)];
        assert_eq!(
            layout(&rects, 56).unwrap(),
            vec![vec![
                Slot {
                    index: 0,
                    offset: 0,
                    len: 16
                },
                Slot {
                    index: 1,
                    offset: 16,
                    len: 8
                },
                Slot {
                    index: 2,
                    offset: 24,
                    len: 32
                },
            ]]
        );
    }

    #[test]
    fn layout_starts_a_new_batch_when_the_segment_is_full() {
        let rects = [rect(0, 0, 2, 2), rect(9, 9, 2, 1), rect(3, 3, 4, 2)];
        let batches = layout(&rects, 32).unwrap();
        let offsets: Vec<Vec<(usize, usize)>> = batches
            .iter()
            .map(|batch| batch.iter().map(|s| (s.index, s.offset)).collect())
            .collect();
        assert_eq!(offsets, vec![vec![(0, 0), (1, 16)], vec![(2, 0)]]);
        // Every image lies within the segment, and a batch's images do
        // not overlap.
        for batch in &batches {
            let mut end = 0;
            for slot in batch {
                assert!(slot.offset >= end);
                end = slot.offset + slot.len;
                assert!(end <= 32);
            }
        }
    }

    #[test]
    fn layout_refuses_an_image_larger_than_the_segment() {
        let message = layout(&[rect(0, 0, 4, 4)], 63).unwrap_err().to_string();
        assert!(
            message.contains("larger than the 63 byte segment"),
            "{message}"
        );
        assert!(layout(&[], 0).unwrap().is_empty());
    }

    #[test]
    fn a_whole_screen_fits_one_batch() {
        let screen = rect(0, 0, 1024, 768);
        let capacity = image_bytes(&screen).unwrap();
        // The tiles of a screen are disjoint, so they fit together.
        let tiles: Vec<Rectangle> = (0..12)
            .flat_map(|row| (0..16).map(move |col| rect(col * 64, row * 64, 64, 64)))
            .collect();
        assert_eq!(layout(&tiles, capacity).unwrap().len(), 1);
        assert_eq!(layout(&[screen], capacity).unwrap().len(), 1);
    }

    #[test]
    fn a_unix_socket_is_told_from_tcp() {
        let (left, _right) = UnixStream::pair().unwrap();
        assert!(is_unix_socket(left.as_fd()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        assert!(!is_unix_socket(tcp.as_fd()));
        assert!(!is_unix_socket(listener.as_fd()));
    }

    #[test]
    fn refusal_names_the_first_obstacle() {
        assert_eq!(refusal(true, true, true), None);
        assert!(refusal(false, true, true).unwrap().contains("turned off"));
        assert!(refusal(true, false, true).unwrap().contains("MIT-SHM 1.2"));
        assert!(refusal(true, true, false).unwrap().contains("TCP"));
    }

    #[test]
    fn a_memfd_is_sized_and_sealed() {
        let file = memfd(4096).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 4096);
        assert!(file.set_len(1024).is_err(), "sealed against shrinking");
        assert!(file.set_len(8192).is_err(), "sealed against growing");
    }
}
