//! Command-line options, and their validation into a [`Config`].

use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use tracing::warn;

use crate::platform::Size;

/// The longest ticket SPICE allows, in bytes (`SPICE_MAX_PASSWORD_LENGTH`
/// in spice-protocol's `protocol.h`).
pub const MAX_TICKET_LEN: usize = 60;

/// The shortest ticket andris does not warn about, in bytes. Wrong
/// guesses are only slowed, never locked out, so a ticket must be too
/// long to guess.
pub const RECOMMENDED_TICKET_LEN: usize = 16;

/// The shortest frame interval `--frame-interval-ms` accepts.
pub const MIN_FRAME_INTERVAL_MS: u64 = 20;

/// `--frame-interval-ms`'s default: at most about 30 frames a second.
pub const DEFAULT_FRAME_INTERVAL_MS: u64 = 33;

/// Which display system andris exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PlatformKind {
    /// An X11 display, named by `--display`.
    X11,
    /// A generated test pattern of `--synthetic-size`.
    Synthetic,
}

/// Andris's command line.
#[derive(Debug, Clone, Parser)]
#[command(name = "andris", version, about)]
pub struct Args {
    /// The plain TCP listener's address. It must be a loopback address
    /// unless a TLS listener is also configured.
    #[arg(long, value_name = "ADDR:PORT", default_value = "127.0.0.1:5900")]
    pub listen: SocketAddr,

    /// A TLS listener's address. With it, the plain listener redirects
    /// every client here. Needs `--tls-cert` and `--tls-key`.
    #[arg(long, value_name = "ADDR:PORT")]
    pub tls_listen: Option<SocketAddr>,

    /// The TLS certificate chain, as PEM.
    #[arg(long, value_name = "PATH")]
    pub tls_cert: Option<PathBuf>,

    /// The TLS private key, as PEM. Like the ticket file, it must be
    /// owned by andris's user and give group and other no access.
    #[arg(long, value_name = "PATH")]
    pub tls_key: Option<PathBuf>,

    /// A file holding the SPICE ticket (password) clients must present.
    /// It must be owned by andris's user, give group and other no access,
    /// and hold at most 60 bytes, plus an optional trailing newline.
    #[arg(long, value_name = "PATH")]
    pub ticket_file: PathBuf,

    /// The X display to export, for `--platform x11`.
    #[arg(long, value_name = "DISPLAY", env = "DISPLAY")]
    pub display: Option<String>,

    /// The display system to export.
    #[arg(long, value_enum, default_value_t = PlatformKind::X11)]
    pub platform: PlatformKind,

    /// The test pattern's size, as WIDTHxHEIGHT, for `--platform
    /// synthetic`.
    #[arg(long, value_name = "WxH", default_value = "1024x768", value_parser = parse_size)]
    pub synthetic_size: Size,

    /// The shortest time between two frames, in milliseconds: a cap on
    /// the frame rate. A frame is sent only when the desktop changes.
    #[arg(
        long,
        value_name = "MS",
        default_value_t = DEFAULT_FRAME_INTERVAL_MS,
        value_parser = clap::value_parser!(u64).range(MIN_FRAME_INTERVAL_MS..),
    )]
    pub frame_interval_ms: u64,
}

/// A TLS listener and the certificate it presents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    pub listen: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// The platform to build, with what it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformConfig {
    /// An X11 display, by name (`:10`, say).
    X11 { display: String },
    /// The synthetic test pattern, at this size.
    Synthetic { size: Size },
}

/// The SPICE ticket. Its `Debug` output does not show it.
#[derive(Clone, PartialEq, Eq)]
pub struct Ticket(Vec<u8>);

impl Ticket {
    /// A ticket of `bytes`, which must be 1 to [`MAX_TICKET_LEN`] bytes
    /// long. `--ticket-file` is read with [`read_ticket_file`]; this is
    /// for embedding andris, and for tests.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > MAX_TICKET_LEN {
            bail!(
                "a ticket must be 1 to {MAX_TICKET_LEN} bytes long, not {}",
                bytes.len()
            );
        }
        Ok(Self(bytes))
    }

    /// The ticket's bytes, as a client must present them.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ticket(<redacted>)")
    }
}

