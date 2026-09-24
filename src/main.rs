// Phase 0 scaffold: many APIs are staged for Phases 1-6 and not all are used yet.
#![allow(dead_code)]

mod cli;
mod commands;
mod common;
mod media;
mod subs;
mod translate;

use anyhow::Result;
use clap::Parser;
use cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    common::logging::init(cli.verbose, cli.quiet);

    // Best-effort config load (missing file => defaults).
    let cfg = common::config::AppConfig::load(cli.config.as_deref());
    tracing::debug!(?cfg, "config loaded");

    commands::dispatch(cli).await
}
