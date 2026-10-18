//! `andris install-unit`: install the systemd user unit, and the
//! configuration it reads.
//!
//! The unit, `packaging/systemd/andris@.service`, is embedded in the
//! binary, so that the file in the repository stays its one source. Its
//! `ExecStart=` names a bare `andris`, which systemd looks up only in its
//! own fixed binary path; a binary installed anywhere else, by pipx or
//! pip say, would not be found. Installing renders the unit with the
//! running binary's absolute path in its place, writes it to the user's
//! unit directory, and creates `andris/` and a ticket in the
//! configuration directory if they are missing.
//!
//! It writes files and prints the systemctl commands to run next; it
//! never runs systemctl. Enabling andris is the operator's step, and so
//! tests never touch a real user manager. [`render_unit`] and
//! [`install`] take every path as an argument, so tests run them against
//! temporary directories; only [`run`] reads the environment.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::config::{effective_uid, read_ticket_file};

/// The unit template, `packaging/systemd/andris@.service`. Its
/// `ExecStart=` names a bare `andris`, which [`render_unit`] replaces.
/// The file lies outside the crate, which only `cargo package` minds,
/// and andris is not published as a crate.
pub const UNIT_TEMPLATE: &str = include_str!("../../packaging/systemd/andris@.service");

/// The unit's file name in the user's unit directory.
pub const UNIT_NAME: &str = "andris@.service";

/// How many random bytes a new ticket holds. Base64 makes 32 characters
/// of them, with no padding, as `docs/xrdp.md`'s recipe
/// (`head -c 24 /dev/urandom | base64`) does.
pub const TICKET_RANDOM_BYTES: usize = 24;

/// Where the docs say how to find the display number, for when `DISPLAY`
/// does not name a local display.
const STARTING_DOC: &str =
    "https://github.com/shakenfist/andris/blob/develop/docs/xrdp.md#starting-it";

/// The setting [`render_unit`] rewrites.
const EXEC_START: &str = "ExecStart";

/// The bare command the template's `ExecStart=` starts with.
const BARE_COMMAND: &str = "andris";

/// Render the unit `template` with `exe`, an absolute path, in place of
/// the bare `andris` that starts its one `ExecStart=` line. Everything
/// else is kept as it is.
///
/// It fails unless the template has exactly one `ExecStart=` line, and
/// that line starts with a bare `andris`; and it fails if `exe` cannot be
/// written as systemd's executable path (see [`quote_executable`]).
pub fn render_unit(template: &str, exe: &Path) -> Result<String> {
    let quoted = quote_executable(exe)?;
    let exec_lines: Vec<usize> = template
        .lines()
        .enumerate()
        .filter(|(_, line)| setting_name(line) == Some(EXEC_START))
        .map(|(number, _)| number)
        .collect();
    let [exec_line] = exec_lines[..] else {
        bail!(
            "the unit template has {} {EXEC_START}= lines, not one",
            exec_lines.len()
        );
    };

    let mut rendered = String::with_capacity(template.len() + quoted.len());
    for (number, line) in template.split_inclusive('\n').enumerate() {
        if number != exec_line {
            rendered.push_str(line);
            continue;
        }
        let rest = line
            .split_once('=')
            .map(|(_, value)| value.trim_start())
            .and_then(|value| value.strip_prefix(BARE_COMMAND))
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            .with_context(|| {
                format!(
                    "the unit template's {EXEC_START}= does not start with a bare \
                     {BARE_COMMAND}: {}",
                    line.trim_end()
                )
            })?;
        rendered.push_str(EXEC_START);
        rendered.push('=');
        rendered.push_str(&quoted);
        rendered.push_str(rest);
    }
    Ok(rendered)
}

/// The name of the setting `line` assigns, if it assigns one: the text
/// before its `=`, trimmed, for a line that is not a comment. systemd
/// allows whitespace around the `=`.
fn setting_name(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if line.starts_with('#') || line.starts_with(';') {
        return None;
    }
    line.split_once('=').map(|(name, _)| name.trim_end())
}

