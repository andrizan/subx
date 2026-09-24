use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::subs::{batch, shift};

/// `subx shift <IN> <OUT> --shift-ms -1500` — shift timing + drop empty events.
#[derive(Debug, Args)]
pub struct ShiftArgs {
    /// Input subtitle file or folder.
    pub input: PathBuf,
    /// Output subtitle file or folder.
    pub output: PathBuf,

    /// Shift in ms (+delays, -advances).
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    pub shift_ms: i32,

    /// Keep empty events instead of deleting them.
    #[arg(long)]
    pub no_clean: bool,

    /// Keep tag-only events.
    #[arg(long)]
    pub keep_tag_only: bool,

    /// Recurse into subfolders (folder mode).
    #[arg(short, long)]
    pub recursive: bool,

    /// File extensions to process.
    #[arg(long, default_value = ".srt,.ass,.ssa,.vtt")]
    pub ext: String,
}

struct ShiftSummary {
    before: usize,
    after: usize,
    removed: usize,
}

fn shift_one(input: &Path, output: &Path, args: &ShiftArgs) -> Result<ShiftSummary> {
    // Fully loaded before writing, so in-place runs (input == output) are safe.
    let mut file = batch::load_any(input)?;
    let before = file.events.len();
    if args.shift_ms != 0 {
        shift::shift_events(&mut file.events, args.shift_ms);
    }
    let mut removed = 0;
    if !args.no_clean {
        let (kept, r) = shift::drop_empty(std::mem::take(&mut file.events), !args.keep_tag_only);
        file.events = kept;
        removed = r;
    }
    batch::save_any(&file, output)?;
    Ok(ShiftSummary {
        before,
        after: file.events.len(),
        removed,
    })
}

fn print_one(input: &Path, output: &Path, s: &ShiftSummary, args: &ShiftArgs) {
    println!("Input file   : {}", input.display());
    println!("Output file  : {}", output.display());
    println!("Total events : {} -> {}", s.before, s.after);
    if args.shift_ms != 0 {
        let dir = if args.shift_ms > 0 {
            "delayed"
        } else {
            "advanced"
        };
        println!("Shift        : {} ms ({dir})", args.shift_ms.abs());
    }
    if !args.no_clean {
        println!("Events dropped: {}", s.removed);
    }
}

pub async fn run(args: ShiftArgs) -> Result<()> {
    if args.input.is_file() {
        let s = shift_one(&args.input, &args.output, &args)
            .with_context(|| format!("failed on {}", args.input.display()))?;
        print_one(&args.input, &args.output, &s, &args);
        return Ok(());
    }
    if !args.input.is_dir() {
        anyhow::bail!("Input not found: {}", args.input.display());
    }
    let exts = batch::parse_ext_list(&args.ext);
    let files = batch::collect_subs(&args.input, &exts, args.recursive)?;
    if files.is_empty() {
        anyhow::bail!("No subtitle files found in {}", args.input.display());
    }
    let mut ok = 0;
    let mut failed = 0;
    let mut removed_total = 0;
    for f in &files {
        let rel = f.strip_prefix(&args.input).unwrap_or(f);
        let dest = args.output.join(rel);
        match shift_one(f, &dest, &args) {
            Ok(s) => {
                ok += 1;
                removed_total += s.removed;
                println!(
                    "[OK] {} (events: {} -> {}, dropped: {})",
                    rel.display(),
                    s.before,
                    s.after,
                    s.removed
                );
            }
            Err(e) => {
                failed += 1;
                println!("[FAILED] {} -> {e:#}", rel.display());
            }
        }
    }
    println!("--------------------------------------------------");
    println!("Total files   : {}", files.len());
    println!("Succeeded     : {ok}");
    println!("Failed        : {failed}");
    println!("Events dropped: {removed_total}");
    if ok == 0 {
        anyhow::bail!("All files failed.");
    }
    Ok(())
}
