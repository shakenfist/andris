//! The andris binary: parse the command line, set up logging, and hand
//! over to the library.

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use andris::config::Args;

/// The exit status for a configuration andris refused, the same status
/// clap uses for a malformed command line. A supervisor should not
/// restart andris after it: nothing changes until the configuration
/// does. Runtime failures, the desktop going away among them, exit 1.
const CONFIGURATION_REFUSED: u8 = 2;

fn main() -> Result<ExitCode> {
    let args = Args::parse();

    // RUST_LOG picks what is logged, and info is the default. Logs go to
    // stderr.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
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
