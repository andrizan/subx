use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use regex::Regex;
use reqwest::Client;

use crate::subs::{Event, batch};
use crate::translate::{LangPair, Translator, cache, prompt, segment};
use crate::translate::{ai, libre};

/// `subx translate <IN> <OUT> --from en --to id`
#[derive(Debug, Args)]
pub struct TranslateArgs {
    /// Input subtitle file or folder.
    pub input: PathBuf,
    /// Output subtitle file or folder.
    pub output: PathBuf,

    /// Source language.
    #[arg(long, default_value = "en")]
    pub from: String,

    /// Target language.
    #[arg(long, default_value = "id")]
    pub to: String,

    /// LibreTranslate URL (`--engine libre` only).
    #[arg(long, default_value = "http://localhost:5000")]
    pub url: String,

    /// Translation engine: `libre` (default, free) or `ai` (needs an API key).
    #[arg(long, default_value = "libre")]
    pub engine: String,

    /// AI model (`--engine ai` only), e.g. gpt-4o-mini, deepseek-chat, google/gemini-2.0-flash.
    #[arg(long)]
    pub model: Option<String>,

    /// OpenAI-compatible base URL (`--engine ai` only). Example for OpenRouter:
    /// `https://openrouter.ai/api/v1`, local Ollama: `http://localhost:11434/v1`.
    #[arg(long)]
    pub base_url: Option<String>,

    /// API key: LibreTranslate public instance OR AI provider.
    /// Can come from env OPENAI_API_KEY / OPENROUTER_API_KEY / DEEPSEEK_API_KEY /
    /// GEMINI_API_KEY / SUBX_API_KEY (never printed to logs).
    #[arg(long)]
    pub api_key: Option<String>,

    /// Batch size, lines per request (default: 20 for libre, 10 for ai).
    #[arg(long)]
    pub batch: Option<usize>,

    /// Context lines before/after each batch (`--engine ai` only).
    #[arg(long, default_value_t = 2)]
    pub context: usize,

    /// Glossary entries `src=dst` (repeatable, space-separated ok).
    #[arg(long, num_args = 1..)]
    pub glossary: Vec<String>,

    /// Also translate karaoke/effect lines (skipped by default).
    #[arg(long)]
    pub translate_karaoke: bool,

    /// Estimate input size and exit without calling any service.
    #[arg(long)]
    pub dry_run: bool,

    /// Milliseconds to wait between batches (rate-limit).
    #[arg(long, default_value_t = 0)]
    pub rate_limit_ms: u64,

    /// Disable the resume cache.
    #[arg(long)]
    pub no_cache: bool,
}

struct PipeOpts {
    pair: LangPair,
    glossary_pairs: Vec<(String, String)>,
    glossary_re: Vec<(Regex, String)>,
    translate_karaoke: bool,
}

#[derive(Debug, Default)]
struct TranslateStats {
    total: usize,
    fresh: usize,
    cached: usize,
    skipped: usize,
}

/// Translate one loaded event list: cache lookup, engine batch, placeholder
/// restore, glossary post-replace. The cache stores raw translations so
/// glossary edits apply on re-runs without new service calls.
async fn translate_events<T: Translator>(
    events: &mut [Event],
    translator: &T,
    cache: &mut cache::Cache,
    engine: &str,
    model: &str,
    opts: &PipeOpts,
) -> Result<TranslateStats> {
    let mut stats = TranslateStats {
        total: events.len(),
        ..Default::default()
    };
    let mut pending: Vec<(usize, String, Vec<String>, String)> = Vec::new();
    // Collect owned work items first so no borrow is held while assigning results.
    struct Item {
        index: usize,
        protected: String,
        table: Vec<String>,
        key: String,
        skip: bool,
    }
    let items: Vec<Item> = events
        .iter()
        .enumerate()
        .map(|(index, e)| {
            let skip = !opts.translate_karaoke
                && (e.style.eq_ignore_ascii_case("karaoke") || !e.effect.trim().is_empty());
            let (protected, table) = segment::protect(&e.text);
            let key = cache::Cache::key(engine, model, &opts.pair.from, &opts.pair.to, &protected);
            Item {
                index,
                protected,
                table,
                key,
                skip,
            }
        })
        .collect();
    for it in items {
        if it.skip {
            stats.skipped += 1;
            continue;
        }
        match cache.get(&it.key).cloned() {
            Some(hit) => {
                events[it.index].text =
                    prompt::apply_glossary(&segment::restore(&hit, &it.table), &opts.glossary_re);
                stats.cached += 1;
            }
            None => pending.push((it.index, it.protected, it.table, it.key)),
        }
    }
    if !pending.is_empty() {
        let srcs: Vec<String> = pending.iter().map(|(_, p, _, _)| p.clone()).collect();
        let outs = translator.translate_batch(&srcs, &opts.pair).await?;
        for ((i, _, table, key), raw) in pending.into_iter().zip(outs) {
            let restored = segment::restore(&raw, &table);
            cache.put(key, restored.clone());
            events[i].text = prompt::apply_glossary(&restored, &opts.glossary_re);
            stats.fresh += 1;
        }
    }
    Ok(stats)
}

