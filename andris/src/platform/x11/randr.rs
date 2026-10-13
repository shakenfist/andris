//! Display control through RandR 1.2: making the desktop the size a client
//! asks for, on the capture thread's connection.
//!
//! A resize works on one output and the CRTC that drives it: the primary
//! output if one is set and has a CRTC, else the first connected output
//! that has one. Andris:
//!
//! 1. clamps the size to the server's `GetScreenSizeRange`;
//! 2. answers [`Resize::Unchanged`] if the screen and the CRTC are already
//!    that size, and [`Resize::Refused`] if clamping left the size the
//!    desktop already is, short of what was asked (see below);
//! 3. finds a mode of exactly that size, adding it to the CRTC's outputs
//!    that lack it, or creates one named `andris-<width>x<height>`;
//! 4. changes the screen size and the CRTC in an order that keeps the CRTC
//!    inside the screen at every step (see [`plan`]), with the screen's
//!    physical size set for 96 DPI;
//! 5. reads the screen and the CRTC back, because a server can answer
//!    `RRSetCrtcConfig` with Success and change nothing (xorgxrdp does);
//! 6. removes every `andris-` mode no CRTC uses any longer.
//!
//! If any request fails, or the read-back finds the desktop is not the
//! size asked for, the steps already taken are undone in reverse, the
//! modes the attempt added are removed, and the answer is
//! [`Resize::Refused`], naming the X error. xorgxrdp refuses every
//! `RRSetScreenSize` with `BadMatch`, so on xrdp a resize always ends
//! there. Only a failed connection is an error rather than a refusal.
//!
//! A size outside the server's range is clamped to it, and a resize to the
//! clamped size is [`Resize::Applied`]: the desktop moved as far towards
//! the request as the server allows, and the client sees the new size as
//! it sees any other. When the clamped size is the one the desktop
//! already has, nothing moves, and the answer is [`Resize::Refused`]
//! rather than [`Resize::Unchanged`]: the client asked for a size it did
//! not get, and nothing else will tell it so.

use anyhow::anyhow;
use tracing::{debug, warn};
use x11rb::connection::RequestConnection;
use x11rb::protocol::randr::{
    self, ConnectionExt as _, Crtc, GetCrtcInfoReply, Mode, ModeFlag, ModeInfo, Output, Rotation,
    SetConfig,
};
use x11rb::protocol::xproto::Window;
use x11rb::rust_connection::RustConnection;

use super::Fault;
use super::capture::root_size;
use crate::platform::{Resize, Size};

/// The RandR version andris asks for. It needs 1.2, for CRTCs, outputs and
/// user modes; with 1.3 it also reads the primary output, and reads the
/// screen's resources without asking the server to probe its outputs.
pub(super) const RANDR_VERSION: (u32, u32) = (1, 3);

/// The prefix of the name of every mode andris creates.
const MODE_PREFIX: &str = "andris-";

/// The name of the mode andris creates for `size`.
fn mode_name(size: Size) -> String {
    format!("{MODE_PREFIX}{size}")
}

/// Whether a mode named `name` is one andris created.
fn is_andris_mode(name: &[u8]) -> bool {
    name.starts_with(MODE_PREFIX.as_bytes())
}

/// The smallest and largest screen the server allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SizeRange {
    min: Size,
    max: Size,
}

impl std::fmt::Display for SizeRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} to {}", self.min, self.max)
    }
}

/// `asked`, each side brought within `range`.
fn clamp(asked: Size, range: SizeRange) -> Size {
    Size {
        width: asked.width.clamp(range.min.width, range.max.width),
        height: asked.height.clamp(range.min.height, range.max.height),
    }
}

/// `pixels` at 96 DPI, in whole millimetres.
fn millimetres(pixels: u32) -> u32 {
    let mm = (u64::from(pixels) * 254 + 480) / 960;
    u32::try_from(mm).unwrap_or(u32::MAX)
}

/// One change to the server's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Set the screen to this size.
    Screen(Size),
    /// Set the CRTC to the new mode, at the screen's origin.
    Crtc,
}

/// Whether `outer` is at least `inner` on both sides.
fn contains(outer: Size, inner: Size) -> bool {
    outer.width >= inner.width && outer.height >= inner.height
}

