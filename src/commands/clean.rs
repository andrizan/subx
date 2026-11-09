use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::subs::{self, batch, clean, sync};

/// `subx clean <IN> [OUT]` — clean ASS files (keep karaoke, drop junk).
#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Input .ass file or folder.
    pub input: PathBuf,
    /// Output .ass file or folder. Default: a `<name>.clean.ass` copy next to
    /// each input; ignored with --in-place.
    pub output: Option<PathBuf>,

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

    /// Overwrite the input instead of writing a `.clean` copy.
    #[arg(long)]
    pub in_place: bool,

    /// Keep only the spoken dialogue: drop karaoke, songs, signs and other
    /// typesetting (the same selection `sync` times against).
    #[arg(long)]
    pub dialogue_only: bool,

    /// Recurse into subfolders (needed for `extract` output with `subs/<lang>/`).
    #[arg(short, long)]
    pub recursive: bool,
}

struct CleanReport {
    dialogs: usize,
    karaoke: usize,
    duplicates: usize,
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
    let source: Vec<subs::Event> = if args.dialogue_only {
        let mask = sync::dialogue_mask(&file.events);
        file.events
            .iter()
            .zip(mask)
            // Dialogue lines can still carry karaoke timing tags.
            .filter(|(e, keep)| *keep && !clean::is_karaoke_vertical(&e.text))
            .map(|(e, _)| e.clone())
            .collect()
    } else {
        file.events
    };
    let (events, stats) = if args.keep_karaoke || args.drop_junk {
        clean::clean_events(&source, &args.font, args.shift_ms)
    } else {
        let mut events = source.clone();
        if args.shift_ms != 0 {
            subs::shift::shift_events(&mut events, args.shift_ms);
        }
        (
            events,
            clean::CleanStats {
                dialogs: source.len(),
                ..Default::default()
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
        duplicates: stats.duplicates,
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

fn print_report(name: &str, dest: Option<&Path>, r: &CleanReport) {
    let duplicates = if r.duplicates > 0 {
        format!(", duplicates dropped: {}", r.duplicates)
    } else {
        String::new()
    };
    println!(
        "[OK] {name} (dialogs: {}, karaoke: {}{duplicates}, size: {:.1}% smaller)",
        r.dialogs,
        r.karaoke,
        reduction(r.in_bytes, r.out_bytes)
    );
    if let Some(dest) = dest {
        println!("     -> {}", dest.display());
    }
}

/// Where the cleaned copy of `input` goes. `rel` is its path inside a folder
/// INPUT (None for a single file).
fn destination(args: &CleanArgs, input: &Path, rel: Option<&Path>) -> PathBuf {
    if args.in_place {
        return input.to_path_buf();
    }
    match (&args.output, rel) {
        // Mirror the input structure so `subs/<lang>/` layouts survive.
        (Some(out), Some(rel)) => out.join(rel),
        (Some(out), None) => out.clone(),
        (None, _) => batch::tagged_path(input, "clean"),
    }
}

pub async fn run(args: CleanArgs) -> Result<()> {
    if args.input.is_file() {
        let dest = destination(&args, &args.input, None);
        let r = clean_one(&args.input, &dest, &args)
            .with_context(|| format!("failed on {}", args.input.display()))?;
        let copied = (!args.in_place).then_some(dest.as_path());
        print_report(&args.input.to_string_lossy(), copied, &r);
        return Ok(());
    }
    if !args.input.is_dir() {
        anyhow::bail!("Input not found: {}", args.input.display());
    }
    // `.clean` copies from an earlier run are not input.
    let files: Vec<PathBuf> = batch::collect_subs(
        &args.input,
        &["ass".to_string(), "ssa".to_string()],
        args.recursive,
    )?
    .into_iter()
    .filter(|f| !batch::is_tagged(f, "clean"))
    .collect();
    if files.is_empty() {
        let hint = if args.recursive {
            ""
        } else {
            " (use -r to look in subfolders)"
        };
        anyhow::bail!("No .ass files found in {}{hint}", args.input.display());
    }
    let mut ok = 0;
    let mut failed = 0;
    let mut dialogs_total = 0;
    let mut karaoke_total = 0;
    let mut duplicates_total = 0;
    for f in &files {
        let dest = destination(&args, f, Some(f.strip_prefix(&args.input).unwrap_or(f)));
        match clean_one(f, &dest, &args) {
            Ok(r) => {
                ok += 1;
                dialogs_total += r.dialogs;
                karaoke_total += r.karaoke;
                duplicates_total += r.duplicates;
                let copied = (!args.in_place).then_some(dest.as_path());
                print_report(&f.to_string_lossy(), copied, &r);
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
    println!("Duplicates dropped: {duplicates_total}");
    if ok == 0 {
        anyhow::bail!("All files failed.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(input: &Path) -> CleanArgs {
        CleanArgs {
            input: input.to_path_buf(),
            output: None,
            keep_karaoke: true,
            drop_junk: true,
            font: "Arial".to_string(),
            shift_ms: 0,
            in_place: false,
            dialogue_only: false,
            recursive: false,
        }
    }

    /// A messy ASS from the repo fixtures, copied to `dir/name`.
    fn fixture(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let dest = dir.join(name);
        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/dirty.ass");
        std::fs::copy(src, &dest).unwrap();
        dest
    }

    #[tokio::test]
    async fn a_folder_is_cleaned_into_copies_beside_the_originals() {
        let dir = tempfile::tempdir().unwrap();
        let first = fixture(dir.path(), "Ep01.ass");
        fixture(dir.path(), "Ep02.ass");
        let before = std::fs::read(&first).unwrap();

        run(args(dir.path())).await.unwrap();
        assert!(dir.path().join("Ep01.clean.ass").is_file());
        assert!(dir.path().join("Ep02.clean.ass").is_file());
        assert_eq!(std::fs::read(&first).unwrap(), before, "original untouched");

        // A second run does not read its own copies back.
        run(args(dir.path())).await.unwrap();
        assert!(!dir.path().join("Ep01.clean.clean.ass").exists());
    }

    #[tokio::test]
    async fn a_single_file_defaults_to_a_copy_and_in_place_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path(), "Ep01.ass");
        let before = std::fs::read(&file).unwrap();

        run(args(&file)).await.unwrap();
        assert!(dir.path().join("Ep01.clean.ass").is_file());
        assert_eq!(std::fs::read(&file).unwrap(), before);

        let mut in_place = args(&file);
        in_place.in_place = true;
        run(in_place).await.unwrap();
        assert_ne!(std::fs::read(&file).unwrap(), before, "cleaned in place");
    }

    /// A script shaped like a typeset release: dialogue, an opening lyric, karaoke
    /// syllables carrying an Effect, a karaoke-timed line and a sign.
    fn typeset_script() -> String {
        let mut text = String::from(
            "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, \
             BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, \
             BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
             Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,2,2,10,10,10,1\n\n\
             [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
        );
        for i in 0..10 {
            text += &format!(
                "Dialogue: 0,0:00:{:02}.00,0:00:{:02}.00,Default,,0,0,0,,Spoken line {i}\n",
                i * 3,
                i * 3 + 2
            );
        }
        text += "Dialogue: 0,0:01:00.00,0:01:02.00,OP,,0,0,0,,Opening lyric translated\n";
        text += "Dialogue: 0,0:01:00.00,0:01:00.80,OP,,0,0,0,fx,ta\n";
        text += "Dialogue: 0,0:00:20.00,0:00:22.00,Default,,0,0,0,,{\\k20}ka{\\k20}ra{\\k20}oke\n";
        text += "Dialogue: 0,0:00:25.00,0:00:27.00,Sign,,0,0,0,,{\\pos(10,10)}Shop sign\n";
        text
    }

    #[tokio::test]
    async fn dialogue_only_keeps_the_spoken_lines_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Ep01.ass");
        std::fs::write(&file, typeset_script()).unwrap();

        // Without the flag everything survives cleaning, songs and signs included.
        run(args(&file)).await.unwrap();
        let all = std::fs::read_to_string(dir.path().join("Ep01.clean.ass")).unwrap();
        assert!(all.contains("Opening lyric") && all.contains("Shop sign"));

        let mut only = args(&file);
        only.dialogue_only = true;
        only.in_place = true;
        run(only).await.unwrap();
        let kept = std::fs::read_to_string(&file).unwrap();
        for i in 0..10 {
            assert!(kept.contains(&format!("Spoken line {i}")), "line {i} lost");
        }
        for gone in ["Opening lyric", "Shop sign", ",ta", "k20"] {
            assert!(
                !kept.contains(gone),
                "{gone:?} should have been dropped:\n{kept}"
            );
        }
        assert_eq!(kept.matches("\nDialogue:").count(), 10);
    }

    #[tokio::test]
    async fn subfolders_need_r_and_an_output_folder_mirrors_them() {
        let dir = tempfile::tempdir().unwrap();
        fixture(&dir.path().join("en"), "Ep01.en.ass");

        let err = run(args(dir.path())).await.unwrap_err().to_string();
        assert!(err.contains("-r"), "{err}");

        let out = dir.path().join("out");
        let mut recursive = args(dir.path());
        recursive.recursive = true;
        recursive.output = Some(out.clone());
        run(recursive).await.unwrap();
        assert!(out.join("en").join("Ep01.en.ass").is_file());
    }
}
