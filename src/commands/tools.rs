use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::common::config::AppConfig;
use crate::common::tools::{self, Resolver, Tool};

/// `subx tools check|install|where` — external tool management.
#[derive(Debug, Args)]
pub struct ToolsArgs {
    #[command(subcommand)]
    pub cmd: ToolsCmd,
}

#[derive(Debug, Subcommand)]
pub enum ToolsCmd {
    /// Show every tool: status, version, location, and how to get a missing one.
    Check,
    /// Download, verify and unpack tools into the tools folder (`all` = every installable one).
    Install {
        /// Tool names (`subx tools check` lists them) or `all`.
        #[arg(required = true)]
        names: Vec<String>,
        /// Skip checksum verification entirely.
        #[arg(long)]
        no_verify: bool,
        /// Install root (default: `[tools] dir`, else `tools/` next to subx).
        #[arg(long)]
        tools_dir: Option<PathBuf>,
    },
    /// Print the resolved path of one tool (exit 3 when missing).
    Where {
        /// Tool name, e.g. ffmpeg, mkvmerge, svt-av1.
        name: String,
    },
}

fn tool_named(name: &str) -> Result<Tool> {
    Tool::from_name(name).with_context(|| {
        format!(
            "unknown tool '{name}'; known: {}",
            Tool::ALL
                .iter()
                .map(|t| t.name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

fn check(resolver: &Resolver) {
    println!("{:<11} {:<9} {:<26} LOCATION", "TOOL", "STATUS", "VERSION");
    let mut missing = 0;
    for tool in Tool::ALL {
        match resolver.probe(tool) {
            Some(found) if found.usable => println!(
                "{:<11} {:<9} {:<26} {} ({})",
                tool.name(),
                "ok",
                found.version.as_deref().unwrap_or("?"),
                found.path.display(),
                found.origin
            ),
            Some(found) => {
                missing += 1;
                println!(
                    "{:<11} {:<9} {:<26} {} ({})",
                    tool.name(),
                    "unusable",
                    found.version.as_deref().unwrap_or("?"),
                    found.path.display(),
                    found.origin
                );
                println!(
                    "{:<11} {}",
                    "",
                    found.detail.as_deref().unwrap_or("cannot run")
                );
            }
            None => {
                missing += 1;
                println!(
                    "{:<11} {:<9} {:<26} {}",
                    tool.name(),
                    "missing",
                    "-",
                    tool.purpose()
                );
                if tool.source().is_some() {
                    println!(
                        "{:<11} install: subx tools install {}  |  {}",
                        "",
                        tool.name(),
                        tool.link()
                    );
                } else {
                    println!("{:<11} download: {}", "", tool.link());
                }
            }
        }
    }
    println!(
        "{}",
        if missing == 0 {
            "All tools found.".to_string()
        } else {
            format!("{missing} tool(s) missing or unusable (optional ones included).")
        }
    );
}

pub async fn run(args: ToolsArgs, cfg: &AppConfig) -> Result<()> {
    match args.cmd {
        ToolsCmd::Check => {
            check(tools::resolver());
            Ok(())
        }
        ToolsCmd::Where { name } => {
            let tool = tool_named(&name)?;
            let path = tools::require_one(tool)?;
            println!("{}", path.display());
            Ok(())
        }
        ToolsCmd::Install {
            names,
            no_verify,
            tools_dir,
        } => {
            let root = match tools_dir {
                Some(dir) => dir,
                None => tools::default_install_root(&cfg.tools)?,
            };
            let wanted: Vec<Tool> = if names.iter().any(|n| n.eq_ignore_ascii_case("all")) {
                Tool::ALL
                    .into_iter()
                    .filter(|t| t.install_target() == *t && t.source().is_some())
                    .collect()
            } else {
                names
                    .iter()
                    .map(|n| tool_named(n))
                    .collect::<Result<Vec<_>>>()?
            };
            if wanted.is_empty() {
                bail!("nothing installable for this platform");
            }
            let mut failed = 0;
            for tool in wanted {
                match tools::install(tool, &root, !no_verify).await {
                    Ok(found) => println!(
                        "[OK] {} {} -> {}",
                        tool.name(),
                        found.version.as_deref().unwrap_or("?"),
                        found.path.display()
                    ),
                    Err(e) => {
                        failed += 1;
                        println!("[FAILED] {} -> {e:#}", tool.name());
                    }
                }
            }
            if failed > 0 {
                bail!("{failed} tool(s) failed to install");
            }
            Ok(())
        }
    }
}