/// The steps that take a screen of `screen`, whose CRTC reaches `extent`
/// (its right and bottom edges), to a screen of `target` filled by the
/// CRTC, keeping the CRTC inside the screen at every step, as RandR
/// requires: the screen first when the old CRTC fits the new screen, the
/// CRTC first when the new screen fits inside the old, and otherwise the
/// screen grown to hold both, the CRTC, and the screen to its size. A
/// screen step that would not change the screen is left out.
fn plan(screen: Size, extent: Size, target: Size) -> Vec<Step> {
    let mut steps = Vec::with_capacity(3);
    let mut now = screen;
    let mut resize = |steps: &mut Vec<Step>, to: Size| {
        if to != now {
            steps.push(Step::Screen(to));
            now = to;
        }
    };
    if contains(target, extent) {
        resize(&mut steps, target);
        steps.push(Step::Crtc);
    } else if contains(screen, target) {
        steps.push(Step::Crtc);
        resize(&mut steps, target);
    } else {
        let both = Size {
            width: target.width.max(extent.width),
            height: target.height.max(extent.height),
        };
        resize(&mut steps, both);
        steps.push(Step::Crtc);
        resize(&mut steps, target);
    }
    steps
}

/// A mode for a `size` CRTC that no real display will show: andris's
/// desktops are virtual, so the timings only need to be self-consistent.
/// The blanking is a few lines and columns, and the dot clock gives about
/// 60 Hz, as far as it fits.
fn mode_info(size: Size, name: &str) -> Result<ModeInfo, Fault> {
    let side = |pixels: u32| {
        u16::try_from(pixels).map_err(|_| Fault::Failed(anyhow!("{size} does not fit a mode")))
    };
    let (width, height) = (side(size.width)?, side(size.height)?);
    let htotal = width.saturating_add(160);
    let vtotal = height.saturating_add(30);
    let dot_clock = u64::from(htotal) * u64::from(vtotal) * 60;
    Ok(ModeInfo {
        id: 0,
        width,
        height,
        dot_clock: u32::try_from(dot_clock).unwrap_or(u32::MAX),
        hsync_start: width.saturating_add(48),
        hsync_end: width.saturating_add(80),
        htotal,
        hskew: 0,
        vsync_start: height.saturating_add(3),
        vsync_end: height.saturating_add(8),
        vtotal,
        name_len: u16::try_from(name.len())
            .map_err(|_| Fault::Failed(anyhow!("the mode name {name} is too long")))?,
        mode_flags: ModeFlag::HSYNC_POSITIVE | ModeFlag::VSYNC_POSITIVE,
    })
}

/// What [`choose_output`] needs to know of an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputState {
    output: Output,
    connected: bool,
    /// The CRTC driving it, or 0 for none.
    crtc: Crtc,
}

/// The output to resize: `primary` if it is set and has a CRTC, else the
/// first connected output that has one.
fn choose_output(primary: Option<Output>, outputs: &[OutputState]) -> Option<OutputState> {
    let primary = primary.and_then(|primary| {
        outputs
            .iter()
            .find(|state| state.output == primary && state.crtc != 0)
    });
    primary
        .or_else(|| {
            outputs
                .iter()
                .find(|state| state.connected && state.crtc != 0)
        })
        .copied()
}

/// What to do before changing anything, given the size `asked`, the size
/// `target` it clamped to, and whether the screen and CRTC already are
/// `target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The desktop is already the size asked for.
    Unchanged,
    /// The desktop is already as near the size asked for as the server
    /// allows, and that is not the size asked for.
    AtLimit,
    /// Change the desktop to `target`.
    Change,
}

fn verdict(asked: Size, target: Size, already: bool) -> Verdict {
    match (already, asked == target) {
        (true, true) => Verdict::Unchanged,
        (true, false) => Verdict::AtLimit,
        (false, _) => Verdict::Change,
    }
}

/// A request's failure, with what was being asked for.
fn context(what: impl FnOnce() -> String) -> impl FnOnce(Fault) -> Fault {
    move |fault| match fault {
        Fault::Gone(e) => Fault::Gone(e),
        Fault::Failed(e) => Fault::Failed(e.context(what())),
    }
}

/// The screen's resources: its CRTCs, outputs and modes with their names.
struct Resources {
    config_timestamp: u32,
    crtcs: Vec<Crtc>,
    outputs: Vec<Output>,
    modes: Vec<(ModeInfo, Vec<u8>)>,
}

/// What `RRSetCrtcConfig` sets a CRTC to.
#[derive(Debug, Clone)]
struct CrtcSetting {
    x: i16,
    y: i16,
    mode: Mode,
    rotation: Rotation,
    outputs: Vec<Output>,
}

