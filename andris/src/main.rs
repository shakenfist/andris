//! The andris binary: parse the command line, set up logging, and hand
//! over to the library.

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use andris::config::Args;

fn main() -> Result<()> {
    let args = Args::parse();

    // RUST_LOG picks what is logged, and info is the default. Logs go to
    // stderr.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    info!("{}", andris::banner());
    andris::run(args.validate()?)
}
