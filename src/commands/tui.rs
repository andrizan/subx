use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::common::config::AppConfig;

/// `subx tui` — profiles, tools and encodes in a terminal UI.
#[derive(Debug, Args)]
pub struct TuiArgs {
    /// File the profile editor writes to (default: `--config` when given, else
    /// the existing `subx.local.toml` in the current folder or next to `subx`,
    /// else a new `subx.local.toml` in the current folder).
    #[arg(long)]
    pub write_to: Option<PathBuf>,
}

pub async fn run(args: TuiArgs, cfg: AppConfig, config_arg: Option<PathBuf>) -> Result<()> {
    let local = args
        .write_to
        .unwrap_or_else(|| crate::tui::default_local_path(config_arg.as_deref()));
    // The event loop blocks on terminal input; keep it off the async runtime.
    tokio::task::spawn_blocking(move || crate::tui::run(cfg, local, config_arg)).await?
}