/// The CRTC's configuration, as `RRGetCrtcInfo` reports it.
#[derive(Debug, Clone)]
struct CrtcConfig {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    mode: Mode,
    rotation: Rotation,
    outputs: Vec<Output>,
}

impl From<GetCrtcInfoReply> for CrtcConfig {
    fn from(info: GetCrtcInfoReply) -> Self {
        Self {
            x: info.x,
            y: info.y,
            width: info.width,
            height: info.height,
            mode: info.mode,
            rotation: info.rotation,
            outputs: info.outputs,
        }
    }
}

impl CrtcConfig {
    /// Where its right and bottom edges reach; nothing for a disabled
    /// CRTC.
    fn extent(&self) -> Size {
        if self.mode == 0 {
            return Size {
                width: 0,
                height: 0,
            };
        }
        let edge = |origin: i16, length: u16| {
            u32::try_from(i32::from(origin) + i32::from(length)).unwrap_or(0)
        };
        Size {
            width: edge(self.x, self.width),
            height: edge(self.y, self.height),
        }
    }

    /// Whether it shows `mode`, `size` large, from the screen's origin.
    fn fills(&self, size: Size, mode: Option<Mode>) -> bool {
        self.mode != 0
            && mode.is_none_or(|mode| mode == self.mode)
            && (self.x, self.y) == (0, 0)
            && (u32::from(self.width), u32::from(self.height)) == (size.width, size.height)
    }

    /// What to set the CRTC to, to restore this configuration.
    fn setting(&self) -> CrtcSetting {
        CrtcSetting {
            x: self.x,
            y: self.y,
            mode: self.mode,
            rotation: self.rotation,
            outputs: self.outputs.clone(),
        }
    }

    /// Whether it is rotated a quarter turn, so that its mode is the
    /// screen's size on its side.
    fn on_its_side(&self) -> bool {
        self.rotation & (Rotation::ROTATE90 | Rotation::ROTATE270) != Rotation::from(0u16)
    }
}

/// RandR on one connection, once its version handshake is made.
pub(super) struct Randr {
    /// The version the server agreed to: 1.2 or later.
    version: (u32, u32),
}

/// A change one resize made, and how to undo it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Undo {
    /// The screen was changed from this size.
    Screen(Size),
    /// The CRTC was changed from its old configuration.
    Crtc,
}

/// What one resize has done, to undo it if a later step fails.
#[derive(Default)]
struct Attempt {
    /// The configuration changes made, in order.
    done: Vec<Undo>,
    /// Modes this attempt added to outputs, as (output, mode).
    added: Vec<(Output, Mode)>,
    /// Modes this attempt created.
    created: Vec<Mode>,
}

impl Randr {
    /// RandR on a connection that has made the `QueryVersion` handshake
    /// and been granted `version`, which must be 1.2 or later.
    pub(super) fn new(version: (u32, u32)) -> Self {
        Self { version }
    }

    /// Make the desktop rooted at `root` `asked`, as the module's
    /// documentation describes. Only a failed connection is an error.
    pub(super) fn set_size(
        &self,
        conn: &RustConnection,
        root: Window,
        asked: Size,
    ) -> Result<Resize, Fault> {
        match self.try_set_size(conn, root, asked) {
            Ok(resize) => Ok(resize),
            Err(Fault::Gone(e)) => Err(Fault::Gone(e)),
            Err(Fault::Failed(e)) => Ok(Resize::Refused(format!("{e:#}"))),
        }
    }

