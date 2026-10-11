//! Command-line options, and their validation into a [`Config`].

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};

use crate::platform::Size;

/// The longest ticket SPICE allows, in bytes (`SPICE_MAX_PASSWORD_LENGTH`
/// in spice-protocol's `protocol.h`).
pub const MAX_TICKET_LEN: usize = 60;

/// The shortest frame interval `--frame-interval-ms` accepts.
pub const MIN_FRAME_INTERVAL_MS: u64 = 20;

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

    /// The TLS private key, as PEM.
    #[arg(long, value_name = "PATH")]
    pub tls_key: Option<PathBuf>,

    /// A file holding the SPICE ticket (password) clients must present.
    /// It must give group and other no access, and holds at most 60
    /// bytes, plus an optional trailing newline.
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

    /// How often the desktop is captured, in milliseconds.
    #[arg(
        long,
        value_name = "MS",
        default_value_t = 200,
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

/// Read and check the ticket file named by `--ticket-file`.
///
/// It is refused if its mode gives group or other any access, if it is
/// empty, or if it holds more than [`MAX_TICKET_LEN`] bytes after one
/// trailing newline is stripped.
pub fn read_ticket_file(path: &Path) -> Result<Ticket> {
    let shown = path.display();
    let metadata =
        std::fs::metadata(path).with_context(|| format!("--ticket-file {shown} cannot be read"))?;
    if !metadata.is_file() {
        bail!("--ticket-file {shown} is not a regular file");
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "--ticket-file {shown} is open to group or other (mode {mode:04o}): run chmod 600 {shown}"
        );
    }
    let mut ticket =
        std::fs::read(path).with_context(|| format!("--ticket-file {shown} cannot be read"))?;
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
            "--ticket-file {shown} holds a {}-byte ticket; SPICE allows at most {MAX_TICKET_LEN} \
             bytes, so write a shorter one",
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
        assert_eq!(config.frame_interval, Duration::from_millis(200));
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