/// `exe`, an absolute path, quoted and escaped to be the executable of an
/// `ExecStart=` line.
///
/// These rules were checked against systemd 257's manual pages, and then
/// against `systemd-analyze verify` (systemd 257) on units whose
/// executable path held each character:
///
/// - **Quoted.** The path is wrapped in double quotes, so that a space
///   in it does not end the word (systemd.syntax(7), *Quoting*).
///   systemd.service(5), *Command Lines*, says that "the command to
///   execute may contain spaces".
/// - **`%` becomes `%%`.** Specifiers are expanded over the whole line,
///   and `%%` is "a single percent sign" (systemd.unit(5),
///   *Specifiers*).
/// - **`$` is kept as it is.** systemd.service(5), *Command Lines*, says
///   that "$$" passes a literal dollar sign, but also that "the first
///   argument (i.e. the program to execute) may not be a variable":
///   environment variables are not expanded in the executable path, so
///   neither is `$$`. `systemd-analyze verify` confirms it: a unit
///   naming `"/opt/a$$b/andris"` looks for that path, `$$` and all, and
///   one naming `"/opt/a$b/andris"` finds `/opt/a$b/andris`.
/// - **Control characters are refused**, a newline among them.
///   systemd.service(5), *Command Lines*: "control characters are not
///   allowed".
/// - **`"`, `'` and `\` are refused.** systemd.syntax(7) would let them
///   be escaped inside quotes, but systemd then refuses the unescaped
///   path anyway: `systemd-analyze verify` rejects each of the three
///   with "Executable path contains special characters", and the unit
///   "will not be started". No escaping makes such a path work, so
///   saying so here beats writing a unit systemd refuses.
/// - **A path that is not UTF-8, or not absolute, is refused.** A unit
///   file is UTF-8 (systemd.syntax(7)), and a relative path would be
///   looked up in systemd's binary path, which is what this replaces.
///   An absolute path also cannot start with one of the executable
///   prefixes (`@`, `-`, `:`, `+`, `!`).
pub fn quote_executable(exe: &Path) -> Result<String> {
    let shown = exe.display();
    if !exe.is_absolute() {
        bail!("andris's path, {shown}, is not absolute, so systemd would look it up instead");
    }
    let Some(text) = exe.to_str() else {
        bail!("andris's path, {shown}, is not UTF-8, and a systemd unit file must be");
    };
    if text.chars().any(char::is_control) {
        bail!(
            "andris's path, {text:?}, holds a control character, which systemd does not allow \
             in a command: move andris to a path without one"
        );
    }
    if let Some(bad) = text.chars().find(|c| matches!(c, '"' | '\'' | '\\')) {
        bail!(
            "andris's path, {text}, holds {bad:?}, which systemd refuses in an executable's \
             path: move andris to a path without one"
        );
    }
    Ok(format!("\"{}\"", text.replace('%', "%%")))
}

/// What happened to the unit file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitState {
    /// There was none, and now there is.
    Written,
    /// There was a different one, or something else at its path (a
    /// symbolic link, say), and it was replaced.
    Replaced,
    /// It already said exactly this.
    Unchanged,
}

/// The configuration directory, `andris/`, as installing found or left
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDirState {
    /// It was missing, and was created with mode 0700.
    Created,
    /// It was there, and private, and was left alone.
    Existing,
    /// It was there, and left alone, but group or other may read or
    /// enter it: these are its permission bits.
    OpenToOthers { mode: u32 },
}

/// The ticket file, as installing found or left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketState {
    /// There was none, and a new random one was written, mode 0600.
    Created,
    /// There was one that andris would accept, and it was left alone.
    Existing,
    /// There was one, and it was left alone, but andris would refuse it,
    /// for this reason.
    Refused(String),
}