/// A validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The plain listener's address.
    pub listen: SocketAddr,
    /// The TLS listener, if there is one.
    pub tls: Option<TlsConfig>,
    pub ticket: Ticket,
    pub platform: PlatformConfig,
    pub frame_interval: Duration,
}

impl Args {
    /// Check the options together, read the ticket file, and produce a
    /// [`Config`]. Each error names the option at fault and says how to
    /// fix it.
    pub fn validate(self) -> Result<Config> {
        let tls = match (self.tls_listen, self.tls_cert, self.tls_key) {
            (Some(listen), Some(cert), Some(key)) => Some(TlsConfig { listen, cert, key }),
            (None, None, None) => None,
            _ => bail!(
                "--tls-listen, --tls-cert and --tls-key go together: give all three for a TLS \
                 listener, or none"
            ),
        };

        if tls.is_none() && !self.listen.ip().is_loopback() {
            bail!(
                "--listen {} is not a loopback address, and without TLS the ticket would cross \
                 the network in the clear: listen on 127.0.0.1 or ::1, or add --tls-listen, \
                 --tls-cert and --tls-key",
                self.listen
            );
        }

        let ticket = read_ticket_file(&self.ticket_file)?;

        let platform = match self.platform {
            PlatformKind::X11 => match self.display {
                Some(display) if !display.is_empty() => PlatformConfig::X11 { display },
                _ => bail!(
                    "--platform x11 needs a display: give --display (such as --display :10) or \
                     set DISPLAY"
                ),
            },
            PlatformKind::Synthetic => PlatformConfig::Synthetic {
                size: self.synthetic_size,
            },
        };

        Ok(Config {
            listen: self.listen,
            tls,
            ticket,
            platform,
            frame_interval: Duration::from_millis(self.frame_interval_ms),
        })
    }
}

/// Open a file that must be private to andris's user: `--ticket-file`
/// or `--tls-key`, named in errors as `option`.
///
/// It is refused unless it is a regular file, owned by andris's
/// effective uid, whose mode gives group and other no access. The checks
/// are made on the open file, so the file read is the file checked.
pub fn open_private_file(path: &Path, option: &str) -> Result<File> {
    let shown = path.display().to_string();
    // O_NONBLOCK, so that opening a FIFO with no writer cannot block
    // startup; the type check below then refuses it. Reads of a regular
    // file ignore the flag.
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("{option} {shown} cannot be read"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("{option} {shown} cannot be read"))?;
    check_private(
        option,
        &shown,
        metadata.is_file(),
        metadata.mode(),
        metadata.uid(),
        effective_uid()?,
    )?;
    Ok(file)
}

/// The checks [`open_private_file`] makes, on an open file's type, mode
/// and owner.
fn check_private(
    option: &str,
    shown: &str,
    is_file: bool,
    mode: u32,
    owner: u32,
    euid: u32,
) -> Result<()> {
    if !is_file {
        bail!("{option} {shown} is not a regular file");
    }
    let mode = mode & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{option} {shown} is open to group or other (mode {mode:04o}): run chmod 600 {shown}"
        );
    }
    if owner != euid {
        bail!(
            "{option} {shown} is owned by uid {owner}, not uid {euid} that andris runs as: run \
             chown $(id -un) {shown} as root, or run andris as its owner"
        );
    }
    Ok(())
}

/// The process's effective uid, from `/proc/self/status`. Andris is
/// Linux-only, and this needs no new dependency.
fn effective_uid() -> Result<u32> {
    let status = std::fs::read_to_string("/proc/self/status")
        .context("reading /proc/self/status for andris's effective uid")?;
    parse_effective_uid(&status).context("/proc/self/status names no effective uid")
}

