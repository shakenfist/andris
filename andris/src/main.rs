//! The andris binary: parse the command line, set up logging, and hand
//! over to the library.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use andris::config::{Cli, Command};

/// The exit status for a configuration andris refused, the same status
/// clap uses for a malformed command line. A supervisor should not
/// restart andris after it: nothing changes until the configuration
/// does. Runtime failures, the desktop going away among them, exit 1.
const CONFIGURATION_REFUSED: u8 = 2;

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    // A subcommand runs instead of the server: before logging is set up,
    // and without reading a ticket, opening a display or starting the
    // async runtime.
    match cli.command {
        Some(Command::InstallUnit) => {
            andris::install::run()?;
            return Ok(ExitCode::SUCCESS);
        }
        None => {}
    }
    let args = cli.serve;

    // RUST_LOG picks what is logged, and info is the default. Logs go to
    // stderr, in colour only when that is a terminal: under systemd it is
    // the journal, where escape codes are noise.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    info!("{}", andris::banner());
    let config = match args.validate() {
        Ok(config) => config,
        Err(err) => {
            // Printed as returning the error from main would print it.
            eprintln!("Error: {err:?}");
            return Ok(ExitCode::from(CONFIGURATION_REFUSED));
        }
    };
    andris::run(config)?;
    Ok(ExitCode::SUCCESS)
}