/// What [`install`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The unit file's path.
    pub unit_path: PathBuf,
    pub unit: UnitState,
    /// The executable the unit names.
    pub exe: PathBuf,
    /// The configuration directory's path.
    pub config_dir: PathBuf,
    pub config_dir_state: ConfigDirState,
    /// The ticket file's path.
    pub ticket_path: PathBuf,
    pub ticket: TicketState,
}

/// Install the unit, rendered for `exe`, and the configuration it reads,
/// under `config_home` (`$XDG_CONFIG_HOME`, or `~/.config`):
///
/// - the unit goes to `systemd/user/andris@.service`, creating the
///   directories as needed. It is always overwritten: local changes
///   belong in a drop-in, and running this again is how `ExecStart=`
///   follows a moved binary;
/// - `andris/` is created with mode 0700 if it is missing, and left alone
///   if not;
/// - `andris/ticket` is written with a new random ticket, mode 0600, only
///   if there is no ticket there. It is created exclusively, so that a
///   ticket made at the same moment by something else is never
///   clobbered.
///
/// The unit reads `andris/` from `%E`, which systemd resolves to the user
/// manager's `$XDG_CONFIG_HOME`, or `~/.config`, so the unit finds what
/// this writes as long as `config_home` is the directory the manager
/// would name; the manager also looks for units under that directory.
///
/// It fails without writing anything if `exe` cannot be named in the unit
/// (see [`quote_executable`]).
pub fn install(config_home: &Path, exe: &Path) -> Result<Report> {
    let rendered = render_unit(UNIT_TEMPLATE, exe)?;

    let unit_dir = config_home.join("systemd").join("user");
    fs::create_dir_all(&unit_dir)
        .with_context(|| format!("cannot create the unit directory {}", unit_dir.display()))?;
    let unit_path = unit_dir.join(UNIT_NAME);
    let unit = write_unit(&unit_path, &rendered)?;

    let config_dir = config_home.join("andris");
    let config_dir_state = make_config_dir(&config_dir)?;

    let ticket_path = config_dir.join("ticket");
    let ticket = make_ticket(&ticket_path)?;

    Ok(Report {
        unit_path,
        unit,
        exe: exe.to_path_buf(),
        config_dir,
        config_dir_state,
        ticket_path,
        ticket,
    })
}

/// Write `contents` to the unit file at `path`, unless it already holds
/// exactly that.
///
/// Whatever is at `path` is removed first, rather than written through,
/// so that a symbolic link to the template in a checkout (as `systemctl
/// link` makes) is replaced by a file, and the checkout is not rewritten.
fn write_unit(path: &Path, contents: &str) -> Result<UnitState> {
    let shown = path.display();
    let state = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            bail!("{shown} is a directory, where the unit file should be")
        }
        Ok(metadata) => {
            if metadata.is_file()
                && fs::read(path).with_context(|| format!("cannot read {shown}"))?
                    == contents.as_bytes()
            {
                return Ok(UnitState::Unchanged);
            }
            fs::remove_file(path).with_context(|| format!("cannot replace {shown}"))?;
            UnitState::Replaced
        }
        Err(err) if err.kind() == ErrorKind::NotFound => UnitState::Written,
        Err(err) => return Err(err).with_context(|| format!("cannot read {shown}")),
    };
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .with_context(|| format!("cannot write {shown}"))?;
    Ok(state)
}

/// Create the configuration directory at `path` with mode 0700, or check
/// the one already there.
fn make_config_dir(path: &Path) -> Result<ConfigDirState> {
    let shown = path.display();
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => return Ok(ConfigDirState::Created),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err).with_context(|| format!("cannot create {shown}")),
    }
    // Followed if it is a symbolic link: what matters is the directory
    // the ticket ends up in.
    let metadata = fs::metadata(path).with_context(|| format!("cannot read {shown}"))?;
    if !metadata.is_dir() {
        bail!("{shown} is not a directory, so it cannot hold andris's ticket");
    }
    let mode = metadata.permissions().mode() & 0o777;
    Ok(if mode & 0o077 == 0 {
        ConfigDirState::Existing
    } else {
        ConfigDirState::OpenToOthers { mode }
    })
}