/// The effective uid from a `/proc/<pid>/status` file: the second of the
/// `Uid:` line's real, effective, saved and filesystem uids.
fn parse_effective_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Read and check the ticket file named by `--ticket-file`.
///
/// It is refused if [`open_private_file`] refuses it, if it is empty, or
/// if it holds more than [`MAX_TICKET_LEN`] bytes after one trailing
/// newline is stripped. A ticket shorter than [`RECOMMENDED_TICKET_LEN`]
/// is accepted with a warning.
pub fn read_ticket_file(path: &Path) -> Result<Ticket> {
    let shown = path.display();
    let file = open_private_file(path, "--ticket-file")?;
    // Enough to tell a ticket that is too long, and no more.
    let mut ticket = Vec::new();
    file.take(MAX_TICKET_LEN as u64 + 2)
        .read_to_end(&mut ticket)
        .with_context(|| format!("--ticket-file {shown} cannot be read"))?;
    if ticket.last() == Some(&b'\n') {
        ticket.pop();
    }
    if ticket.is_empty() {
        bail!(
            "--ticket-file {shown} is empty: write a ticket of 1 to {MAX_TICKET_LEN} bytes to it"
        );
    }
    if ticket.len() > MAX_TICKET_LEN {
        bail!(
            "--ticket-file {shown} holds a ticket of more than {MAX_TICKET_LEN} bytes; SPICE \
             allows at most {MAX_TICKET_LEN} bytes, so write a shorter one"
        );
    }
    if ticket.len() < RECOMMENDED_TICKET_LEN {
        warn!(
            "--ticket-file {shown} holds a {}-byte ticket; use at least {RECOMMENDED_TICKET_LEN} \
             random bytes, because wrong guesses are only slowed, never locked out",
            ticket.len()
        );
    }
    Ok(Ticket(ticket))
}

