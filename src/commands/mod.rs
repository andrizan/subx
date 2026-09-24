pub mod clean;
pub mod convert;
pub mod extract;
pub mod fetch;
pub mod filter;
pub mod mux;
pub mod probe;
pub mod shift;
pub mod stats;
pub mod translate;

use anyhow::Result;

use crate::cli::{Cli, Commands};

/// Dispatch the root CLI to each command handler (Phase 0 stub).
pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Extract(a) => extract::run(a).await,
        Commands::Clean(a) => clean::run(a).await,
        Commands::Shift(a) => shift::run(a).await,
        Commands::Filter(a) => filter::run(a).await,
        Commands::Mux(a) => mux::run(a).await,
        Commands::Convert(a) => convert::run(a).await,
        Commands::Translate(a) => translate::run(a).await,
        Commands::Probe(a) => probe::run(a).await,
        Commands::Fetch(a) => fetch::run(a).await,
        Commands::Stats(a) => stats::run(a).await,
    }
}
