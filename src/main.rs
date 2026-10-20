mod cli;
mod commands;
mod common;
mod media;
mod subs;
mod translate;
mod tui;

use anyhow::Result;
use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    common::logging::init(cli.verbose, cli.quiet);

    // Best-effort config load (missing files => defaults).
    let cfg = common::config::AppConfig::load(cli.config.as_deref());
    tracing::debug!(?cfg, "config loaded");
    common::tools::init(&cfg.tools);

    if let Err(err) = commands::dispatch(cli, cfg).await {
        // Missing external tools get their own exit code so scripts can tell
        // "install something" apart from a failed job.
        if err.downcast_ref::<common::tools::Missing>().is_some() {
            eprintln!("Error: {err}");
            std::process::exit(3);
        }
        return Err(err);
    }
    Ok(())
}