/// Write a new ticket to `path` if there is no ticket there, or check the
/// one that is.
fn make_ticket(path: &Path) -> Result<TicketState> {
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    let mut file = match created {
        Ok(file) => file,
        // A ticket, or something else, is already there.
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            return Ok(match read_ticket_file(path) {
                Ok(_) => TicketState::Existing,
                Err(err) => TicketState::Refused(format!("{err:#}")),
            });
        }
        Err(err) => {
            return Err(err).with_context(|| format!("cannot create {}", path.display()));
        }
    };
    file.write_all(new_ticket().as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(TicketState::Created)
}

/// A new ticket: [`TICKET_RANDOM_BYTES`] random bytes, in base64, and a
/// newline, which reading a ticket strips. The bytes come from rand's
/// thread-local generator, a CSPRNG seeded by the operating system.
fn new_ticket() -> String {
    let bytes: [u8; TICKET_RANDOM_BYTES] = rand::random();
    let mut ticket = STANDARD.encode(bytes);
    ticket.push('\n');
    ticket
}

impl fmt::Display for Report {
    /// One line for each of the unit, the directory and the ticket.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = self.unit_path.display();
        let exe = self.exe.display();
        match self.unit {
            UnitState::Written => writeln!(f, "Wrote {unit}, which runs {exe}.")?,
            UnitState::Replaced => writeln!(
                f,
                "Replaced {unit}, which now runs {exe}. Put local changes in a drop-in \
                 (systemctl --user edit andris@), so that installing again keeps them."
            )?,
            UnitState::Unchanged => writeln!(f, "{unit} already runs {exe}; it is unchanged.")?,
        }

        let dir = self.config_dir.display();
        match self.config_dir_state {
            ConfigDirState::Created => writeln!(f, "Created {dir}, mode 0700.")?,
            ConfigDirState::Existing => writeln!(f, "{dir} already exists; it is unchanged.")?,
            ConfigDirState::OpenToOthers { mode } => writeln!(
                f,
                "{dir} already exists, but is open to group or other (mode {mode:04o}); \
                 run chmod 700 {dir}."
            )?,
        }

        let ticket = self.ticket_path.display();
        match &self.ticket {
            TicketState::Created => writeln!(f, "Wrote a new random ticket to {ticket}."),
            TicketState::Existing => writeln!(f, "{ticket} already exists; it is unchanged."),
            TicketState::Refused(reason) => writeln!(
                f,
                "{ticket} already exists, and is unchanged, but andris would refuse it: \
                 {reason}"
            ),
        }
    }
}

/// The commands to run after installing, for `DISPLAY`'s value
/// `display`. Only the value is read; the display is never opened.
///
/// They restart the instance rather than `enable --now` it: `--now` does
/// nothing to an instance that is already running, which would leave the
/// old binary serving after a reinstall, under a unit that names the new
/// one. Restarting starts an instance that is stopped, so the same
/// commands serve a first install, a replaced unit, and an unchanged one
/// whose binary an upgrade replaced in place.
///
/// When `display` names a local display, the commands name its instance.
/// Otherwise they carry a placeholder, and say where the docs tell how to
/// find the number.
pub fn next_steps(display: Option<&str>) -> String {
    let number = display.and_then(local_display_number);
    let instance = number.map_or_else(|| "N".to_string(), |number| number.to_string());
    let mut steps = String::from("Then, to start andris, run:\n\n");
    steps.push_str("    systemctl --user daemon-reload\n");
    steps.push_str(&format!("    systemctl --user enable andris@{instance}\n"));
    steps.push_str(&format!("    systemctl --user restart andris@{instance}\n"));
    steps.push_str(
        "\nRestarting starts andris if it is stopped. If it is running, it stops the old \
         process, dropping any client connected to it, so that the new unit and binary take \
         effect.\n",
    );
    match number {
        Some(number) => steps.push_str(&format!(
            "\nandris@{number} shares display :{number}, which DISPLAY names here.\n"
        )),
        None => steps.push_str(&format!(
            "\nReplace N with the number of the X display to share (10 for :10). How to find \
             it: {STARTING_DOC}\n"
        )),
    }
    steps
}

