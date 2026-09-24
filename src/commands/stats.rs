use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::subs::{batch, stats};

/// `subx stats <IN>` — QC report: reading speed, durations, overlaps.
#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Input subtitle file or folder.
    pub input: PathBuf,

    /// CPS limit for "too fast" warnings.
    #[arg(long, default_value_t = 20.0)]
    pub cps: f64,

    /// JSON output.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: StatsArgs) -> Result<()> {
    let files: Vec<PathBuf> = if args.input.is_file() {
        vec![args.input.clone()]
    } else if args.input.is_dir() {
        let exts: Vec<String> = crate::subs::supported_exts()
            .iter()
            .map(|s| s.to_string())
            .collect();
        batch::collect_subs(&args.input, &exts, false)?
    } else {
        anyhow::bail!("Input not found: {}", args.input.display());
    };
    if files.is_empty() {
        anyhow::bail!("No subtitle files found in {}", args.input.display());
    }
    let mut reports: Vec<(PathBuf, stats::FileStats)> = Vec::new();
    let mut failed = 0;
    for f in &files {
        match batch::load_any(f) {
            Ok(file) => reports.push((f.clone(), stats::analyze(&file.events, args.cps))),
            Err(e) => {
                failed += 1;
                println!("[FAILED] {} -> {e:#}", f.display());
            }
        }
    }
    if reports.is_empty() {
        anyhow::bail!("All files failed.");
    }
    if args.json {
        let out: Vec<serde_json::Value> = reports
            .iter()
            .map(|(f, s)| serde_json::json!({"file": f.to_string_lossy(), "stats": s}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        for (f, s) in &reports {
            println!(
                "{}: {} event(s), dur {}-{}ms, avg CPS {:.1}, max {:.1}",
                f.display(),
                s.events,
                s.min_dur_ms,
                s.max_dur_ms,
                s.avg_cps,
                s.max_cps
            );
            if !s.too_fast.is_empty() {
                let list: Vec<String> = s
                    .too_fast
                    .iter()
                    .map(|l| format!("#{}({:.1})", l.index, l.cps))
                    .collect();
                println!(
                    "  ! {} too fast (>{}): {}",
                    s.too_fast.len(),
                    args.cps,
                    list.join(" ")
                );
            }
            for (a, b) in &s.overlaps {
                println!("  ! overlap: #{a} -> #{b}");
            }
        }
        println!("--------------------------------------------------");
        println!("Files checked: {}, failed: {failed}", reports.len());
    }
    Ok(())
}