    fn try_set_size(
        &self,
        conn: &RustConnection,
        root: Window,
        asked: Size,
    ) -> Result<Resize, Fault> {
        let range = conn
            .randr_get_screen_size_range(root)?
            .reply()
            .map_err(Fault::from)
            .map_err(context(|| "RRGetScreenSizeRange".to_string()))?;
        let range = SizeRange {
            min: Size {
                width: range.min_width.into(),
                height: range.min_height.into(),
            },
            max: Size {
                width: range.max_width.into(),
                height: range.max_height.into(),
            },
        };
        let target = clamp(asked, range);
        let resources = self.resources(conn, root)?;
        let chosen = self.choose_output(conn, root, &resources)?.ok_or_else(|| {
            Fault::Failed(anyhow!(
                "the X server has no connected output with a CRTC to resize"
            ))
        })?;
        let old = self.crtc_config(conn, chosen.crtc, resources.config_timestamp)?;
        let screen = root_size(conn, root)?;
        match verdict(asked, target, screen == target && old.fills(target, None)) {
            Verdict::Unchanged => return Ok(Resize::Unchanged),
            Verdict::AtLimit => {
                return Ok(Resize::Refused(format!(
                    "the X server allows desktops from {range}, and the desktop is already \
                     {target}, the nearest it allows to {asked}"
                )));
            }
            Verdict::Change => {}
        }
        let mut attempt = Attempt::default();
        let outcome = self.change(
            conn,
            root,
            &resources,
            &chosen,
            &old,
            screen,
            target,
            &mut attempt,
        );
        match outcome {
            Ok(()) => {
                debug!("resized the desktop to {target} with RandR");
                carry_on(
                    self.remove_unused_modes(conn, root),
                    "removing andris's unused modes",
                )?;
                Ok(Resize::Applied)
            }
            Err(Fault::Gone(e)) => Err(Fault::Gone(e)),
            Err(Fault::Failed(e)) => {
                self.undo(conn, root, chosen.crtc, &old, &attempt)?;
                Err(Fault::Failed(e))
            }
        }
    }

    /// Find or make the mode, then take the planned steps and read the
    /// result back, recording in `attempt` what has been done.
    #[allow(clippy::too_many_arguments)]
    fn change(
        &self,
        conn: &RustConnection,
        root: Window,
        resources: &Resources,
        chosen: &OutputState,
        old: &CrtcConfig,
        screen: Size,
        target: Size,
        attempt: &mut Attempt,
    ) -> Result<(), Fault> {
        let mode_size = if old.on_its_side() {
            Size {
                width: target.height,
                height: target.width,
            }
        } else {
            target
        };
        let outputs = if old.outputs.is_empty() {
            vec![chosen.output]
        } else {
            old.outputs.clone()
        };
        let mode = self.find_or_create_mode(conn, root, resources, &outputs, mode_size, attempt)?;
        let mut now = screen;
        for step in plan(screen, old.extent(), target) {
            match step {
                Step::Screen(size) => {
                    set_screen_size(conn, root, size)?;
                    attempt.done.push(Undo::Screen(now));
                    now = size;
                }
                Step::Crtc => {
                    self.set_crtc(
                        conn,
                        root,
                        chosen.crtc,
                        &CrtcSetting {
                            x: 0,
                            y: 0,
                            mode,
                            rotation: old.rotation,
                            outputs: outputs.clone(),
                        },
                    )?;
                    attempt.done.push(Undo::Crtc);
                }
            }
        }
        // The read-back: never trust a Success.
        let screen = root_size(conn, root)?;
        let resources = self.resources(conn, root)?;
        let crtc = self.crtc_config(conn, chosen.crtc, resources.config_timestamp)?;
        if screen != target || !crtc.fills(target, Some(mode)) {
            return Err(Fault::Failed(anyhow!(
                "the X server accepted every request, but the desktop is {screen} and its CRTC \
                 {}x{}, not {target}",
                crtc.width,
                crtc.height
            )));
        }
        Ok(())
    }

    /// Undo what `attempt` did, in reverse, and remove the modes it added
    /// and created. A screen size is restored at 96 DPI, since RandR 1.2
    /// offers no cheap way to read the physical size it had. Each failure
    /// is logged and the rest carried on with; only a failed connection
    /// stops it.
    fn undo(
        &self,
        conn: &RustConnection,
        root: Window,
        crtc: Crtc,
        old: &CrtcConfig,
        attempt: &Attempt,
    ) -> Result<(), Fault> {
        for undo in attempt.done.iter().rev() {
            match *undo {
                Undo::Screen(size) => carry_on(
                    set_screen_size(conn, root, size),
                    "restoring the screen size",
                )?,
                Undo::Crtc => carry_on(
                    self.set_crtc(conn, root, crtc, &old.setting()),
                    "restoring the CRTC",
                )?,
            }
        }
        for &(output, mode) in attempt.added.iter().rev() {
            carry_on(
                delete_output_mode(conn, output, mode),
                "removing a mode andris added",
            )?;
        }
        for &mode in attempt.created.iter().rev() {
            carry_on(destroy_mode(conn, mode), "destroying a mode andris created")?;
        }
        Ok(())
    }