/// Parse `--synthetic-size`, `WIDTHxHEIGHT`.
fn parse_size(text: &str) -> Result<Size> {
    let (width, height) = text
        .split_once('x')
        .with_context(|| format!("{text:?} is not WIDTHxHEIGHT, such as 1024x768"))?;
    let size = Size {
        width: width
            .parse()
            .with_context(|| format!("{width:?} is not a width in pixels"))?,
        height: height
            .parse()
            .with_context(|| format!("{height:?} is not a height in pixels"))?,
    };
    size.check()?;
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::NamedTempFile;

    /// A ticket file with `contents` and `mode`.
    fn ticket_file(contents: &[u8], mode: u32) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents).unwrap();
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        file
    }

    /// Parse `extra` after a good ticket file and the synthetic platform.
    fn parse(ticket: &NamedTempFile, extra: &[&str]) -> Args {
        let path = ticket.path().to_str().unwrap();
        let mut argv = vec!["andris", "--ticket-file", path, "--platform", "synthetic"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).unwrap()
    }

    fn error(args: Args) -> String {
        format!("{:#}", args.validate().unwrap_err())
    }

    #[test]
    fn defaults() {
        let ticket = ticket_file(b"secret\n", 0o600);
        let config = parse(&ticket, &[]).validate().unwrap();
        assert_eq!(config.listen, "127.0.0.1:5900".parse().unwrap());
        assert!(config.tls.is_none());
        assert_eq!(config.ticket.as_bytes(), b"secret");
        assert_eq!(
            config.platform,
            PlatformConfig::Synthetic {
                size: Size {
                    width: 1024,
                    height: 768
                }
            }
        );
        assert_eq!(config.frame_interval, Duration::from_millis(33));
    }

    #[test]
    fn ticket_file_is_required() {
        assert!(Args::try_parse_from(["andris"]).is_err());
    }

    #[test]
    fn ticket_is_not_in_debug_output() {
        let ticket = ticket_file(b"hunter2", 0o600);
        let config = parse(&ticket, &[]).validate().unwrap();
        assert!(!format!("{config:?}").contains("hunter2"));
    }

    #[test]
    fn tls_options_go_together() {
        let ticket = ticket_file(b"secret", 0o600);
        let config = parse(
            &ticket,
            &[
                "--tls-listen",
                "0.0.0.0:5901",
                "--tls-cert",
                "/c.pem",
                "--tls-key",
                "/k.pem",
            ],
        )
        .validate()
        .unwrap();
        assert_eq!(
            config.tls,
            Some(TlsConfig {
                listen: "0.0.0.0:5901".parse().unwrap(),
                cert: "/c.pem".into(),
                key: "/k.pem".into(),
            })
        );

        for partial in [
            &["--tls-listen", "0.0.0.0:5901"][..],
            &["--tls-cert", "/c.pem", "--tls-key", "/k.pem"][..],
            &["--tls-listen", "0.0.0.0:5901", "--tls-key", "/k.pem"][..],
        ] {
            let message = error(parse(&ticket, partial));
            assert!(
                message.contains("--tls-listen, --tls-cert and --tls-key"),
                "{message}"
            );
        }
    }

    #[test]
    fn non_loopback_plain_listener_needs_tls() {
        let ticket = ticket_file(b"secret", 0o600);
        let message = error(parse(&ticket, &["--listen", "0.0.0.0:5900"]));
        assert!(message.contains("--listen 0.0.0.0:5900"), "{message}");
        assert!(message.contains("--tls-listen"), "{message}");

        assert!(
            parse(&ticket, &["--listen", "[::1]:5900"])
                .validate()
                .is_ok()
        );

        // With TLS, the plain listener only redirects, so it may bind
        // anywhere.
        assert!(
            parse(
                &ticket,
                &[
                    "--listen",
                    "0.0.0.0:5900",
                    "--tls-listen",
                    "0.0.0.0:5901",
                    "--tls-cert",
                    "/c.pem",
                    "--tls-key",
                    "/k.pem",
                ],
            )
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn ticket_file_open_to_group_or_other_is_refused() {
        for mode in [0o644, 0o640, 0o604, 0o620] {
            let ticket = ticket_file(b"secret", mode);
            let message = error(parse(&ticket, &[]));
            assert!(message.contains("--ticket-file"), "{message}");
            assert!(message.contains("chmod 600"), "{message}");
        }
        assert!(read_ticket_file(ticket_file(b"secret", 0o400).path()).is_ok());
    }

    #[test]
    fn empty_ticket_is_refused() {
        for contents in [&b""[..], b"\n"] {
            let ticket = ticket_file(contents, 0o600);
            let message = error(parse(&ticket, &[]));
            assert!(message.contains("--ticket-file"), "{message}");
            assert!(message.contains("is empty"), "{message}");
        }
    }

    #[test]
    fn ticket_length_limit() {
        let longest = [b'a'; MAX_TICKET_LEN];
        assert_eq!(
            read_ticket_file(ticket_file(&longest, 0o600).path())
                .unwrap()
                .as_bytes(),
            longest
        );

        let mut with_newline = longest.to_vec();
        with_newline.push(b'\n');
        assert!(read_ticket_file(ticket_file(&with_newline, 0o600).path()).is_ok());

        let too_long = [b'a'; MAX_TICKET_LEN + 1];
        let ticket = ticket_file(&too_long, 0o600);
        let message = error(parse(&ticket, &[]));
        assert!(message.contains("--ticket-file"), "{message}");
        assert!(message.contains("at most 60"), "{message}");
    }

    #[test]
    fn only_one_trailing_newline_is_stripped() {
        let ticket = read_ticket_file(ticket_file(b"secret\n\n", 0o600).path()).unwrap();
        assert_eq!(ticket.as_bytes(), b"secret\n");
    }

    #[test]
    fn missing_ticket_file_names_the_option() {
        let message = format!(
            "{:#}",
            read_ticket_file(Path::new("/nonexistent/andris-ticket")).unwrap_err()
        );
        assert!(message.contains("--ticket-file"), "{message}");
    }

    #[test]
    fn ticket_file_is_checked_as_opened() {
        // A directory is not a regular file.
        let dir = tempfile::tempdir().unwrap();
        let message = format!("{:#}", read_ticket_file(dir.path()).unwrap_err());
        assert!(message.contains("not a regular file"), "{message}");
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("ticket");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        // In a thread, so that a blocking open fails the test rather
        // than hanging it.
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(format!("{:#}", read_ticket_file(&fifo).unwrap_err()));
        });
        let message = result
            .recv_timeout(Duration::from_secs(5))
            .expect("opening the FIFO blocked");
        assert!(message.contains("not a regular file"), "{message}");
    }

    #[test]
    fn private_file_owner_must_be_the_effective_uid() {
        assert!(check_private("--ticket-file", "t", true, 0o100600, 1000, 1000).is_ok());
        let message = format!(
            "{:#}",
            check_private("--ticket-file", "t", true, 0o100600, 0, 1000).unwrap_err()
        );
        assert!(
            message.contains("--ticket-file t is owned by uid 0"),
            "{message}"
        );
        assert!(message.contains("chown"), "{message}");
        let message = format!(
            "{:#}",
            check_private("--tls-key", "k", true, 0o100640, 1000, 1000).unwrap_err()
        );
        assert!(message.contains("--tls-key k is open"), "{message}");
        assert!(check_private("--tls-key", "k", false, 0o600, 1000, 1000).is_err());
    }

    #[test]
    fn effective_uid_owns_the_files_andris_creates() {
        let file = NamedTempFile::new().unwrap();
        let owner = file.as_file().metadata().unwrap().uid();
        assert_eq!(effective_uid().unwrap(), owner);
    }

    #[test]
    fn parses_the_effective_uid() {
        let status =
            "Name:\tandris\nUmask:\t0022\nUid:\t1000\t1001\t1002\t1003\nGid:\t5\t5\t5\t5\n";
        assert_eq!(parse_effective_uid(status), Some(1001));
        assert_eq!(parse_effective_uid("Name:\tandris\n"), None);
        assert_eq!(parse_effective_uid("Uid:\t1000\n"), None);
    }

    #[test]
    fn short_tickets_are_accepted() {
        // Shorter than RECOMMENDED_TICKET_LEN warns, but is not refused:
        // kerbside's demo uses an 11-byte ticket.
        let ticket = ticket_file(b"kerbside-11", 0o600);
        assert_eq!(
            read_ticket_file(ticket.path()).unwrap().as_bytes(),
            b"kerbside-11"
        );
    }

    #[test]
    fn x11_needs_a_display() {
        let ticket = ticket_file(b"secret", 0o600);
        let path = ticket.path().to_str().unwrap();
        let mut args =
            Args::try_parse_from(["andris", "--ticket-file", path, "--display", ":10"]).unwrap();
        assert_eq!(args.platform, PlatformKind::X11, "x11 is the default");
        assert_eq!(
            args.clone().validate().unwrap().platform,
            PlatformConfig::X11 {
                display: ":10".into()
            }
        );
        // Clear what --display or a DISPLAY in the test's environment set.
        args.display = None;
        let message = error(args);
        assert!(message.contains("--display"), "{message}");
    }

    #[test]
    fn synthetic_size() {
        let ticket = ticket_file(b"secret", 0o600);
        let config = parse(&ticket, &["--synthetic-size", "640x480"])
            .validate()
            .unwrap();
        assert_eq!(
            config.platform,
            PlatformConfig::Synthetic {
                size: Size {
                    width: 640,
                    height: 480
                }
            }
        );
        for bad in ["640", "0x480", "640x", "16385x1", "axb"] {
            let path = ticket.path().to_str().unwrap();
            let parsed =
                Args::try_parse_from(["andris", "--ticket-file", path, "--synthetic-size", bad]);
            let message = parsed.unwrap_err().to_string();
            assert!(message.contains("--synthetic-size"), "{bad}: {message}");
        }
    }

    #[test]
    fn frame_interval_minimum() {
        let ticket = ticket_file(b"secret", 0o600);
        let config = parse(&ticket, &["--frame-interval-ms", "20"])
            .validate()
            .unwrap();
        assert_eq!(config.frame_interval, Duration::from_millis(20));

        let path = ticket.path().to_str().unwrap();
        let message =
            Args::try_parse_from(["andris", "--ticket-file", path, "--frame-interval-ms", "19"])
                .unwrap_err()
                .to_string();
        assert!(message.contains("--frame-interval-ms"), "{message}");
    }

    #[test]
    fn ticket_new_checks_length() {
        assert_eq!(Ticket::new("secret").unwrap().as_bytes(), b"secret");
        assert!(Ticket::new("").is_err());
        assert!(Ticket::new(vec![b'x'; MAX_TICKET_LEN]).is_ok());
        assert!(Ticket::new(vec![b'x'; MAX_TICKET_LEN + 1]).is_err());
    }

    #[test]
    fn clap_definition_is_consistent() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }
}
