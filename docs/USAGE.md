---
layout: default
title: subx manual
---

# subx — User Manual

> Single binary for the full fansub pipeline: extract, clean, retime,
> translate, QC, and mux.
> Requires `ffmpeg` + `ffprobe` on `PATH` (or next to `subx.exe`).

- [Install](#install)
- [Typical workflows](#typical-workflows)
- [Command reference](#command-reference)
- [Configuration](#configuration)
- [Translate engines in depth](#translate-engines-in-depth)
- [Troubleshooting](#troubleshooting)

Global flags (every command): `--config <FILE>`, `-v` (repeatable for debug),
`-q` (errors only), `-h/--help`, `-V/--version`. Exit code is `0` on success,
non-zero with an error message otherwise.

## Install

```powershell
cargo install --path .      # via cargo (run at the repo root)
cargo build --release       # or binary at target/release/subx.exe
```

Or from the repo root: `make build-release`, `make check`, `make install`.
`ffmpeg` via `winget install ffmpeg` / `scoop install ffmpeg`;
`yt-dlp` (for `fetch`) via `winget install yt-dlp`.

## Typical workflows

### One episode, subtitle release

```powershell
subx probe "Ep01.mkv"                                  # inspect first (metadata below)
subx extract "." -j 8 --audio                          # subs/ + audio/
subx clean subs out -r --shift-ms -1500                # -r walks subs/<lang>/
subx filter out -k opening -k ending                   # drop recap/credit lines
subx translate out out-id --engine ai --from en --to id --dry-run   # cost estimate
subx translate out out-id --engine ai --from en --to id --glossary ore=aku
subx stats out-id --cps 20                             # QC: speed, overlaps
subx mux --video "Ep01.mkv" --subs "out-id\Ep01.id.ass" -o "Ep01_SUBX.mkv"
```

### Whole season at once

```powershell
subx extract "Season1" -j 8
subx clean "Season1\subs" "Season1\out" -r
subx translate "Season1\out" "Season1\id" --engine ai --from en --to id
subx mux --batch-dir "Season1" --out-dir "Season1_muxed" --title "MyFansub"
```

## Command reference

### `probe` — full media metadata

```
subx probe <FILE> [--json]
```

Prints container format (duration, size, bitrate, tags), per-stream details
(video: resolution/fps/pixel format; audio: sample rate/channels; subtitles:
language + title), disposition flags (`[default]`/`[forced]`), chapters, and
file tags. `--json` dumps raw ffprobe JSON for scripting.

### `extract` — subtitles (+ audio) out of video

```
subx extract <FOLDER> [--out subs] [-j 8] [--ext mkv,mp4,mka] [--overwrite]
                        [--audio] [--audio-out audio]
```

- Probes every video with ffprobe; Indonesian (`id`) goes to the output root,
  other languages to `<out>/<lang>/` as `<name>.S<index>[.<title>].<lang>.<ext>`.
- `--audio` also copies audio tracks (no re-encode) to `--audio-out` as
  `<name>.A<index>.<lang>.<ext>` (`.m4a`, `.mp3`, `.flac`, …, fallback `.mka`).
- Filenames are Windows-sanitized and length-capped; existing files are skipped
  unless `--overwrite`. Image subtitles (PGS/DVD) are copied as `.sup` — they
  cannot be cleaned or translated (no text layer).

### `clean` — scrub ASS, keep karaoke

```
subx clean <IN> <OUT> [--keep-karaoke] [--drop-junk] [--font Arial]
             [--shift-ms 0] [--in-place] [-r]
```

- Keeps vertical karaoke verbatim (style `Karaoke`), strips visual tags,
  drawings, junk credits and empty lines from normal dialogue (style `Default`),
  and rebuilds the header in one font. `-r` recurses (needed after `extract`).
- `--shift-ms` retimes in the same pass; `--in-place` rewrites the input.

### `shift` — retime + drop empties

```
subx shift <IN> <OUT> --shift-ms -1500 [--no-clean] [--keep-tag-only]
             [-r] [--ext .srt,.ass,.ssa,.vtt]
```

Positive delays, negative advances (clamped at zero). By default, events
without visible text are deleted (tag-only events too, unless `--keep-tag-only`
or `--no-clean`). Folder runs mirror the input structure and are in-place safe.

### `filter` — delete keyword lines

```
subx filter <FOLDER> -k <regex>... [--ext ass,srt] [--dry-run]
```

Case-insensitive regexes (grep `-E` flavor) matched against visible text;
`--dry-run` only reports counts. Files are rewritten atomically.

### `mux` — merge it all back

```
subx mux --video V [--audio A...] [--subs S...] -o OUT.mkv [--title T]
subx mux --video V --hardsub S.ass -o OUT.mkv
subx mux --batch-dir DIR [--out-dir D] [--title T]
```

- Default softmux: stream copy (`-c copy`), source audio kept, subtitle language
  auto-tagged from filenames (`Ep01.id.ass` → `ind`).
- `--hardsub` burns one subtitle in (libx264 CRF 18) while other tracks still mux.
- `--batch-dir` muxes every video with its same-stem audio/subs to `<stem>.muxed.mkv`.

### `convert` — srt ↔ ass ↔ vtt

```
subx convert <IN> <OUT> [--fps N]
```

Target format comes from the output extension. Line breaks and tags are
remapped (`\n` ↔ `\N`, `{...}`/`<v>` stripped); timing is bit-identical.
`--fps` is reserved for frame-based formats (informational for now).

### `translate` — LibreTranslate or AI

```
subx translate <IN> <OUT> --from en --to id [--engine libre|ai]
                 [--url ...] [--model ...] [--base-url ...] [--api-key ...]
                 [--batch N] [--context 2] [--glossary src=dst...]
                 [--translate-karaoke] [--dry-run] [--rate-limit-ms 0] [--no-cache]
```

See [Translate engines in depth](#translate-engines-in-depth). File or folder
input; karaoke/effect lines are skipped unless `--translate-karaoke`;
`--dry-run` prints a size estimate and exits.

### `fetch` — subtitles via yt-dlp

```
subx fetch <URL> [--lang id] [--out Subs] [--format srt/ass]
```

Skips media download, writes `.srt`/`.ass` subs (incl. auto-subs) for videos
and playlists.

### `stats` — subtitle QC

```
subx stats <IN> [--cps 20] [--json]
```

Reports per file: event count, min/max durations, average/max reading speed
(CPS on visible text), lines over the limit, and overlapping cues.

## Configuration

Optional `subx.toml` (next to the binary, or `--config`); CLI flags always win.
`subx.local.toml` (gitignored) overrides it — put secrets there.

| Key | Default | Meaning |
|---|---|---|
| `workers` | `8` | parallel workers for `extract` |
| `lang_priority` | `["id"]` | (reserved) preferred language |
| `default_font` | `"Arial"` | font used by `clean` |
| `libre_url` | `"http://localhost:5000"` | LibreTranslate server |
| `libre_api_key` | — | key for public LibreTranslate instances |
| `translate_engine` | `"libre"` | default `--engine` |
| `ai_base_url` | `"https://api.openai.com/v1"` | OpenAI-compatible endpoint |
| `ai_model` | `"gpt-4o-mini"` | default AI model |
| `ai_api_key` | — | AI key (prefer env, see below) |

## Translate engines in depth

Both engines share batching, `⟦n⟧` tag protection, resume cache
(`.subx-translate-cache.json`, hash of engine+model+texts — glossary edits
apply without re-calling), and glossary handling.

### `libre` (default, free)

Needs a server: `docker run -d -p 5000:5000 libretranslate/libretranslate`
(or `--url` + `--api-key` for a public instance). Best for drafts and bulk;
style quality is limited.

### `ai` (release quality, needs key)

One OpenAI-compatible implementation (`POST {base-url}/chat/completions`)
covers every provider — only `--base-url`/`--model` change:

| Provider | `--base-url` | Example `--model` | Key env |
|---|---|---|---|
| OpenAI | `https://api.openai.com/v1` | `gpt-4o-mini` | `OPENAI_API_KEY` |
| OpenRouter | `https://openrouter.ai/api/v1` | `deepseek/deepseek-chat` | `OPENROUTER_API_KEY` |
| DeepSeek | `https://api.deepseek.com/v1` | `deepseek-chat` | `DEEPSEEK_API_KEY` |
| Google Gemini | `https://generativelanguage.googleapis.com/v1beta/openai/` | `gemini-2.0-flash` | `GEMINI_API_KEY` |
| Ollama (local, free) | `http://localhost:11434/v1` | `qwen2.5` | none |
| LM Studio (local) | `http://localhost:1234/v1` | loaded model | none |

Key resolution: `--api-key` → provider env → `SUBX_API_KEY` → `ai_api_key`
(toml). Keys are never logged (masked). `--batch` (default 10 for AI) with
`--context` neighboring lines keeps references consistent; `--glossary`
`ore=aku` is injected into the prompt *and* post-applied; `--dry-run` shows
token estimates first.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `ffmpeg not found` | install it (`winget`/`scoop`) or drop `ffmpeg.exe`+`ffprobe.exe` next to `subx.exe` |
| `No subtitles found` | file truly has none, or only image subs (copied as `.sup`, not translatable) |
| `LibreTranslate HTTP …` / connection refused | server not running — start docker above or fix `--url` |
| `AI provider rejected the key (401)` | wrong/expired key (masked in the message, never printed) |
| `AI returned N lines, expected M` | model broke numbering — retry, or lower `--batch` |
| `yt-dlp not found` | `winget install yt-dlp` |
| `clean` skipped `subs/en/` | add `-r` to recurse into language subfolders |
| mojibake input | files are read as UTF-8; convert legacy encodings first (planned: auto-detect) |