    /// The screen's resources, without probing outputs where the server
    /// allows that.
    fn resources(&self, conn: &RustConnection, root: Window) -> Result<Resources, Fault> {
        let names = |modes: &[ModeInfo], names: &[u8]| -> Result<Vec<(ModeInfo, Vec<u8>)>, Fault> {
            let mut rest = names;
            modes
                .iter()
                .map(|mode| {
                    let (name, tail) =
                        rest.split_at_checked(mode.name_len.into()).ok_or_else(|| {
                            Fault::Failed(anyhow!("the X server's mode names are short"))
                        })?;
                    rest = tail;
                    Ok((*mode, name.to_vec()))
                })
                .collect()
        };
        if self.version >= (1, 3) {
            let reply = conn
                .randr_get_screen_resources_current(root)?
                .reply()
                .map_err(Fault::from)
                .map_err(context(|| "RRGetScreenResourcesCurrent".to_string()))?;
            Ok(Resources {
                config_timestamp: reply.config_timestamp,
                modes: names(&reply.modes, &reply.names)?,
                crtcs: reply.crtcs,
                outputs: reply.outputs,
            })
        } else {
            let reply = conn
                .randr_get_screen_resources(root)?
                .reply()
                .map_err(Fault::from)
                .map_err(context(|| "RRGetScreenResources".to_string()))?;
            Ok(Resources {
                config_timestamp: reply.config_timestamp,
                modes: names(&reply.modes, &reply.names)?,
                crtcs: reply.crtcs,
                outputs: reply.outputs,
            })
        }
    }

    /// The output to resize; see [`choose_output`].
    fn choose_output(
        &self,
        conn: &RustConnection,
        root: Window,
        resources: &Resources,
    ) -> Result<Option<OutputState>, Fault> {
        let primary = if self.version >= (1, 3) {
            let primary = conn
                .randr_get_output_primary(root)?
                .reply()
                .map_err(Fault::from)
                .map_err(context(|| "RRGetOutputPrimary".to_string()))?
                .output;
            (primary != 0).then_some(primary)
        } else {
            None
        };
        let mut outputs = Vec::with_capacity(resources.outputs.len());
        for &output in &resources.outputs {
            let info = output_info(conn, output, resources.config_timestamp)?;
            outputs.push(OutputState {
                output,
                connected: info.connection == randr::Connection::CONNECTED,
                crtc: info.crtc,
            });
        }
        Ok(choose_output(primary, &outputs))
    }

    /// The CRTC's configuration now.
    fn crtc_config(
        &self,
        conn: &RustConnection,
        crtc: Crtc,
        config_timestamp: u32,
    ) -> Result<CrtcConfig, Fault> {
        let info = conn
            .randr_get_crtc_info(crtc, config_timestamp)?
            .reply()
            .map_err(Fault::from)
            .map_err(context(|| format!("RRGetCrtcInfo for CRTC {crtc:#x}")))?;
        Ok(info.into())
    }

    /// A mode of exactly `size` that every one of `outputs` lists: one the
    /// server has, added to the outputs that lack it, or one created for
    /// andris. Records what it adds and creates in `attempt`.
    fn find_or_create_mode(
        &self,
        conn: &RustConnection,
        root: Window,
        resources: &Resources,
        outputs: &[Output],
        size: Size,
        attempt: &mut Attempt,
    ) -> Result<Mode, Fault> {
        let mut listed = Vec::with_capacity(outputs.len());
        for &output in outputs {
            listed.push(output_info(conn, output, resources.config_timestamp)?.modes);
        }
        let candidates: Vec<Mode> = resources
            .modes
            .iter()
            .filter(|(mode, _)| {
                (u32::from(mode.width), u32::from(mode.height)) == (size.width, size.height)
            })
            .map(|(mode, _)| mode.id)
            .collect();
        // Best a mode every output lists already, then the one most list.
        let existing = candidates
            .iter()
            .copied()
            .max_by_key(|mode| listed.iter().filter(|modes| modes.contains(mode)).count());
        let mode = match existing {
            Some(mode) => mode,
            None => {
                let name = mode_name(size);
                let info = mode_info(size, &name)?;
                let mode = conn
                    .randr_create_mode(root, info, name.as_bytes())?
                    .reply()
                    .map_err(Fault::from)
                    .map_err(context(|| format!("RRCreateMode {name}")))?
                    .mode;
                attempt.created.push(mode);
                mode
            }
        };
        for (&output, modes) in outputs.iter().zip(&listed) {
            if !modes.contains(&mode) {
                conn.randr_add_output_mode(output, mode)?
                    .check()
                    .map_err(Fault::from)
                    .map_err(context(|| {
                        format!("RRAddOutputMode of a {size} mode to output {output:#x}")
                    }))?;
                attempt.added.push((output, mode));
            }
        }
        Ok(mode)
    }