/// The display number of `display` if it names a display on this host
/// reached over its Unix socket: `:N`, `:N.S`, `unix:N` or `unix:N.S`. A
/// display with a host name, such as `localhost:10.0`, is reached over
/// TCP, and may be another machine's, so it gives `None`.
pub fn local_display_number(display: &str) -> Option<u32> {
    let rest = display
        .strip_prefix("unix:")
        .or_else(|| display.strip_prefix(':'))?;
    let number = rest.split_once('.').map_or(rest, |(number, _)| number);
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// The configuration directory, from the values of `XDG_CONFIG_HOME` and
/// `HOME`. The XDG Base Directory Specification says to ignore an
/// `XDG_CONFIG_HOME` that is empty or relative, and use `$HOME/.config`
/// in its place.
pub fn config_home(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(xdg) = xdg_config_home.map(PathBuf::from)
        && xdg.is_absolute()
    {
        return Ok(xdg);
    }
    match home.map(PathBuf::from) {
        Some(home) if home.is_absolute() => Ok(home.join(".config")),
        _ => bail!(
            "neither XDG_CONFIG_HOME nor HOME names an absolute directory, so there is nowhere \
             to install the unit"
        ),
    }
}

/// Refuse to install when `euid`, the effective user, is root.
///
/// The unit is a *user* unit, installed into a directory the environment
/// names. Under sudo with `HOME` or `XDG_CONFIG_HOME` kept, root would
/// create directories, follow symbolic links (`andris/` among them, on
/// purpose) and remove and write files in a directory the user chose,
/// and leave root-owned files in the user's configuration besides.
pub fn refuse_root(euid: u32) -> Result<()> {
    if euid == 0 {
        bail!(
            "andris install-unit installs a systemd user unit, and must be run as the user whose \
             desktop andris will export, not as root or under sudo"
        );
    }
    Ok(())
}

/// `andris install-unit`: install for the running binary, under the
/// configuration directory the environment names, and print what was
/// done and what to run next.
pub fn run() -> Result<()> {
    refuse_root(effective_uid()?)?;
    let exe = std::env::current_exe().context("cannot find andris's own path")?;
    // The kernel's name for a binary deleted since it started ends
    // " (deleted)"; a unit naming it would never start.
    if !exe.is_file() {
        bail!(
            "andris's own path, {}, is not a file: was it removed or replaced while running?",
            exe.display()
        );
    }
    let config_home = config_home(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )?;
    let report = install(&config_home, &exe)?;
    let display = std::env::var("DISPLAY").ok();
    print!("{report}\n{}", next_steps(display.as_deref()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::MetadataExt;

    use tempfile::TempDir;

    /// A small template, with the real one's `ExecStart=` line.
    const TEMPLATE: &str = "[Service]\n\
        # ExecStart=andris in a comment is not a setting\n\
        ExecStart=andris --display :%i --ticket-file %E/andris/ticket $ANDRIS_ARGS\n\
        Restart=on-failure\n";

    fn exec_start(unit: &str) -> &str {
        unit.lines()
            .find(|line| line.starts_with("ExecStart="))
            .unwrap()
    }

    #[test]
    fn renders_a_plain_path() {
        let unit = render_unit(TEMPLATE, Path::new("/home/alice/.local/bin/andris")).unwrap();
        assert_eq!(
            exec_start(&unit),
            "ExecStart=\"/home/alice/.local/bin/andris\" --display :%i --ticket-file \
             %E/andris/ticket $ANDRIS_ARGS"
        );
        // Only that line changes.
        assert_eq!(
            unit.replace("\"/home/alice/.local/bin/andris\"", "andris"),
            TEMPLATE
        );
    }

    #[test]
    fn renders_a_path_with_a_space_a_percent_and_a_dollar() {
        let unit = render_unit(TEMPLATE, Path::new("/opt/my apps/100%/$HOME/andris")).unwrap();
        assert!(
            exec_start(&unit)
                .starts_with("ExecStart=\"/opt/my apps/100%%/$HOME/andris\" --display"),
            "{unit}"
        );
        // Non-ASCII text needs nothing.
        let unit = render_unit(TEMPLATE, Path::new("/opt/café/andris")).unwrap();
        assert!(exec_start(&unit).starts_with("ExecStart=\"/opt/café/andris\" "));
    }

    #[test]
    fn refuses_quotes_and_backslashes_systemd_would_refuse() {
        for path in ["/opt/a\"b/andris", "/opt/a'b/andris", "/opt/a\\b/andris"] {
            let message = format!("{:#}", render_unit(TEMPLATE, Path::new(path)).unwrap_err());
            assert!(message.contains("systemd refuses"), "{path}: {message}");
        }
    }

    #[test]
    fn refuses_a_newline_and_other_control_characters() {
        for path in [
            "/opt/a\nb/andris",
            "/opt/a\tb/andris",
            "/opt/a\u{7f}b/andris",
        ] {
            let message = format!("{:#}", render_unit(TEMPLATE, Path::new(path)).unwrap_err());
            assert!(message.contains("control character"), "{path:?}: {message}");
        }
    }

    #[test]
    fn refuses_a_relative_or_non_utf8_path() {
        assert!(render_unit(TEMPLATE, Path::new("bin/andris")).is_err());
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/opt/\xff/andris"));
        let message = format!("{:#}", render_unit(TEMPLATE, path).unwrap_err());
        assert!(message.contains("not UTF-8"), "{message}");
    }

    #[test]
    fn refuses_a_template_without_exactly_one_exec_start() {
        let exe = Path::new("/usr/bin/andris");
        let none = TEMPLATE.replace("ExecStart=andris", "ExecStop=andris");
        let message = format!("{:#}", render_unit(&none, exe).unwrap_err());
        assert!(message.contains("0 ExecStart= lines"), "{message}");

        let two = format!("{TEMPLATE}ExecStart=andris --again\n");
        let message = format!("{:#}", render_unit(&two, exe).unwrap_err());
        assert!(message.contains("2 ExecStart= lines"), "{message}");

        // One line, but not starting with a bare andris.
        for other in ["/usr/bin/andris", "andrisx", "-andris"] {
            let template = TEMPLATE.replace("ExecStart=andris", &format!("ExecStart={other}"));
            let message = format!("{:#}", render_unit(&template, exe).unwrap_err());
            assert!(message.contains("bare andris"), "{other}: {message}");
        }
    }

    #[test]
    fn the_embedded_unit_renders_and_filters_system_calls() {
        let unit = render_unit(UNIT_TEMPLATE, Path::new("/usr/bin/andris")).unwrap();
        assert!(exec_start(&unit).starts_with("ExecStart=\"/usr/bin/andris\" --display :%i "));
        assert!(
            unit.lines()
                .any(|line| line == "SystemCallFilter=@system-service"),
            "{unit}"
        );
        // Without it, a call made through another ABI escapes the filter.
        assert!(
            unit.lines()
                .any(|line| line == "SystemCallArchitectures=native"),
            "{unit}"
        );
        // NoNewPrivileges=yes is what lets a user unit filter calls.
        assert!(unit.lines().any(|line| line == "NoNewPrivileges=yes"));
        // The unit reads its configuration from %E, the directory
        // `install` puts andris/ in, not from a fixed ~/.config.
        assert!(
            exec_start(&unit).contains(" --ticket-file %E/andris/ticket "),
            "{unit}"
        );
        assert!(
            unit.lines()
                .any(|line| line == "EnvironmentFile=-%E/andris/andris.env"),
            "{unit}"
        );
        assert!(!unit.contains("%h"), "{unit}");
    }

    /// A configuration home and an executable path for one test.
    fn home() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin").join("andris");
        (dir, exe)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn installs_into_an_empty_home() {
        let (dir, exe) = home();
        let report = install(dir.path(), &exe).unwrap();

        assert_eq!(
            report.unit_path,
            dir.path().join("systemd/user/andris@.service")
        );
        assert_eq!(report.unit, UnitState::Written);
        let unit = fs::read_to_string(&report.unit_path).unwrap();
        assert_eq!(unit, render_unit(UNIT_TEMPLATE, &exe).unwrap());

        assert_eq!(report.config_dir, dir.path().join("andris"));
        assert_eq!(report.config_dir_state, ConfigDirState::Created);
        assert_eq!(mode(&report.config_dir), 0o700);

        assert_eq!(report.ticket, TicketState::Created);
        assert_eq!(mode(&report.ticket_path), 0o600);
        // andris accepts the ticket it made: 32 characters of base64.
        let ticket = read_ticket_file(&report.ticket_path).unwrap();
        assert_eq!(ticket.as_bytes().len(), 32);
        assert!(
            ticket
                .as_bytes()
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/'),
            "{:?}",
            String::from_utf8_lossy(ticket.as_bytes())
        );

        let shown = report.to_string();
        assert!(shown.contains("Wrote "), "{shown}");
        assert!(shown.contains("Created "), "{shown}");
        assert!(shown.contains("new random ticket"), "{shown}");
    }

    #[test]
    fn installing_again_changes_nothing_and_keeps_the_ticket() {
        let (dir, exe) = home();
        let first = install(dir.path(), &exe).unwrap();
        let ticket = fs::read(&first.ticket_path).unwrap();

        let again = install(dir.path(), &exe).unwrap();
        assert_eq!(again.unit, UnitState::Unchanged);
        assert_eq!(again.config_dir_state, ConfigDirState::Existing);
        assert_eq!(again.ticket, TicketState::Existing);
        assert_eq!(fs::read(&again.ticket_path).unwrap(), ticket);
        assert!(again.to_string().contains("unchanged"));
    }

    #[test]
    fn a_moved_binary_replaces_the_unit() {
        let (dir, exe) = home();
        install(dir.path(), &exe).unwrap();
        let moved = dir.path().join("elsewhere").join("andris");
        let report = install(dir.path(), &moved).unwrap();
        assert_eq!(report.unit, UnitState::Replaced);
        let unit = fs::read_to_string(&report.unit_path).unwrap();
        assert!(unit.contains(&format!("ExecStart=\"{}\" ", moved.display())));
        assert!(report.to_string().contains("drop-in"));
    }

    #[test]
    fn a_symbolic_link_is_replaced_not_written_through() {
        let (dir, exe) = home();
        let checkout = dir.path().join("andris@.service.in-a-checkout");
        fs::write(&checkout, UNIT_TEMPLATE).unwrap();
        let unit_dir = dir.path().join("systemd/user");
        fs::create_dir_all(&unit_dir).unwrap();
        std::os::unix::fs::symlink(&checkout, unit_dir.join(UNIT_NAME)).unwrap();

        let report = install(dir.path(), &exe).unwrap();
        assert_eq!(report.unit, UnitState::Replaced);
        assert!(
            fs::symlink_metadata(&report.unit_path)
                .unwrap()
                .file_type()
                .is_file()
        );
        assert_eq!(fs::read_to_string(&checkout).unwrap(), UNIT_TEMPLATE);
    }

    #[test]
    fn an_existing_open_directory_is_reported_and_left_alone() {
        let (dir, exe) = home();
        let config_dir = dir.path().join("andris");
        fs::create_dir(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755)).unwrap();

        let report = install(dir.path(), &exe).unwrap();
        assert_eq!(
            report.config_dir_state,
            ConfigDirState::OpenToOthers { mode: 0o755 }
        );
        assert_eq!(mode(&config_dir), 0o755);
        let shown = report.to_string();
        assert!(shown.contains("mode 0755"), "{shown}");
        assert!(shown.contains("chmod 700"), "{shown}");
    }

    #[test]
    fn an_existing_ticket_is_never_overwritten() {
        let (dir, exe) = home();
        let config_dir = dir.path().join("andris");
        DirBuilder::new().mode(0o700).create(&config_dir).unwrap();
        let ticket = config_dir.join("ticket");
        fs::write(&ticket, "mine, all mine\n").unwrap();
        fs::set_permissions(&ticket, fs::Permissions::from_mode(0o600)).unwrap();

        let report = install(dir.path(), &exe).unwrap();
        assert_eq!(report.ticket, TicketState::Existing);
        assert_eq!(fs::read_to_string(&ticket).unwrap(), "mine, all mine\n");

        // One andris would refuse is still left alone, and the report
        // says why.
        fs::set_permissions(&ticket, fs::Permissions::from_mode(0o644)).unwrap();
        let report = install(dir.path(), &exe).unwrap();
        let TicketState::Refused(reason) = &report.ticket else {
            panic!("{:?}", report.ticket);
        };
        assert!(reason.contains("chmod 600"), "{reason}");
        assert_eq!(fs::read_to_string(&ticket).unwrap(), "mine, all mine\n");
        assert!(report.to_string().contains("would refuse it"));
    }

    #[test]
    fn an_unusable_path_writes_nothing() {
        let (dir, _) = home();
        assert!(install(dir.path(), Path::new("/opt/a\nb/andris")).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn new_tickets_differ() {
        assert_ne!(new_ticket(), new_ticket());
        assert_eq!(new_ticket().len(), 33);
    }

    #[test]
    fn local_display_numbers() {
        for (display, number) in [
            (":10", Some(10)),
            (":10.0", Some(10)),
            (":0", Some(0)),
            ("unix:10", Some(10)),
            ("unix:10.1", Some(10)),
            ("localhost:10.0", None),
            ("remote:0", None),
            (":", None),
            (":x", None),
            (":+1", None),
            ("", None),
        ] {
            assert_eq!(local_display_number(display), number, "{display:?}");
        }
    }

    #[test]
    fn next_steps_name_a_local_display() {
        let steps = next_steps(Some(":10.0"));
        assert!(
            steps.contains("systemctl --user daemon-reload\n"),
            "{steps}"
        );
        assert!(
            steps.contains("systemctl --user enable andris@10\n"),
            "{steps}"
        );
        assert!(
            steps.contains("systemctl --user restart andris@10\n"),
            "{steps}"
        );
        // --now does nothing to a running instance, so it would leave the
        // old binary serving after a reinstall.
        assert!(!steps.contains("--now"), "{steps}");
        assert!(!steps.contains(STARTING_DOC), "{steps}");
    }

    #[test]
    fn next_steps_otherwise_carry_a_placeholder() {
        for display in [None, Some("localhost:10.0"), Some("")] {
            let steps = next_steps(display);
            assert!(
                steps.contains("systemctl --user enable andris@N\n"),
                "{steps}"
            );
            assert!(
                steps.contains("systemctl --user restart andris@N\n"),
                "{steps}"
            );
            assert!(steps.contains(STARTING_DOC), "{steps}");
        }
    }

    #[test]
    fn root_is_refused() {
        let message = format!("{:#}", refuse_root(0).unwrap_err());
        assert!(message.contains("not as root or under sudo"), "{message}");
        for euid in [1, 1000, 65534] {
            assert!(refuse_root(euid).is_ok(), "{euid}");
        }
    }

    #[test]
    fn config_home_follows_the_xdg_specification() {
        let some = |text: &str| Some(OsString::from(text));
        assert_eq!(
            config_home(some("/x/config"), some("/home/alice")).unwrap(),
            PathBuf::from("/x/config")
        );
        for ignored in [None, some(""), some("relative/config")] {
            assert_eq!(
                config_home(ignored, some("/home/alice")).unwrap(),
                PathBuf::from("/home/alice/.config")
            );
        }
        assert!(config_home(None, None).is_err());
        assert!(config_home(None, some("relative")).is_err());
    }
}
