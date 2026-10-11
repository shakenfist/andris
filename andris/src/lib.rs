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

use anyhow::{Context, Result};
use tracing::info;

use config::{Config, PlatformConfig};
use platform::Platform;
use platform::synthetic::SyntheticPlatform;
use platform::x11::X11Platform;
use server::{Server, ServerOptions, TlsOptions};

/// Andris's name and version, as `--version` prints them.
pub fn banner() -> String {
    format!("andris {}", env!("CARGO_PKG_VERSION"))
}

/// Run andris with a validated configuration: build the platform the
/// configuration names, start the listeners, and serve until Ctrl-C.
pub fn run(config: Config) -> Result<()> {
    let platform = match &config.platform {
        PlatformConfig::Synthetic { size } => {
            info!("exporting a synthetic {size} desktop");
            SyntheticPlatform::new(*size)?.platform()
        }
        PlatformConfig::X11 { display: name } => {
            info!("exporting X display {name}");
            X11Platform::connect(name)?.platform()
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    runtime.block_on(serve(config, platform))
}

/// Serve `platform` as `config` says, until Ctrl-C.
async fn serve(config: Config, platform: Platform) -> Result<()> {
    let size = platform.geometry.size()?;
    info!("the desktop is {size}");
    let tls = match &config.tls {
        Some(tls) => Some(TlsOptions {
            listen: tls.listen,
            config: server::tls::load_server_config(&tls.cert, &tls.key)?,
        }),
        None => None,
    };
    let options = ServerOptions {
        listen: config.listen,
        tls,
        ticket: config.ticket,
        frame_interval: config.frame_interval,
    };
    let handle = Server::bind(options, platform).await?.spawn()?;
    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl-C")?;
    info!("Ctrl-C: shutting down");
    handle.shutdown();
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
