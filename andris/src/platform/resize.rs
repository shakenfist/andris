//! Planning a resize, with no call into any desktop.
//!
//! What a display control does with a size it is asked for is the same
//! reasoning on every backend: bring the size within the range the
//! desktop allows, then decide whether that leaves anything to do. Only
//! the asking is backend-specific, so the deciding lives here, where a
//! test can reach it without a desktop and where the X11 and synthetic
//! backends cannot drift apart on it.
//!
//! The rule, which [`Resize`] states from the caller's side:
//!
//! - a size outside the range is clamped to it, and a resize to the
//!   clamped size is [`Resize::Applied`]. The desktop moved as far
//!   towards the request as it allows, and the client sees the new size
//!   as it sees any other;
//! - when the clamped size is the one the desktop already has, nothing
//!   moves, and the answer is [`Resize::Refused`] rather than
//!   [`Resize::Unchanged`]: the client asked for a size it did not get,
//!   and nothing else will tell it so;
//! - when the size asked for is the one the desktop already has, the
//!   answer is [`Resize::Unchanged`].
//!
//! [`Resize`]: super::Resize
//! [`Resize::Applied`]: super::Resize::Applied
//! [`Resize::Refused`]: super::Resize::Refused
//! [`Resize::Unchanged`]: super::Resize::Unchanged

use super::Size;

/// The smallest and largest desktop a display control allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeRange {
    pub min: Size,
    pub max: Size,
}

impl std::fmt::Display for SizeRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} to {}", self.min, self.max)
    }
}

/// `asked`, each side brought within `range`.
pub fn clamp(asked: Size, range: SizeRange) -> Size {
    Size {
        width: asked.width.clamp(range.min.width, range.max.width),
        height: asked.height.clamp(range.min.height, range.max.height),
    }
}

/// What to do before changing anything, given the size `asked`, the size
/// `target` it clamped to, and whether the desktop already is `target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The desktop is already the size asked for.
    Unchanged,
    /// The desktop is already as near the size asked for as it allows,
    /// and that is not the size asked for.
    AtLimit,
    /// Change the desktop to `target`.
    Change,
}

/// The verdict for `asked`, clamped to `target`, where `already` says
/// the desktop is `target` now.
pub fn verdict(asked: Size, target: Size, already: bool) -> Verdict {
    match (already, asked == target) {
        (true, true) => Verdict::Unchanged,
        (true, false) => Verdict::AtLimit,
        (false, _) => Verdict::Change,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> Size {
        Size { width, height }
    }

    fn range() -> SizeRange {
        SizeRange {
            min: size(320, 200),
            max: size(3840, 2160),
        }
    }

    #[test]
    fn clamp_brings_each_side_within_the_range() {
        assert_eq!(clamp(size(1920, 1080), range()), size(1920, 1080));
        assert_eq!(clamp(size(8000, 8000), range()), size(3840, 2160));
        assert_eq!(clamp(size(1, 1), range()), size(320, 200));
        // Each side is clamped on its own.
        assert_eq!(clamp(size(8000, 1), range()), size(3840, 200));
    }

    #[test]
    fn a_size_the_desktop_has_is_unchanged() {
        let asked = size(1920, 1080);
        assert_eq!(verdict(asked, asked, true), Verdict::Unchanged);
    }

    #[test]
    fn a_clamped_size_the_desktop_has_is_at_the_limit() {
        // Asked for more than the range allows, and already at the top
        // of it: nothing moves, and the client has to be told.
        assert_eq!(
            verdict(size(8000, 8000), size(3840, 2160), true),
            Verdict::AtLimit
        );
    }

    #[test]
    fn anything_the_desktop_is_not_is_a_change() {
        assert_eq!(
            verdict(size(1280, 720), size(1280, 720), false),
            Verdict::Change
        );
        // Clamped, and not there yet: still a change, to the clamp.
        assert_eq!(
            verdict(size(8000, 8000), size(3840, 2160), false),
            Verdict::Change
        );
    }

    #[test]
    fn a_range_displays_as_its_bounds() {
        assert_eq!(range().to_string(), "320x200 to 3840x2160");
    }
}
