# subx

All-in-one fansub toolkit in a single Rust binary: extract subtitles/audio from
video, clean and retime them, translate via LibreTranslate or any
OpenAI-compatible AI model, QC the result, and mux it back.

Status: **complete** — 141 unit + 13 end-to-end tests, `clippy --all-targets -D warnings` clean.

## Requirements

| Dependency | Needed for | Install |
|---|---|---|
| `ffmpeg` + `ffprobe` | everything media-related | `winget install ffmpeg` / `scoop install ffmpeg` |
| MKVToolNix (`mkvmerge`, `mkvextract`) | `sync` on a video, `extract --fonts/--chapters`, the native `encode` | Windows x64: `subx tools install mkvmerge`; otherwise any install on `PATH` |
| `yt-dlp` (optional) | `fetch` | `winget install yt-dlp` |
| LibreTranslate server **or** AI API key (optional) | `translate` | `docker run -p 5000:5000 libretranslate/libretranslate` or set `OPENROUTER_API_KEY` |
| StaxRip (Windows, optional; ships its own MKVToolNix) | `encode` | portable zip, point `--staxrip-dir` at it |

`subx tools check` shows what is found (`tools/` next to `subx`, a StaxRip
bundle via `[tools] staxrip_dir`, then `PATH`); `subx tools install all` fetches
the installable ones from their official channels with checksum verification.

## Install

```powershell
cargo install --path .      # installs `subx` via cargo
cargo build --release       # or binary at target/release/subx.exe
```

Shortcuts: `make build-release`, `make check`, `make run ARGS="..."`.

## Commands

| Command | What it does |
|---|---|
| `extract` | Subtitles (+ optional `--audio`, `--fonts`, `--chapters`) out of MKV/MP4, one file or a folder, into `subs/`, `audio/`, `fonts/`, `chapters/` |
| `fonts` | Which fonts the ASS scripts need, and whether each is attached, installed or missing |
| `clean` | Scrub ASS junk, keep karaoke, normalize the font (`-r` for `subs/<lang>/`) |
| `shift` | Shift timestamps ±ms, drop empty events (file or folder, in-place safe) |
| `sync` | Retime automatically: the subtitles inside an `.mkv`, or an external subtitle given with its video; the offset is measured from the speech and low-confidence guesses are refused; a folder fixes what is off and skips what is right (`--dry-run`, `--in-place`, `-o`) |
| `filter` | Delete lines matching keywords (regex, dry-run supported) |
| `mux` | Merge video+audio+subs (softmux copy, `--hardsub` burn-in, `--batch-dir` seasons) |
| `convert` | Convert `srt` ↔ `ass` ↔ `vtt` (timing preserved) |
| `translate` | Translate via LibreTranslate or AI (tags preserved, resume cache, glossary) |
| `probe` | Full media metadata: format, streams, chapters, tags (`--json` for scripts) |
| `fetch` | Download subtitles via `yt-dlp` |
| `stats` | QC report: reading speed (CPS), durations, overlaps |
| `encode` | Batch-encode a folder from a TOML profile (ffmpeg → SVT-AV1/x265, Opus/AAC, mkvmerge): one audio language, no embedded subs/fonts, matching external subs added; `--engine staxrip` drives a StaxRip template instead |
| `tools` | Check, locate or install the external binaries (ffmpeg, MKVToolNix, SVT-AV1, x265, qaac, yt-dlp, ...) |
| `tui` | Terminal UI (ratatui): edit encode profiles in `subx.local.toml`, check/install tools, run encodes with a live log |
| `self-update` | Update subx from the latest GitHub release (SHA-256 verified; `--check`, `--to <tag>` for a rollback) |

```powershell
subx --help
subx extract "D:\Anime\Season1" -j 8 --audio
subx extract Ep01.mkv                                    # a single video works too
subx clean subs out -r --shift-ms -1500
subx sync Ep01.mkv                                       # retime the subtitles inside the video
subx sync "D:\Anime\Season1" --dry-run                  # a whole folder: look first, then --in-place
subx sync Ep01.en.srt Ep01.mkv                           # or an external subtitle, with its video
subx translate out out-id --engine ai --from en --to id
subx stats out-id --cps 20
subx mux --video Ep01.mkv --subs out\Ep01.id.ass -o Ep01_FANSUB.mkv
subx tui                                                 # profiles, tools, encodes
subx self-update --check                                 # is there a newer release?
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
SUBX_PARITY_NATIVE=a.mkv SUBX_PARITY_REFERENCE=b.mkv cargo test -- --ignored parity   # encode parity
```

Conventions: [`AGENTS.md`](AGENTS.md); architecture and roadmap:
[`docs/DESIGN.md`](docs/DESIGN.md). Every PR builds release binaries for all
supported targets (7-day artifacts); a `v*` tag publishes archives for
Windows x64/arm64, Linux x64/arm64 (static musl) and macOS arm64, plus
`SHA256SUMS` and a changelog.

## License

Public domain ([Unlicense](LICENSE)) — free for any purpose.