    /// Set `crtc` to `config`. The configuration timestamp is read afresh,
    /// since adding a mode to an output changes it, and read again for one
    /// retry if the server says it changed meanwhile.
    fn set_crtc(
        &self,
        conn: &RustConnection,
        root: Window,
        crtc: Crtc,
        config: &CrtcSetting,
    ) -> Result<(), Fault> {
        let what = || format!("RRSetCrtcConfig for CRTC {crtc:#x}");
        let mut retried = false;
        loop {
            let config_timestamp = self.resources(conn, root)?.config_timestamp;
            let status = conn
                .randr_set_crtc_config(
                    crtc,
                    x11rb::CURRENT_TIME,
                    config_timestamp,
                    config.x,
                    config.y,
                    config.mode,
                    config.rotation,
                    &config.outputs,
                )?
                .reply()
                .map_err(Fault::from)
                .map_err(context(what))?
                .status;
            match status {
                SetConfig::SUCCESS => return Ok(()),
                SetConfig::INVALID_CONFIG_TIME if !retried => retried = true,
                status => {
                    return Err(Fault::Failed(anyhow!(
                        "{}: the X server answered {status:?}",
                        what()
                    )));
                }
            }
        }
    }

    /// Remove every mode andris created that no CRTC shows: from each
    /// output that lists it, then from the server. A failure is logged,
    /// and the rest carried on with; only a failed connection stops it.
    fn remove_unused_modes(&self, conn: &RustConnection, root: Window) -> Result<(), Fault> {
        let resources = self.resources(conn, root)?;
        let ours: Vec<Mode> = resources
            .modes
            .iter()
            .filter(|(_, name)| is_andris_mode(name))
            .map(|(mode, _)| mode.id)
            .collect();
        if ours.is_empty() {
            return Ok(());
        }
        let mut shown = Vec::with_capacity(resources.crtcs.len());
        for &crtc in &resources.crtcs {
            shown.push(
                self.crtc_config(conn, crtc, resources.config_timestamp)?
                    .mode,
            );
        }
        for mode in ours.into_iter().filter(|mode| !shown.contains(mode)) {
            for &output in &resources.outputs {
                let modes = output_info(conn, output, resources.config_timestamp)?.modes;
                if modes.contains(&mode) {
                    carry_on(
                        delete_output_mode(conn, output, mode),
                        "removing an unused mode andris added",
                    )?;
                }
            }
            carry_on(
                destroy_mode(conn, mode),
                "destroying an unused mode andris created",
            )?;
        }
        Ok(())
    }
}

/// The output's state.
fn output_info(
    conn: &RustConnection,
    output: Output,
    config_timestamp: u32,
) -> Result<randr::GetOutputInfoReply, Fault> {
    conn.randr_get_output_info(output, config_timestamp)?
        .reply()
        .map_err(Fault::from)
        .map_err(context(|| {
            format!("RRGetOutputInfo for output {output:#x}")
        }))
}

/// Set the screen to `size`, at 96 DPI.
fn set_screen_size(conn: &RustConnection, root: Window, size: Size) -> Result<(), Fault> {
    let what = || format!("RRSetScreenSize to {size}");
    let side = |pixels: u32| {
        u16::try_from(pixels).map_err(|_| Fault::Failed(anyhow!("{}: too large", what())))
    };
    conn.randr_set_screen_size(
        root,
        side(size.width)?,
        side(size.height)?,
        millimetres(size.width),
        millimetres(size.height),
    )?
    .check()
    .map_err(Fault::from)
    .map_err(context(what))
}

fn delete_output_mode(conn: &RustConnection, output: Output, mode: Mode) -> Result<(), Fault> {
    conn.randr_delete_output_mode(output, mode)?
        .check()
        .map_err(Fault::from)
        .map_err(context(|| {
            format!("RRDeleteOutputMode of mode {mode:#x} from output {output:#x}")
        }))
}

fn destroy_mode(conn: &RustConnection, mode: Mode) -> Result<(), Fault> {
    conn.randr_destroy_mode(mode)?
        .check()
        .map_err(Fault::from)
        .map_err(context(|| format!("RRDestroyMode of mode {mode:#x}")))
}

