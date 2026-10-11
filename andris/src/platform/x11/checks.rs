//! What andris needs of an X server before it will export it, decided
//! from plain facts so that the decision can be tested without one.

use anyhow::{Result, bail};

/// What the X server says about itself, as far as andris cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFacts {
    /// The root window's depth.
    pub root_depth: u8,
    /// Bits per pixel of the pixmap format for `root_depth`; `None` if
    /// the server lists no format for that depth.
    pub bits_per_pixel: Option<u8>,
    /// Whether the image byte order is LSBFirst.
    pub lsb_first: bool,
    /// The root visual's red, green and blue masks; `None` if the root
    /// visual is not TrueColor or DirectColor, or was not found.
    pub rgb_masks: Option<(u32, u32, u32)>,
    /// Whether the XTEST extension is present.
    pub xtest: bool,
    /// Whether the XFIXES extension is present.
    pub xfixes: bool,
    /// Whether the XKEYBOARD extension is present and accepted the
    /// `UseExtension` handshake.
    pub xkb: bool,
    /// Whether the DAMAGE extension is present.
    pub damage: bool,
    /// Whether MIT-SHM 1.2 or later is present: the version that attaches
    /// a segment passed as a file descriptor. Optional; without it andris
    /// captures with `GetImage`.
    pub shm_fd: bool,
}

/// The masks that make a 32 bit pixel B, G, R, X in memory on an
/// LSBFirst server.
pub const BGRX_MASKS: (u32, u32, u32) = (0x00ff_0000, 0x0000_ff00, 0x0000_00ff);

/// Fails, naming every problem, unless `facts` describe a server whose
/// root window `GetImage` returns as BGRX at 32 bits a pixel, and which
/// has the extensions andris uses for input, the cursor and damage.
/// MIT-SHM is not required, so [`ServerFacts::shm_fd`] is not checked.
pub fn check(display: &str, facts: &ServerFacts) -> Result<()> {
    let mut problems = Vec::new();
    if facts.root_depth != 24 && facts.root_depth != 32 {
        problems.push(format!(
            "the root window's depth is {}, not 24 or 32",
            facts.root_depth
        ));
    }
    match facts.bits_per_pixel {
        Some(32) => {}
        Some(bpp) => problems.push(format!(
            "depth {} is stored at {bpp} bits per pixel, not 32",
            facts.root_depth
        )),
        None => problems.push(format!(
            "the server lists no pixmap format for depth {}",
            facts.root_depth
        )),
    }
    if !facts.lsb_first {
        problems.push("the image byte order is MSBFirst, not LSBFirst".to_string());
    }
    match facts.rgb_masks {
        Some(BGRX_MASKS) => {}
        Some((r, g, b)) => problems.push(format!(
            "the root visual's red, green and blue masks are {r:#08x}, {g:#08x} and {b:#08x}, \
             not {:#08x}, {:#08x} and {:#08x}",
            BGRX_MASKS.0, BGRX_MASKS.1, BGRX_MASKS.2
        )),
        None => problems.push("the root visual is not TrueColor".to_string()),
    }
    for (present, name) in [
        (facts.xtest, "XTEST"),
        (facts.xfixes, "XFIXES"),
        (facts.xkb, "XKEYBOARD"),
        (facts.damage, "DAMAGE"),
    ] {
        if !present {
            problems.push(format!("the {name} extension is missing"));
        }
    }
    if !problems.is_empty() {
        bail!(
            "andris cannot export X display {display}: {}",
            problems.join("; ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Xvfb at 24 bits reports.
    fn xvfb() -> ServerFacts {
        ServerFacts {
            root_depth: 24,
            bits_per_pixel: Some(32),
            lsb_first: true,
            rgb_masks: Some(BGRX_MASKS),
            xtest: true,
            xfixes: true,
            xkb: true,
            damage: true,
            shm_fd: true,
        }
    }

    fn refusal(facts: &ServerFacts) -> String {
        format!("{:#}", check(":99", facts).unwrap_err())
    }

    #[test]
    fn accepts_depth_24_and_32() {
        check(":99", &xvfb()).unwrap();
        check(
            ":99",
            &ServerFacts {
                shm_fd: false,
                ..xvfb()
            },
        )
        .expect("MIT-SHM is optional");
        check(
            ":99",
            &ServerFacts {
                root_depth: 32,
                ..xvfb()
            },
        )
        .unwrap();
    }

    #[test]
    fn refuses_other_depths() {
        let message = refusal(&ServerFacts {
            root_depth: 16,
            bits_per_pixel: Some(16),
            ..xvfb()
        });
        assert!(message.contains(":99"), "{message}");
        assert!(message.contains("depth is 16"), "{message}");
        assert!(message.contains("16 bits per pixel"), "{message}");
    }

    #[test]
    fn refuses_packed_24_bit_pixels() {
        let message = refusal(&ServerFacts {
            bits_per_pixel: Some(24),
            ..xvfb()
        });
        assert!(message.contains("24 bits per pixel, not 32"), "{message}");
        let message = refusal(&ServerFacts {
            bits_per_pixel: None,
            ..xvfb()
        });
        assert!(message.contains("no pixmap format"), "{message}");
    }

    #[test]
    fn refuses_msb_first() {
        let message = refusal(&ServerFacts {
            lsb_first: false,
            ..xvfb()
        });
        assert!(message.contains("MSBFirst"), "{message}");
    }

    #[test]
    fn refuses_other_channel_orders_and_visual_classes() {
        let message = refusal(&ServerFacts {
            rgb_masks: Some((0xff, 0xff00, 0xff_0000)),
            ..xvfb()
        });
        assert!(message.contains("masks"), "{message}");
        let message = refusal(&ServerFacts {
            rgb_masks: None,
            ..xvfb()
        });
        assert!(message.contains("TrueColor"), "{message}");
    }

    #[test]
    fn names_each_missing_extension() {
        for (facts, name) in [
            (
                ServerFacts {
                    xtest: false,
                    ..xvfb()
                },
                "XTEST",
            ),
            (
                ServerFacts {
                    xfixes: false,
                    ..xvfb()
                },
                "XFIXES",
            ),
            (
                ServerFacts {
                    xkb: false,
                    ..xvfb()
                },
                "XKEYBOARD",
            ),
            (
                ServerFacts {
                    damage: false,
                    ..xvfb()
                },
                "DAMAGE",
            ),
        ] {
            let message = refusal(&facts);
            assert!(
                message.contains(&format!("the {name} extension is missing")),
                "{message}"
            );
        }
    }

    #[test]
    fn names_every_problem_at_once() {
        let message = refusal(&ServerFacts {
            lsb_first: false,
            xtest: false,
            xkb: false,
            ..xvfb()
        });
        assert!(message.contains("MSBFirst"), "{message}");
        assert!(message.contains("XTEST"), "{message}");
        assert!(message.contains("XKEYBOARD"), "{message}");
        assert!(!message.contains("XFIXES"), "{message}");
    }
}
