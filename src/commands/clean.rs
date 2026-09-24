use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::subs::{self, batch, clean};

/// `subx clean <IN> <OUT>` — clean ASS files (keep karaoke, drop junk).
#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Input .ass file or folder.
    pub input: PathBuf,
    /// Output .ass file or folder.
    pub output: PathBuf,

    /// Keep vertical karaoke.
    #[arg(long, default_value_t = true)]
    pub keep_karaoke: bool,

    /// Drop junk (visual tags, drawings, sync-titles).
    #[arg(long, default_value_t = true)]
    pub drop_junk: bool,

    /// Normalized font.
    #[arg(long, default_value = "Arial")]
    pub font: String,

    /// Also shift timestamps (ms, +delays / -advances).
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    pub shift_ms: i32,

    /// Edit in place.
    #[arg(long)]
    pub in_place: bool,

    /// Recurse into subfolders (needed for `extract` output with `subs/<lang>/`).
    #[arg(short, long)]
    pub recursive: bool,
}

struct CleanReport {
    dialogs: usize,
    karaoke: usize,
    in_bytes: u64,
    out_bytes: u64,
}

fn clean_one(input: &Path, output: &Path, args: &CleanArgs) -> Result<CleanReport> {
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext != "ass" && ext != "ssa" {
        anyhow::bail!("clean only supports .ass/.ssa, got: {}", input.display());
    }
    let in_bytes = std::fs::metadata(input)
        .with_context(|| format!("cannot stat {}", input.display()))?
        .len();
    // Fully loaded before writing, so in-place runs are safe.
    let file = batch::load_any(input)?;
    let (events, stats) = if args.keep_karaoke || args.drop_junk {
        clean::clean_events(&file.events, &args.font, args.shift_ms)
    } else {
        let mut events = file.events.clone();
        if args.shift_ms != 0 {
            subs::shift::shift_events(&mut events, args.shift_ms);
        }
        (
            events,
            clean::CleanStats {
                dialogs: file.events.len(),
                karaoke: 0,
            },
        )
    };
    let out = subs::SubFile {
        format: subs::SubFormat::Ass,
        header: clean::default_header(&args.font),
        ass_columns: subs::standard_columns(),
        events,
    };
    batch::save_any(&out, output)?;
    let out_bytes = std::fs::metadata(output)
        .with_context(|| format!("cannot stat {}", output.display()))?
        .len();
    Ok(CleanReport {
        dialogs: stats.dialogs,
        karaoke: stats.karaoke,
        in_bytes,
        out_bytes,
    })
}

fn reduction(in_bytes: u64, out_bytes: u64) -> f64 {
    if in_bytes == 0 {
        0.0
    } else {
        (in_bytes.saturating_sub(out_bytes)) as f64 / in_bytes as f64 * 100.0
    }
}

fn print_report(name: &str, r: &CleanReport) {
    println!(
        "[OK] {name} (dialogs: {}, karaoke: {}, size: {:.1}% smaller)",
        r.dialogs,
        r.karaoke,
        reduction(r.in_bytes, r.out_bytes)
    );
}

pub async fn run(args: CleanArgs) -> Result<()> {
    if args.input.is_file() {
        let dest = if args.in_place {
            args.input.clone()
        } else {
            args.output.clone()
        };
        let r = clean_one(&args.input, &dest, &args)
            .with_context(|| format!("failed on {}", args.input.display()))?;
        print_report(&args.input.to_string_lossy(), &r);
        return Ok(());
    }
    if !args.input.is_dir() {
        anyhow::bail!("Input not found: {}", args.input.display());
    }
    let files = batch::collect_subs(
        &args.input,
        &["ass".to_string(), "ssa".to_string()],
        args.recursive,
    )?;
    if files.is_empty() {
        anyhow::bail!("No .ass files found in {}", args.input.display());
    }
    let mut ok = 0;
    let mut failed = 0;
    let mut dialogs_total = 0;
    let mut karaoke_total = 0;
    for f in &files {
        // Mirror the input structure so `subs/<lang>/` layouts survive.
        let dest = if args.in_place {
            f.clone()
        } else {
            args.output.join(f.strip_prefix(&args.input).unwrap_or(f))
        };
        match clean_one(f, &dest, &args) {
            Ok(r) => {
                ok += 1;
                dialogs_total += r.dialogs;
                karaoke_total += r.karaoke;
                print_report(&f.to_string_lossy(), &r);
            }
            Err(e) => {
                failed += 1;
                println!("[FAILED] {} -> {e:#}", f.display());
            }
        }
    }
    println!("--------------------------------------------------");
    println!("Total files      : {}", files.len());
    println!("Succeeded        : {ok}");
    println!("Failed           : {failed}");
    println!("Dialogs kept     : {dialogs_total}");
    println!("Karaoke preserved: {karaoke_total}");
    if ok == 0 {
        anyhow::bail!("All files failed.");
    }
    Ok(())
}