/// Log a failed clean-up step, `what`, and carry on; a failed connection
/// still stops everything.
fn carry_on(result: Result<(), Fault>, what: &str) -> Result<(), Fault> {
    match result {
        Ok(()) => Ok(()),
        Err(Fault::Gone(e)) => Err(Fault::Gone(e)),
        Err(Fault::Failed(e)) => {
            warn!(error = format!("{e:#}"), "{what} failed");
            Ok(())
        }
    }
}

/// Make the RandR `QueryVersion` handshake on `conn`, if the server has
/// RandR, and return the version it grants; `None` without RandR.
pub(super) fn handshake(conn: &RustConnection) -> anyhow::Result<Option<(u32, u32)>> {
    if conn
        .extension_information(randr::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(None);
    }
    let reply = conn
        .randr_query_version(RANDR_VERSION.0, RANDR_VERSION.1)?
        .reply()
        .map_err(|e| anyhow!("the RandR QueryVersion handshake: {e}"))?;
    Ok(Some((reply.major_version, reply.minor_version)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn size(width: u32, height: u32) -> Size {
        Size { width, height }
    }

    const XVFB: SizeRange = SizeRange {
        min: size(1, 1),
        max: size(1024, 768),
    };

    #[test]
    fn modes_are_named_for_andris_and_the_size() {
        assert_eq!(mode_name(size(1280, 720)), "andris-1280x720");
        assert!(is_andris_mode(b"andris-1280x720"));
        assert!(!is_andris_mode(b"1280x720"));
        assert!(!is_andris_mode(b"andri"));
    }

    #[test]
    fn a_size_is_clamped_to_the_range_side_by_side() {
        assert_eq!(clamp(size(640, 480), XVFB), size(640, 480));
        assert_eq!(clamp(size(2000, 600), XVFB), size(1024, 600));
        assert_eq!(clamp(size(1280, 1024), XVFB), size(1024, 768));
        let xorgxrdp = SizeRange {
            min: size(256, 256),
            max: size(16384, 16384),
        };
        assert_eq!(clamp(size(100, 4000), xorgxrdp), size(256, 4000));
    }

    #[test]
    fn physical_size_is_96_dpi() {
        assert_eq!(millimetres(96), 25);
        assert_eq!(millimetres(1024), 271);
        assert_eq!(millimetres(768), 203);
        assert_eq!(millimetres(0), 0);
        assert_eq!(millimetres(u32::MAX), 1_136_376_763);
    }

    /// Run `steps` from a screen of `screen` whose CRTC reaches `extent`,
    /// checking the CRTC stays inside the screen; return the final screen.
    fn simulate(screen: Size, extent: Size, target: Size, steps: &[Step]) -> Size {
        let (mut screen, mut crtc) = (screen, extent);
        for step in steps {
            match *step {
                Step::Screen(to) => screen = to,
                Step::Crtc => crtc = target,
            }
            assert!(
                contains(screen, crtc),
                "{steps:?}: a {crtc} CRTC outside a {screen} screen"
            );
        }
        assert_eq!(crtc, target, "{steps:?} never set the CRTC");
        screen
    }

    #[test]
    fn growing_sets_the_screen_first_and_shrinking_the_crtc_first() {
        let start = size(1024, 768);
        assert_eq!(
            plan(size(640, 480), size(640, 480), start),
            vec![Step::Screen(start), Step::Crtc]
        );
        assert_eq!(
            plan(start, start, size(640, 480)),
            vec![Step::Crtc, Step::Screen(size(640, 480))]
        );
        // Wider but shorter: neither order keeps the CRTC inside, so the
        // screen grows to hold both first.
        assert_eq!(
            plan(start, start, size(1280, 600)),
            vec![
                Step::Screen(size(1280, 768)),
                Step::Crtc,
                Step::Screen(size(1280, 600))
            ]
        );
        // The screen is already the size; only the CRTC changes.
        assert_eq!(plan(start, size(800, 600), start), vec![Step::Crtc]);
        // A disabled CRTC reaches nowhere.
        assert_eq!(
            plan(start, size(0, 0), size(640, 480)),
            vec![Step::Screen(size(640, 480)), Step::Crtc]
        );
    }

    #[test]
    fn every_plan_keeps_the_crtc_inside_the_screen() {
        let sides = [1, 300, 640, 1024, 1500];
        for &sw in &sides {
            for &sh in &sides {
                let screen = size(sw, sh);
                for (ew, eh) in [(sw, sh), (sw / 2, sh), (sw, sh / 3), (0, 0)] {
                    let extent = size(ew, eh);
                    for &tw in &sides {
                        for &th in &sides {
                            let target = size(tw, th);
                            let steps = plan(screen, extent, target);
                            assert_eq!(simulate(screen, extent, target, &steps), target);
                            assert!(steps.len() <= 3, "{steps:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_mode_fits_its_size_with_some_blanking() {
        let mode = mode_info(size(1280, 720), "andris-1280x720").unwrap();
        assert_eq!((mode.width, mode.height), (1280, 720));
        assert_eq!(usize::from(mode.name_len), "andris-1280x720".len());
        assert!(mode.width < mode.hsync_start && mode.hsync_start < mode.hsync_end);
        assert!(mode.hsync_end < mode.htotal);
        assert!(mode.height < mode.vsync_start && mode.vsync_start < mode.vsync_end);
        assert!(mode.vsync_end < mode.vtotal);
        let refresh = f64::from(mode.dot_clock) / (f64::from(mode.htotal) * f64::from(mode.vtotal));
        assert!((59.0..61.0).contains(&refresh), "{refresh}");

        // The largest X allows: the clock saturates rather than wraps.
        let mode = mode_info(size(65535, 65535), "big").unwrap();
        assert_eq!(mode.dot_clock, u32::MAX);
        assert_eq!(mode.htotal, u16::MAX);
        assert!(mode_info(size(65536, 1), "too-big").is_err());
    }

    #[test]
    fn the_primary_output_wins_if_it_has_a_crtc() {
        let state = |output, connected, crtc| OutputState {
            output,
            connected,
            crtc,
        };
        let outputs = [
            state(1, false, 0),
            state(2, false, 20),
            state(3, true, 0),
            state(4, true, 40),
            state(5, true, 50),
        ];
        assert_eq!(choose_output(None, &outputs), Some(outputs[3]));
        assert_eq!(choose_output(Some(5), &outputs), Some(outputs[4]));
        // A primary output that is not connected but has a CRTC.
        assert_eq!(choose_output(Some(2), &outputs), Some(outputs[1]));
        // One without a CRTC gives way to the first connected one.
        assert_eq!(choose_output(Some(1), &outputs), Some(outputs[3]));
        assert_eq!(choose_output(Some(99), &outputs), Some(outputs[3]));
        assert_eq!(choose_output(None, &outputs[..3]), None);
    }

    #[test]
    fn a_clamped_size_the_desktop_already_has_is_refused() {
        let at = size(1024, 768);
        assert_eq!(verdict(at, at, true), Verdict::Unchanged);
        assert_eq!(verdict(size(1280, 1024), at, true), Verdict::AtLimit);
        assert_eq!(verdict(size(1280, 1024), at, false), Verdict::Change);
        assert_eq!(verdict(at, at, false), Verdict::Change);
    }

    fn crtc(x: i16, y: i16, width: u16, height: u16, mode: Mode, rotation: Rotation) -> CrtcConfig {
        CrtcConfig {
            x,
            y,
            width,
            height,
            mode,
            rotation,
            outputs: vec![7],
        }
    }

    #[test]
    fn a_crtc_reaches_its_far_edges_and_fills_only_from_the_origin() {
        let config = crtc(100, 50, 640, 480, 9, Rotation::ROTATE0);
        assert_eq!(config.extent(), size(740, 530));
        assert!(!config.fills(size(640, 480), None));
        assert!(!config.on_its_side());

        let config = crtc(0, 0, 640, 480, 9, Rotation::ROTATE0);
        assert!(config.fills(size(640, 480), None));
        assert!(config.fills(size(640, 480), Some(9)));
        assert!(!config.fills(size(640, 480), Some(10)));
        assert!(!config.fills(size(1024, 768), None));

        let disabled = crtc(0, 0, 0, 0, 0, Rotation::ROTATE0);
        assert_eq!(disabled.extent(), size(0, 0));
        assert!(!disabled.fills(size(0, 0), None));

        assert!(crtc(0, 0, 480, 640, 9, Rotation::ROTATE90).on_its_side());
        assert!(crtc(0, 0, 480, 640, 9, Rotation::ROTATE270 | Rotation::REFLECT_X).on_its_side());
        assert!(!crtc(0, 0, 640, 480, 9, Rotation::ROTATE180).on_its_side());
    }
}
