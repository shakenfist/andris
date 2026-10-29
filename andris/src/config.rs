//! Command-line options, and their validation into a [`Config`].

use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use tracing::warn;

use crate::platform::Size;
use crate::platform::pulse::DEFAULT_SOURCE;
use crate::server::TlsOptions;
use crate::server::tls::load_server_config;
use crate::server::updates::JpegQuality;

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

/// `--jpeg-quality`'s default, spice-server's.
pub const DEFAULT_JPEG_QUALITY: u8 = JpegQuality::DEFAULT.get();

/// Which display system andris exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PlatformKind {
    /// An X11 display, named by `--display`.
    X11,
    /// A generated test pattern of `--synthetic-size`.
    Synthetic,
    /// A qemu guest's screen, from the D-Bus display of the qemu whose
    /// QMP socket is `--qmp`.
    #[cfg(feature = "qemu")]
    Qemu,
}

/// Andris's whole command line: either a subcommand, or the options of
/// [`Args`] for serving a desktop, never both.
///
/// Serving takes no subcommand, so `andris --display :10 --ticket-file
/// ...` reads as it always has. Once any of [`Args`]'s options has been
/// given, a later word is never taken for a subcommand
/// (`args_conflicts_with_subcommands`), and a subcommand frees the
/// command line from the options serving requires, `--ticket-file`
/// among them (`subcommand_negates_reqs`).
///
/// `--help` describes andris with the package's description, not with
/// this comment (`long_about = None`).
#[derive(Debug, Clone, Parser)]
#[command(
    name = "andris",
    version,
    about,
    long_about = None,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
pub struct Cli {
    /// What to do instead of serving, if anything.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// The options for serving a desktop, used when there is no
    /// subcommand.
    #[command(flatten)]
    pub serve: Args,
}

/// Andris's subcommands. Each runs instead of the server, and none reads
/// a ticket, opens a display or starts the async runtime.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Install the systemd user unit, and a ticket if there is none.
    ///
    /// Writes andris@.service to the user's systemd unit directory, naming
    /// this binary by its absolute path, and creates ~/.config/andris and
    /// a ticket in it if they are missing. It prints the systemctl
    /// commands to run next, and runs none itself. Run it again after
    /// moving andris, so that the unit follows.
    InstallUnit,
}

/// The options for serving a desktop: andris's command line when no
/// subcommand is given. It parses on its own too, as tests do.
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
    // Required, but an Option: a subcommand lifts the requirement (see
    // `Cli`), and clap's derive can only leave a field out that is an
    // Option.
    #[arg(long, value_name = "PATH", required = true)]
    pub ticket_file: Option<PathBuf>,

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

    /// The QMP socket of the qemu whose guest to export, for `--platform
    /// qemu`. qemu must run with `-display dbus,p2p=on`.
    #[cfg(feature = "qemu")]
    #[arg(long, value_name = "PATH")]
    pub qmp: Option<PathBuf>,

    /// The shortest time between two frames, in milliseconds: a cap on
    /// the frame rate. A frame is sent only when the desktop changes.
    #[arg(
        long,
        value_name = "MS",
        default_value_t = DEFAULT_FRAME_INTERVAL_MS,
        value_parser = clap::value_parser!(u64).range(MIN_FRAME_INTERVAL_MS..),
    )]
    pub frame_interval_ms: u64,

    /// The JPEG quality, 1 to 100, for areas that look photographic, when
    /// the client leaves the choice of compression to the server; 0 never
    /// sends JPEG. An area sent as JPEG is sent again losslessly once it
    /// has been still for half a second.
    #[arg(
        long,
        value_name = "QUALITY",
        default_value_t = DEFAULT_JPEG_QUALITY,
        value_parser = clap::value_parser!(u8).range(0..=100),
    )]
    pub jpeg_quality: u8,

    /// Capture no sound from the desktop.
    #[arg(long)]
    pub no_audio: bool,

    /// The PulseAudio source to capture the desktop's sound from, for
    /// `--platform x11`. The default, @DEFAULT_MONITOR@, is the monitor of
    /// the session's default sink: whatever the desktop plays. PipeWire's
    /// pipewire-pulse serves the same names.
    #[arg(long, value_name = "NAME", conflicts_with = "no_audio")]
    pub audio_source: Option<String>,
}

/// The platform to build, with what it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformConfig {
    /// An X11 display, by name (`:10`, say).
    X11 { display: String },
    /// The synthetic test pattern, at this size.
    Synthetic { size: Size },
    /// A qemu guest, through the QMP socket at this path.
    #[cfg(feature = "qemu")]
    Qemu { qmp: PathBuf },
}

