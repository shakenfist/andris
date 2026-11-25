//! The bound on a backend's input queue.
//!
//! Every backend that injects input hands events to a thread of its own
//! to deliver, and must refuse an event rather than let the queue grow
//! without limit: a client can send input faster than a desktop accepts
//! it, and the events are the client's to choose.
//!
//! One FIFO carries them, bounded by counting the events in it rather
//! than by a bounded channel. A second queue for releases, drained
//! first, would let a release overtake its own press still waiting, and
//! leave the key held. Lock-state reads are not counted: each caller
//! blocks for its answer, so there are never more than there are
//! callers.
//!
//! [`Backlog`] is that count. The X11, qemu and synthetic backends share
//! it and the two limits; each keeps its own channel and request type,
//! because what they send to their thread differs. Before this was
//! shared, the X11 and qemu backends held a copy each, with the same
//! constants and the same compare-exchange loop, and the synthetic
//! backend had neither, so no test through it could reach a full queue.

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};

/// How many events may wait to reach the desktop before
/// [`InputSink::inject`] refuses one.
///
/// [`InputSink::inject`]: super::InputSink::inject
pub const INPUT_QUEUE_LIMIT: usize = 8192;

/// How many further events [`InputSink::inject_release`] may queue past
/// [`INPUT_QUEUE_LIMIT`]. Ample for every key and button held at once,
/// so that a release andris owes the desktop is not refused by a flood.
///
/// [`InputSink::inject_release`]: super::InputSink::inject_release
pub const RELEASE_RESERVE: usize = 512;

/// The events waiting in one backend's input queue, counted.
///
/// An event is counted by [`Backlog::take`] when it is queued and
/// uncounted by [`Backlog::done`] when the input thread takes it, or
/// when it could not be sent. `what` names the backend, for the error a
/// full queue gives.
#[derive(Debug)]
pub struct Backlog {
    queued: AtomicUsize,
    what: &'static str,
}

impl Backlog {
    /// A queue with nothing in it, belonging to the `what` backend.
    pub const fn new(what: &'static str) -> Self {
        Self {
            queued: AtomicUsize::new(0),
            what,
        }
    }

    /// Count one more event, unless `limit` are already counted.
    ///
    /// The caller passes the limit, so that [`InputSink::inject`] and
    /// [`InputSink::inject_release`] differ only in what they pass.
    ///
    /// [`InputSink::inject`]: super::InputSink::inject
    /// [`InputSink::inject_release`]: super::InputSink::inject_release
    pub fn take(&self, limit: usize) -> Result<()> {
        let mut queued = self.queued.load(Ordering::Acquire);
        loop {
            if queued >= limit {
                bail!(
                    "the {} input queue is full: {limit} events are waiting to be injected",
                    self.what
                );
            }
            match self.queued.compare_exchange_weak(
                queued,
                queued + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(now) => queued = now,
            }
        }
    }

    /// Uncount one event.
    pub fn done(&self) {
        self.queued.fetch_sub(1, Ordering::AcqRel);
    }

    /// How many events are counted.
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_counts_up_to_the_limit_and_then_refuses() {
        let backlog = Backlog::new("test");
        for _ in 0..INPUT_QUEUE_LIMIT {
            backlog.take(INPUT_QUEUE_LIMIT).unwrap();
        }
        assert_eq!(backlog.queued(), INPUT_QUEUE_LIMIT);
        let error = backlog.take(INPUT_QUEUE_LIMIT).unwrap_err().to_string();
        assert!(error.contains("test input queue is full"), "{error}");
        assert!(error.contains("input queue is full"), "{error}");
    }

    #[test]
    fn a_higher_limit_leaves_room_past_a_full_queue() {
        let backlog = Backlog::new("test");
        for _ in 0..INPUT_QUEUE_LIMIT {
            backlog.take(INPUT_QUEUE_LIMIT).unwrap();
        }
        // The reserve is room past the limit, and bounded in its turn.
        let reserved = INPUT_QUEUE_LIMIT + RELEASE_RESERVE;
        for _ in 0..RELEASE_RESERVE {
            backlog.take(reserved).unwrap();
        }
        assert!(backlog.take(reserved).is_err());
    }

    #[test]
    fn done_makes_room_again() {
        let backlog = Backlog::new("test");
        backlog.take(1).unwrap();
        assert!(backlog.take(1).is_err());
        backlog.done();
        assert_eq!(backlog.queued(), 0);
        backlog.take(1).unwrap();
    }
}
