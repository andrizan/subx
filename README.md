# subx

All-in-one fansub toolkit in a single Rust binary: extract subtitles/audio from
video, clean and retime them, translate via LibreTranslate or any
OpenAI-compatible AI model, QC the result, and mux it back.

Status: **complete** — 61 unit + 5 end-to-end tests, `clippy -D warnings` clean.

## Requirements

| Dependency | Needed for | Install |
|---|---|---|
| `ffmpeg` + `ffprobe` | everything media-related | `winget install ffmpeg` / `scoop install ffmpeg` |
| `yt-dlp` (optional) | `fetch` | `winget install yt-dlp` |
| LibreTranslate server **or** AI API key (optional) | `translate` | `docker run -p 5000:5000 libretranslate/libretranslate` or set `OPENROUTER_API_KEY` |

`ffmpeg`/`ffprobe` are picked up next to `subx.exe` first, then from `PATH`.

## Install

```powershell
cargo install --path .      # installs `subx` via cargo
cargo build --release       # or binary at target/release/subx.exe
```

Shortcuts: `make build-release`, `make check`, `make run ARGS="..."`.

## Commands

| Command | What it does |
|---|---|
| `extract` | Subtitles (+ optional `--audio`) out of MKV/MP4 into `subs/` + `audio/` |
| `clean` | Scrub ASS junk, keep karaoke, normalize the font (`-r` for `subs/<lang>/`) |
| `shift` | Shift timestamps ±ms, drop empty events (file or folder, in-place safe) |
| `filter` | Delete lines matching keywords (regex, dry-run supported) |
| `mux` | Merge video+audio+subs (softmux copy, `--hardsub` burn-in, `--batch-dir` seasons) |
| `convert` | Convert `srt` ↔ `ass` ↔ `vtt` (timing preserved) |
| `translate` | Translate via LibreTranslate or AI (tags preserved, resume cache, glossary) |
| `probe` | Full media metadata: format, streams, chapters, tags (`--json` for scripts) |
| `fetch` | Download subtitles via `yt-dlp` |
| `stats` | QC report: reading speed (CPS), durations, overlaps |

```powershell
subx --help
subx extract "D:\Anime\Season1" -j 8 --audio
subx clean subs out -r --shift-ms -1500
subx translate out out-id --engine ai --from en --to id
subx stats out-id --cps 20
subx mux --video Ep01.mkv --subs out\Ep01.id.ass -o Ep01_FANSUB.mkv
```

Full manual: [`docs/USAGE.md`](docs/USAGE.md).

## Configuration

Optional `subx.toml` next to the binary (or `--config`); every CLI flag wins
over it. Secrets belong in **`subx.local.toml`** (gitignored, overrides
`subx.toml`) — see [`subx.local.toml.example`](subx.local.toml.example).

## Development

```powershell
cargo fmt --check && cargo clippy -- -D warnings && cargo test   # gate (or: make check)
cargo test -- --ignored                                          # end-to-end (needs ffmpeg)
```

Conventions: [`AGENTS.md`](AGENTS.md). Releases (Windows + Linux binaries with
checksums + changelog) build automatically from `v*` tags.

## License

Public domain ([Unlicense](LICENSE)) — free for any purpose.
