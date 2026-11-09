use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::subs::{batch, shift};

/// `subx shift <IN> [OUT] --shift-ms -1500` — shift timing + drop empty events.
#[derive(Debug, Args)]
pub struct ShiftArgs {
    /// Input subtitle file or folder.
    pub input: PathBuf,
    /// Output subtitle file or folder. Default: a `<name>.shifted.<ext>` copy
    /// next to each input; ignored with --in-place.
    pub output: Option<PathBuf>,

    /// Overwrite the input instead of writing a `.shifted` copy.
    #[arg(long)]
    pub in_place: bool,

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

/// Where the shifted copy of `input` goes. `rel` is its path inside a folder
/// INPUT (None for a single file).
fn destination(args: &ShiftArgs, input: &Path, rel: Option<&Path>) -> PathBuf {
    if args.in_place {
        return input.to_path_buf();
    }
    match (&args.output, rel) {
        (Some(out), Some(rel)) => out.join(rel),
        (Some(out), None) => out.clone(),
        (None, _) => batch::tagged_path(input, "shifted"),
    }
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
        let dest = destination(&args, &args.input, None);
        let s = shift_one(&args.input, &dest, &args)
            .with_context(|| format!("failed on {}", args.input.display()))?;
        print_one(&args.input, &dest, &s, &args);
        return Ok(());
    }
    if !args.input.is_dir() {
        anyhow::bail!("Input not found: {}", args.input.display());
    }
    let exts = batch::parse_ext_list(&args.ext);
    // `.shifted` copies from an earlier run are not input.
    let files: Vec<PathBuf> = batch::collect_subs(&args.input, &exts, args.recursive)?
        .into_iter()
        .filter(|f| !batch::is_tagged(f, "shifted"))
        .collect();
    if files.is_empty() {
        let hint = if args.recursive {
            ""
        } else {
            " (use -r to look in subfolders)"
        };
        anyhow::bail!("No subtitle files found in {}{hint}", args.input.display());
    }
    let mut ok = 0;
    let mut failed = 0;
    let mut removed_total = 0;
    for f in &files {
        let rel = f.strip_prefix(&args.input).unwrap_or(f);
        let dest = destination(&args, f, Some(rel));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(input: &Path) -> ShiftArgs {
        ShiftArgs {
            input: input.to_path_buf(),
            output: None,
            in_place: false,
            shift_ms: 500,
            no_clean: false,
            keep_tag_only: false,
            recursive: false,
            ext: ".srt,.ass,.ssa,.vtt".to_string(),
        }
    }

    const SRT: &str = "1
00:00:01,000 --> 00:00:02,000
Hello

";

    fn write_srt(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, SRT).unwrap();
        path
    }

    #[tokio::test]
    async fn a_folder_is_shifted_into_copies_beside_the_originals() {
        let dir = tempfile::tempdir().unwrap();
        let first = write_srt(dir.path(), "Ep01.srt");
        write_srt(dir.path(), "Ep02.srt");

        run(args(dir.path())).await.unwrap();
        let copy = std::fs::read_to_string(dir.path().join("Ep01.shifted.srt")).unwrap();
        assert!(copy.contains("00:00:01,500 --> 00:00:02,500"), "{copy}");
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            SRT,
            "original untouched"
        );

        // A second run does not shift its own copies again.
        run(args(dir.path())).await.unwrap();
        assert!(!dir.path().join("Ep01.shifted.shifted.srt").exists());
    }

    #[tokio::test]
    async fn in_place_overwrites_and_a_named_output_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_srt(dir.path(), "Ep01.srt");

        let named = dir.path().join("named.srt");
        let mut with_output = args(&file);
        with_output.output = Some(named.clone());
        run(with_output).await.unwrap();
        assert!(
            std::fs::read_to_string(&named)
                .unwrap()
                .contains("00:00:01,500")
        );

        let mut in_place = args(&file);
        in_place.in_place = true;
        run(in_place).await.unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("00:00:01,500 --> 00:00:02,500"), "{text}");
    }
}