async fn translate_one_file<T: Translator>(
    input: &std::path::Path,
    output: &std::path::Path,
    translator: &T,
    engine: &str,
    model: &str,
    cache: &mut cache::Cache,
    opts: &PipeOpts,
) -> Result<TranslateStats> {
    // Fully loaded before writing, so in-place runs are safe.
    let mut file =
        batch::load_any(input).with_context(|| format!("failed on {}", input.display()))?;
    let stats = translate_events(&mut file.events, translator, cache, engine, model, opts).await?;
    batch::save_any(&file, output)?;
    Ok(stats)
}

pub async fn run(args: TranslateArgs) -> Result<()> {
    let glossary_pairs = prompt::parse_glossary(&args.glossary)?;
    let glossary_re = prompt::compile_glossary(&glossary_pairs)?;
    let pair = LangPair {
        from: args.from.clone(),
        to: args.to.clone(),
    };
    let opts = PipeOpts {
        pair: pair.clone(),
        glossary_pairs: glossary_pairs.clone(),
        glossary_re,
        translate_karaoke: args.translate_karaoke,
    };

    let single = args.input.is_file();
    let files: Vec<(PathBuf, PathBuf)> = if single {
        vec![(args.input.clone(), args.output.clone())]
    } else if args.input.is_dir() {
        let exts: Vec<String> = crate::subs::supported_exts()
            .iter()
            .map(|s| s.to_string())
            .collect();
        batch::collect_subs(&args.input, &exts, false)?
            .into_iter()
            .map(|f| {
                let rel = f.strip_prefix(&args.input).unwrap_or(&f).to_path_buf();
                (f, args.output.join(rel))
            })
            .collect()
    } else {
        anyhow::bail!("Input not found: {}", args.input.display());
    };
    if files.is_empty() {
        anyhow::bail!("No subtitle files found in {}", args.input.display());
    }

    let batch_size = args
        .batch
        .unwrap_or(if args.engine == "ai" { 10 } else { 20 });

    if args.dry_run {
        let mut events = 0;
        let mut chars = 0;
        for (inp, _) in &files {
            let file = batch::load_any(inp)?;
            events += file.events.len();
            chars += file
                .events
                .iter()
                .map(|e| e.text.chars().count())
                .sum::<usize>();
        }
        let batches = events.div_ceil(batch_size).max(1);
        println!(
            "Dry run: {events} event(s), {chars} char(s) (~{} tokens), ~{batches} request(s) via engine '{}'.",
            chars / 4,
            args.engine
        );
        return Ok(());
    }

    let cache_path = if single {
        args.output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".subx-translate-cache.json")
    } else {
        args.output.join(".subx-translate-cache.json")
    };
    let mut cache = if args.no_cache {
        cache::Cache::disabled()
    } else {
        cache::Cache::load(cache_path)
    };

    let client = Client::new();
    let mut total = TranslateStats::default();
    match args.engine.as_str() {
        "libre" => {
            let t = libre::LibreTranslator::new(client, args.url.clone(), args.api_key.clone())
                .with_rate_limit(args.rate_limit_ms);
            for (inp, out) in &files {
                match translate_one_file(inp, out, &t, "libre", "libre", &mut cache, &opts).await {
                    Ok(s) => {
                        println!(
                            "[OK] {} (events: {}, fresh: {}, cached: {}, skipped: {})",
                            inp.display(),
                            s.total,
                            s.fresh,
                            s.cached,
                            s.skipped
                        );
                        total.total += s.total;
                        total.fresh += s.fresh;
                        total.cached += s.cached;
                        total.skipped += s.skipped;
                    }
                    Err(e) => println!("[FAILED] {} -> {e:#}", inp.display()),
                }
            }
        }
        "ai" => {
            let base = args
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let model = args
                .model
                .clone()
                .unwrap_or_else(|| "gpt-4o-mini".to_string());
            let key = ai::AiTranslator::resolve_api_key(args.api_key.clone(), &base)?;
            let t = ai::AiTranslator::new(client, base, model.clone(), key)
                .with_batch(batch_size, args.context)
                .with_glossary(glossary_pairs)
                .with_rate_limit(args.rate_limit_ms);
            for (inp, out) in &files {
                match translate_one_file(inp, out, &t, "ai", &model, &mut cache, &opts).await {
                    Ok(s) => {
                        println!(
                            "[OK] {} (events: {}, fresh: {}, cached: {}, skipped: {})",
                            inp.display(),
                            s.total,
                            s.fresh,
                            s.cached,
                            s.skipped
                        );
                        total.total += s.total;
                        total.fresh += s.fresh;
                        total.cached += s.cached;
                        total.skipped += s.skipped;
                    }
                    Err(e) => println!("[FAILED] {} -> {e:#}", inp.display()),
                }
            }
        }
        other => anyhow::bail!("unknown engine '{other}' (expected 'libre' or 'ai')"),
    }
    cache.save()?;
    println!("--------------------------------------------------");
    println!("Files      : {}", files.len());
    println!("Events     : {}", total.total);
    println!("Fresh      : {}", total.fresh);
    println!("Cached     : {}", total.cached);
    println!("Skipped    : {} (karaoke/effects)", total.skipped);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTranslator {
        calls: std::sync::Mutex<usize>,
    }

    impl Translator for FakeTranslator {
        async fn translate_batch(&self, texts: &[String], _pair: &LangPair) -> Result<Vec<String>> {
            *self.calls.lock().unwrap() += texts.len();
            Ok(texts.iter().map(|t| format!("TR:{t}")).collect())
        }
    }

    fn ev(text: &str, style: &str) -> Event {
        Event {
            text: text.into(),
            style: style.into(),
            ..Default::default()
        }
    }

    fn opts() -> PipeOpts {
        let pairs = vec![("Hello".to_string(), "Halo".to_string())];
        PipeOpts {
            pair: LangPair {
                from: "en".into(),
                to: "id".into(),
            },
            glossary_re: prompt::compile_glossary(&pairs).unwrap(),
            glossary_pairs: pairs,
            translate_karaoke: false,
        }
    }

    /// First run translates + caches; second run hits cache (no new calls);
    /// karaoke lines are skipped without calls.
    #[tokio::test]
    async fn cache_avoids_second_round_and_skips_karaoke() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let fake = FakeTranslator::default();
        let o = opts();

        let mut events = vec![
            ev("Hello {\\i1}world", "Default"),
            ev("{\\k40}la", "Karaoke"),
        ];
        let mut cache = cache::Cache::load(cache_path.clone());
        let s = translate_events(&mut events, &fake, &mut cache, "fake", "m", &o)
            .await
            .unwrap();
        assert_eq!((s.fresh, s.cached, s.skipped), (1, 0, 1));
        assert_eq!(*fake.calls.lock().unwrap(), 1);
        assert_eq!(events[0].text, "TR:Halo {\\i1}world");
        assert_eq!(events[1].text, "{\\k40}la");
        cache.save().unwrap();

        let mut cache2 = cache::Cache::load(cache_path);
        let mut events2 = vec![ev("Hello {\\i1}world", "Default")];
        let s2 = translate_events(&mut events2, &fake, &mut cache2, "fake", "m", &o)
            .await
            .unwrap();
        assert_eq!((s2.fresh, s2.cached, s2.skipped), (0, 1, 0));
        assert_eq!(*fake.calls.lock().unwrap(), 1);
        assert_eq!(events2[0].text, "TR:Halo {\\i1}world");
    }
}
