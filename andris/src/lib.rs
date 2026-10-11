//! Andris, a SPICE server that exports an existing X11 desktop.
//!
//! The binary is a thin wrapper around this library, so that tests can
//! drive the server in-process.
//!
//! - [`config`] holds the command line and its validation;
//! - [`platform`] holds the seams between the server and the desktop,
//!   and their implementations;
//! - [`server`] holds the SPICE server.

pub mod config;
pub mod platform;
pub mod server;

use anyhow::{Result, bail};
use tracing::info;

use config::{Config, PlatformConfig};
use platform::synthetic::SyntheticPlatform;

/// Andris's name and version, as `--version` prints them.
pub fn banner() -> String {
    format!("andris {}", env!("CARGO_PKG_VERSION"))
}

/// Run andris with a validated configuration.
///
/// This builds the platform the configuration names. There is no SPICE
/// server yet, so it then returns.
pub fn run(config: Config) -> Result<()> {
    let platform = match &config.platform {
        PlatformConfig::Synthetic { size } => {
            info!("exporting a synthetic {size} desktop");
            SyntheticPlatform::new(*size)?.platform()
        }
        PlatformConfig::X11 { display } => {
            bail!(
                "--platform x11 (display {display}) is not available in this build: use \
                 --platform synthetic"
            )
        }
    };
    let size = platform.geometry.size()?;
    info!("the desktop is {size}; andris has no SPICE server yet, so it exits");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_has_name_and_version() {
        let banner = banner();
        let version = banner.strip_prefix("andris ").unwrap();
        assert!(!version.is_empty(), "no version in banner: {banner:?}");
        assert!(
            !version.chars().any(char::is_whitespace),
            "version contains whitespace: {version:?}"
        );
    }
}
