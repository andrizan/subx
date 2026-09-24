use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::subs::{batch, filter};

/// `subx filter <FOLDER> -k opening -k ending` — delete keyword lines.
#[derive(Debug, Args)]
pub struct FilterArgs {
    /// Folder containing subtitles (or a single file).
    pub folder: PathBuf,

    /// Keywords as regex (repeatable, space-separated ok, case-insensitive).
    #[arg(short = 'k', long = "keyword", required = true, num_args = 1..)]
    pub keywords: Vec<String>,

    /// Extensions (comma-separated, without dots).
    #[arg(long, default_value = "ass,srt")]
    pub ext: String,

    /// Dry run: only show match counts, don't write.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: FilterArgs) -> Result<()> {
    let patterns = filter::compile_keywords(&args.keywords)?;
    let files: Vec<PathBuf> = if args.folder.is_file() {
        vec![args.folder.clone()]
    } else if args.folder.is_dir() {
        let exts = batch::parse_ext_list(&args.ext);
        batch::collect_subs(&args.folder, &exts, false)?
    } else {
        anyhow::bail!("Not found: {}", args.folder.display());
    };
    if files.is_empty() {
        anyhow::bail!("No subtitle files found in {}", args.folder.display());
    }
    let mut ok = 0;
    let mut failed = 0;
    let mut removed_total = 0;
    for f in &files {
        // Fully loaded before the atomic in-place rewrite.
        match batch::load_any(f) {
            Ok(mut file) => {
                let before = file.events.len();
                let (kept, removed) =
                    filter::filter_events(std::mem::take(&mut file.events), &patterns);
                file.events = kept;
                if !args.dry_run
                    && let Err(e) = batch::save_any(&file, f)
                {
                    failed += 1;
                    println!("[FAILED] {} -> {e:#}", f.display());
                    continue;
                }
                ok += 1;
                removed_total += removed;
                let mode = if args.dry_run { " (dry run)" } else { "" };
                println!(
                    "[OK] {} (events: {} -> {}, removed: {}){mode}",
                    f.display(),
                    before,
                    before - removed,
                    removed
                );
            }
            Err(e) => {
                failed += 1;
                println!("[FAILED] {} -> {e:#}", f.display());
            }
        }
    }
    println!("--------------------------------------------------");
    println!("Total files   : {}", files.len());
    println!("Succeeded     : {ok}");
    println!("Failed        : {failed}");
    println!("Lines removed : {removed_total}");
    if ok == 0 {
        anyhow::bail!("All files failed.");
    }
    Ok(())
}