/// Where the desktop's sound comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioConfig {
    /// Nowhere: `--no-audio`.
    Off,
    /// A PulseAudio source, by name: the default for `--platform x11`.
    Pulse { source: String },
    /// The synthetic platform's tone: the default for `--platform
    /// synthetic`.
    Tone,
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
    /// The TLS listener, with its certificate and key loaded, if there
    /// is one.
    pub tls: Option<TlsOptions>,
    pub ticket: Ticket,
    pub platform: PlatformConfig,
    pub frame_interval: Duration,
    /// The quality to send JPEG at, or `None` for `--jpeg-quality 0`.
    pub jpeg_quality: Option<JpegQuality>,
    pub audio: AudioConfig,
}

impl Args {
    /// Check the options together, read the ticket file and any TLS
    /// certificate and key, and produce a [`Config`]. Each error names
    /// the option at fault and says how to fix it. Every file andris
    /// needs is read here, so that a bad one is a refused configuration
    /// (exit status 2), which a supervisor does not retry.
    pub fn validate(self) -> Result<Config> {
        let tls = match (self.tls_listen, self.tls_cert, self.tls_key) {
            (Some(listen), Some(cert), Some(key)) => Some(TlsOptions {
                listen,
                config: load_server_config(&cert, &key)?,
            }),
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

        // clap requires --ticket-file when there is no subcommand, so it
        // is missing only from an `Args` built by hand.
        let Some(ticket_file) = self.ticket_file else {
            bail!("--ticket-file is required: name the file holding the SPICE ticket");
        };
        let ticket = read_ticket_file(&ticket_file)?;

        let audio = match (self.no_audio, self.platform, self.audio_source) {
            // clap refuses --no-audio with --audio-source.
            (true, _, _) => AudioConfig::Off,
            (false, PlatformKind::X11, None) => AudioConfig::Pulse {
                source: DEFAULT_SOURCE.to_string(),
            },
            (false, PlatformKind::X11, Some(source)) if source.is_empty() => bail!(
                "--audio-source is empty: name a PulseAudio source, or leave it out for the \
                 default sink's monitor"
            ),
            (false, PlatformKind::X11, Some(source)) => AudioConfig::Pulse { source },
            (false, PlatformKind::Synthetic, None) => AudioConfig::Tone,
            (false, PlatformKind::Synthetic, Some(source)) => bail!(
                "--audio-source {source} names a PulseAudio source, but --platform synthetic \
                 plays a tone of its own: leave --audio-source out, or use --platform x11"
            ),
            #[cfg(feature = "qemu")]
            (false, PlatformKind::Qemu, None) => AudioConfig::Off,
            #[cfg(feature = "qemu")]
            (false, PlatformKind::Qemu, Some(source)) => bail!(
                "--audio-source {source} names a PulseAudio source, but --platform qemu \
                 exports no sound: leave --audio-source out"
            ),
        };

        #[cfg(feature = "qemu")]
        if let Some(qmp) = &self.qmp
            && self.platform != PlatformKind::Qemu
        {
            bail!(
                "--qmp {} names qemu's QMP socket, which only --platform qemu uses: leave --qmp \
                 out, or use --platform qemu",
                qmp.display()
            );
        }

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
            #[cfg(feature = "qemu")]
            PlatformKind::Qemu => match self.qmp {
                Some(qmp) if !qmp.as_os_str().is_empty() => PlatformConfig::Qemu { qmp },
                _ => bail!(
                    "--platform qemu needs qemu's QMP socket: give --qmp, and run qemu with \
                     -display dbus,p2p=on and a -qmp unix socket"
                ),
            },
        };

        Ok(Config {
            listen: self.listen,
            tls,
            ticket,
            platform,
            frame_interval: Duration::from_millis(self.frame_interval_ms),
            // The parser allows 0 to 100, and 0 means none.
            jpeg_quality: JpegQuality::new(self.jpeg_quality),
            audio,
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
/// Linux-only, and this needs no new dependency, nor an `unsafe` call to
/// libc's `geteuid`.
pub fn effective_uid() -> Result<u32> {
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

    /// A self-signed certificate and its key, as PEM files of mode 0600.
    fn tls_files() -> (NamedTempFile, NamedTempFile) {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = ticket_file(generated.cert.pem().as_bytes(), 0o600);
        let key = ticket_file(generated.signing_key.serialize_pem().as_bytes(), 0o600);
        (cert, key)
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
        assert_eq!(config.jpeg_quality, JpegQuality::new(85));
        assert_eq!(config.audio, AudioConfig::Tone);
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
        let (cert, key) = tls_files();
        let config = parse(
            &ticket,
            &[
                "--tls-listen",
                "0.0.0.0:5901",
                "--tls-cert",
                cert.path().to_str().unwrap(),
                "--tls-key",
                key.path().to_str().unwrap(),
            ],
        )
        .validate()
        .unwrap();
        assert_eq!(
            config.tls.map(|tls| tls.listen),
            Some("0.0.0.0:5901".parse().unwrap())
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
        let (cert, key) = tls_files();
        assert!(
            parse(
                &ticket,
                &[
                    "--listen",
                    "0.0.0.0:5900",
                    "--tls-listen",
                    "0.0.0.0:5901",
                    "--tls-cert",
                    cert.path().to_str().unwrap(),
                    "--tls-key",
                    key.path().to_str().unwrap(),
                ],
            )
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn tls_certificate_and_key_are_loaded_by_validation() {
        // So that a bad one is refused before anything starts, rather
        // than failing while running, which systemd would retry.
        let ticket = ticket_file(b"secret", 0o600);
        let (cert, key) = tls_files();
        let tls = |cert: &str, key: &str| {
            error(parse(
                &ticket,
                &[
                    "--tls-listen",
                    "127.0.0.1:5901",
                    "--tls-cert",
                    cert,
                    "--tls-key",
                    key,
                ],
            ))
        };
        let cert = cert.path().to_str().unwrap();
        let key_path = key.path().to_str().unwrap();

        let message = tls("/nonexistent/c.pem", key_path);
        assert!(
            message.contains("--tls-cert /nonexistent/c.pem"),
            "{message}"
        );
        let message = tls(cert, "/nonexistent/k.pem");
        assert!(
            message.contains("--tls-key /nonexistent/k.pem"),
            "{message}"
        );
        // The certificate is not a key.
        let message = tls(cert, cert);
        assert!(message.contains("holds no private key"), "{message}");

        std::fs::set_permissions(key.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let message = tls(cert, key_path);
        assert!(message.contains("chmod 600"), "{message}");
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

    #[cfg(feature = "qemu")]
    #[test]
    fn qemu_needs_a_qmp_socket_and_has_no_sound() {
        let ticket = ticket_file(b"secret\n", 0o600);
        let path = ticket.path().to_str().unwrap();
        let config = Args::try_parse_from([
            "andris",
            "--ticket-file",
            path,
            "--platform",
            "qemu",
            "--qmp",
            "/run/qemu/qmp.sock",
        ])
        .unwrap()
        .validate()
        .unwrap();
        assert_eq!(
            config.platform,
            PlatformConfig::Qemu {
                qmp: PathBuf::from("/run/qemu/qmp.sock")
            }
        );
        assert_eq!(config.audio, AudioConfig::Off);

        let args =
            Args::try_parse_from(["andris", "--ticket-file", path, "--platform", "qemu"]).unwrap();
        let message = error(args);
        assert!(message.contains("--qmp"), "{message}");
        assert!(message.contains("p2p=on"), "{message}");

        let args = Args::try_parse_from([
            "andris",
            "--ticket-file",
            path,
            "--platform",
            "qemu",
            "--qmp",
            "/run/qemu/qmp.sock",
            "--audio-source",
            "sink.monitor",
        ])
        .unwrap();
        assert!(error(args).contains("--platform qemu"));

        for platform in ["x11", "synthetic"] {
            let args = Args::try_parse_from([
                "andris",
                "--ticket-file",
                path,
                "--platform",
                platform,
                "--display",
                ":10",
                "--qmp",
                "/run/qemu/qmp.sock",
            ])
            .unwrap();
            let message = error(args);
            assert!(message.contains("--qmp /run/qemu/qmp.sock"), "{message}");
            assert!(message.contains("only --platform qemu"), "{message}");
        }
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
    fn jpeg_quality_range() {
        let ticket = ticket_file(b"secret", 0o600);
        for (given, expected) in [("0", None), ("1", Some(1)), ("100", Some(100))] {
            let config = parse(&ticket, &["--jpeg-quality", given])
                .validate()
                .unwrap();
            assert_eq!(
                config.jpeg_quality.map(JpegQuality::get),
                expected,
                "--jpeg-quality {given}"
            );
        }
        let path = ticket.path().to_str().unwrap();
        for bad in ["101", "high"] {
            let message =
                Args::try_parse_from(["andris", "--ticket-file", path, "--jpeg-quality", bad])
                    .unwrap_err()
                    .to_string();
            assert!(message.contains("--jpeg-quality"), "{bad}: {message}");
        }
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
        Cli::command().debug_assert();
    }

    #[test]
    fn serving_options_parse_as_before_the_subcommand() {
        let ticket = ticket_file(b"secret", 0o600);
        let path = ticket.path().to_str().unwrap();
        let cli = Cli::try_parse_from([
            "andris",
            "--display",
            ":10",
            "--ticket-file",
            path,
            "--listen",
            "127.0.0.1:5910",
            "--no-audio",
        ])
        .unwrap();
        assert_eq!(cli.command, None);
        let config = cli.serve.validate().unwrap();
        assert_eq!(config.listen, "127.0.0.1:5910".parse().unwrap());
        assert_eq!(
            config.platform,
            PlatformConfig::X11 {
                display: ":10".into()
            }
        );
        assert_eq!(config.audio, AudioConfig::Off);
    }

    #[test]
    fn serving_still_requires_a_ticket_file() {
        let message = Cli::try_parse_from(["andris", "--display", ":10"])
            .unwrap_err()
            .to_string();
        assert!(message.contains("--ticket-file"), "{message}");
        assert!(Cli::try_parse_from(["andris"]).is_err());
    }

    #[test]
    fn install_unit_needs_no_serving_options() {
        let cli = Cli::try_parse_from(["andris", "install-unit"]).unwrap();
        assert_eq!(cli.command, Some(Command::InstallUnit));
        assert_eq!(cli.serve.ticket_file, None);
    }

    #[test]
    fn install_unit_takes_no_options_and_mixes_with_none() {
        let ticket = ticket_file(b"secret", 0o600);
        let path = ticket.path().to_str().unwrap();
        // Serving options before it make it a stray word, not a
        // subcommand; options after it are not its own.
        for argv in [
            &["andris", "--ticket-file", path, "install-unit"][..],
            &["andris", "install-unit", "--ticket-file", path][..],
            &["andris", "install-unit", "--display", ":10"][..],
            &["andris", "install-unit", "extra"][..],
        ] {
            assert!(Cli::try_parse_from(argv).is_err(), "{argv:?} parsed");
        }
    }

    #[test]
    fn a_hand_built_args_without_a_ticket_file_is_refused() {
        let ticket = ticket_file(b"secret", 0o600);
        let mut args = parse(&ticket, &[]);
        args.ticket_file = None;
        let message = error(args);
        assert!(message.contains("--ticket-file is required"), "{message}");
    }

    /// Parse `extra` after a good ticket file and `--platform x11
    /// --display :10`.
    fn parse_x11(ticket: &NamedTempFile, extra: &[&str]) -> Result<Args, clap::Error> {
        let path = ticket.path().to_str().unwrap();
        let mut argv = vec!["andris", "--ticket-file", path, "--display", ":10"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv)
    }

    #[test]
    fn x11_captures_the_default_monitor_unless_told_otherwise() {
        let ticket = ticket_file(b"secret", 0o600);
        let config = parse_x11(&ticket, &[]).unwrap().validate().unwrap();
        assert_eq!(
            config.audio,
            AudioConfig::Pulse {
                source: "@DEFAULT_MONITOR@".into()
            }
        );
        let config = parse_x11(&ticket, &["--audio-source", "xrdp-sink.monitor"])
            .unwrap()
            .validate()
            .unwrap();
        assert_eq!(
            config.audio,
            AudioConfig::Pulse {
                source: "xrdp-sink.monitor".into()
            }
        );
        let message = error(parse_x11(&ticket, &["--audio-source", ""]).unwrap());
        assert!(message.contains("--audio-source is empty"), "{message}");
    }

    #[test]
    fn no_audio_turns_sound_off_on_either_platform() {
        let ticket = ticket_file(b"secret", 0o600);
        let config = parse_x11(&ticket, &["--no-audio"])
            .unwrap()
            .validate()
            .unwrap();
        assert_eq!(config.audio, AudioConfig::Off);
        let config = parse(&ticket, &["--no-audio"]).validate().unwrap();
        assert_eq!(config.audio, AudioConfig::Off);
    }

    #[test]
    fn no_audio_and_audio_source_conflict() {
        let ticket = ticket_file(b"secret", 0o600);
        let message = parse_x11(&ticket, &["--no-audio", "--audio-source", "x"])
            .unwrap_err()
            .to_string();
        assert!(message.contains("--no-audio"), "{message}");
        assert!(message.contains("--audio-source"), "{message}");
    }

    #[test]
    fn the_synthetic_platform_refuses_an_audio_source() {
        let ticket = ticket_file(b"secret", 0o600);
        let message = error(parse(&ticket, &["--audio-source", "sink.monitor"]));
        assert!(message.contains("--audio-source sink.monitor"), "{message}");
        assert!(message.contains("--platform synthetic"), "{message}");
    }
}
