# subx 2 — Design: native fansub pipeline

Status: phases 0 (CI and releases), 1 (quick wins) and 2 (tools module) are
implemented; phases 3–7 are pending. The current `encode` command drives StaxRip through its
command line; this document describes the target architecture that removes
that dependency while keeping the output equivalent, and the roadmap to get
there. `AGENTS.md` conventions apply throughout.

- [Goals and non-goals](#goals-and-non-goals)
- [Architecture](#architecture)
- [External binaries](#external-binaries)
- [Pipeline stages](#pipeline-stages)
- [Episode identity](#episode-identity)
- [Configuration and profiles](#configuration-and-profiles)
- [Execution model](#execution-model)
- [Windows specifics](#windows-specifics)
- [Supported platforms and releases](#supported-platforms-and-releases)
- [Performance budget](#performance-budget)
- [Parity test against StaxRip](#parity-test-against-staxrip)
- [Crate changes](#crate-changes)
- [Roadmap](#roadmap)
- [Open questions](#open-questions)

## Goals and non-goals

Goals

- One Rust binary that orchestrates proven native tools through subprocesses.
  No FFI, no `build.rs` linking of C libraries: every tool is an executable
  found on disk, checked before work starts, and installable on demand.
- Output equivalent to the StaxRip templates the project replaces: same
  decoder (libavcodec), same resizer (zimg), same video encoders (SVT-AV1 and
  x265 builds), same audio encoders (libopus; AAC-LC, HE-AAC and HE-AACv2
  through libfdk_aac or Apple's encoder), same muxer (mkvmerge), same flags.
  AV1+Opus and HEVC+AAC profiles are both first-class.
- Every stage verifies its own output (`mkvmerge -J`, `ffprobe`) before the
  next one runs, and failures keep their work files for inspection.
- Reproducible: encoder parameters live in TOML profiles under version control,
  tool versions are pinned, logs record the exact command lines.
- Windows-first, cross-platform where the tools exist (ffmpeg, mkvmerge,
  SVT-AV1 run on Linux too; VapourSynth and HE-AAC paths are Windows-only).

Non-goals (for now)

- GUI (egui/eframe). Settings fit in TOML profiles plus flags; a GUI would be a
  separate workspace crate later, never inside the CLI binary.
- Parsing StaxRip `.srip` templates (.NET BinaryFormatter). Profiles are
  transcribed once from StaxRip's logged command lines.
- In-process encoding through `libsvtav1` FFI. A pipe between two processes
  costs nothing measurable here.

## Architecture

```
            ┌────────────┐    ┌──────────────────────────────────────────┐
  CLI ─────▶│ commands/* │───▶│ core services                            │
 (clap)     └────────────┘    │  tools      find / check / install        │
                              │  episode    one parser for every command  │
                              │  profile    TOML profiles + macros        │
                              │  jobs       plan → run (-j) → verify      │
                              │  procs      spawn, pipe, progress, timeout│
                              └───────────────┬──────────────────────────┘
                                              ▼
       ffprobe / mkvmerge -J ──▶ probe ──▶ extract ──▶ subs ops ──▶ encode ──▶ mux ──▶ verify
                                  │          │  subs/audio/fonts/chapters        │
                                  │          └─────────── reuse ─────────────────┘
                                  └── metadata drives every later decision (languages, delay, DAR, color)
```

Module layout (new or changed):

| Module | Responsibility |
|---|---|
| `common/tools.rs` | tool table, resolution order, version probe, download/verify/extract |
| `common/episode.rs` | `EpisodeId` parser shared by `extract`, `mux`, `encode`, `subs/batch` |
| `common/profile.rs` | `[encode.profiles.*]` schema, macro expansion, validation |
| `common/procs.rs` | spawn with pipes, line-based progress parsing, timeout + kill tree |
| `media/mkv.rs` | typed view over `mkvmerge -J` (tracks, `minimum_timestamp`, chapters, attachments) |
| `commands/tools.rs` | `subx tools check \| install \| where` |
| `commands/encode/*` | `plan.rs`, `video.rs` (backends), `audio.rs`, `mux.rs`, `verify.rs`, `staxrip.rs` (legacy engine) |

## External binaries

Policy: tools come from their projects' official distribution channels. Where
the upstream project publishes no Windows binaries (SVT-AV1, x265), the
release channel StaxRip's own package manager designates is used instead: the
Patman builds, which are also what every StaxRip bundle ships and what the
parity test compares against. subx never forks, rebuilds, mirrors or vendors a
tool. `tools install` exists only where such a channel publishes binaries at a
stable URL; every other tool is `tools check` plus the link, and
`[tools] staxrip_dir` can reuse binaries an existing StaxRip bundle already ships.

Resolution order for every tool, first hit wins:

1. `--tools-dir` flag or `[tools] dir` in `subx.toml`
2. `tools/<name>/` next to `subx.exe`
3. `[tools] staxrip_dir` → StaxRip's `Apps` tree (reuses an existing bundle,
   nothing to download)
4. `PATH`

| Tool | Needed by | Status | Pinned source (verified 2026-10-20) | Integrity |
|---|---|---|---|---|
| `ffmpeg`, `ffprobe` | everything | required, installable | Windows x64: gyan.dev release build linked from ffmpeg.org, `gyan.dev/ffmpeg/builds/packages/ffmpeg-<ver>-full_build.7z` (9.0.2 at time of writing; has `zscale`, `libsvtav1`, `libopus`, `libx265`). Windows arm64 and Linux: BtbN `FFmpeg-Builds` `ffmpeg-master-latest-{winarm64,linux64,linuxarm64}-gpl.{zip,tar.xz}` | publisher `.sha256` / `checksums.sha256` |
| `mkvmerge`, `mkvextract`, `mkvpropedit` | mux, extract, verify | required, installable | `mkvtoolnix.download/windows/releases/<ver>/mkvtoolnix-64-bit-<ver>.zip` (103.0 at time of writing); Linux/macOS via the official repos or Homebrew (link) | publisher `.sha256` |
| `SvtAv1EncApp` | AV1 profiles | required for AV1, installable | `Patman86/SVT-AV1-Mods-by-Patman` releases (`.7z`, mainline / Essential / HDR variants; StaxRip's designated source, official GitLab publishes source only) | none published → warn; HTTPS + pinned tag |
| `x265` | HEVC profiles | required for HEVC, installable | `Patman86/x265-Mod-by-Patman` releases (`.7z`, multilib 8/10/12-bit; StaxRip's designated source, upstream publishes source only) | none published → warn; HTTPS + pinned tag |
| `qaac` | AAC-LC / HE-AAC (Apple encoder) | HE-AAC default, installable | `nu774/qaac` releases, `qaac_<ver>.zip`; **plus** Apple CoreAudio DLLs from Apple's own installer (Apple Application Support, or the `makeportable` script in the qaac wiki) | `qaac64 --check` must succeed |
| `libfdk_aac` (inside the user's ffmpeg) | AAC family | optional, if the installed ffmpeg has it | non-free: no official ffmpeg build includes it; detected with `ffmpeg -encoders` | — |
| `fdkaac` | AAC-LC / HE-AAC / HE-AACv2 | optional, if found | official `nu774/fdkaac` publishes source only (its CI builds Windows binaries but keeps no artifacts) | — |
| `exhale` | xHE-AAC (USAC) | optional, link only | official `gitlab.com/ecodis/exhale` releases | playback support is still limited |
| `opusenc` | exact StaxRip audio chain | optional | `archive.mozilla.org/pub/opus/win32/opus-tools-0.2-opus-1.3.zip` (libopus 1.3); a StaxRip bundle is newer | none |
| `vspipe` + VapourSynth | `source = "vapoursynth"` profiles | optional, link only | `vapoursynth/vapoursynth` releases, `VapourSynth64-Portable-R<N>.zip` | needs Python + plugins (ffms2); point at a StaxRip bundle |
| `yt-dlp` | `fetch` | optional | GitHub releases | publisher `SHA2-256SUMS` |
| LibreTranslate | `translate --engine libre` | optional service | `docker run -p 5000:5000 libretranslate/libretranslate` | — |

Opus goes through ffmpeg's `libopus` (`-c:a libopus -b:a 96k -vbr on
-apply_phase_inv 0`), the same library and switches as `opusenc --vbr
--bitrate 96 --no-phase-inv`; `opusenc` is used instead when found. AAC-LC
works with any ffmpeg (`aac`), upgraded to `qaac`/`libfdk_aac`/`fdkaac` when one
is available. HE-AAC needs `qaac` (default, officially distributed) or an
fdk-based encoder; HE-AACv2 needs fdk (Apple's encoder has no v2).
`tools::require` is profile-driven, so a run with only AV1+Opus profiles needs
exactly ffmpeg, SvtAv1EncApp and MKVToolNix, while HEVC+HE-AAC profiles add
x265 and an AAC encoder. The order encoders are tried in is `[encode] aac_encoders`.

`subx tools check` prints one line per tool: status, version, path, and for a
missing tool the official link and install hint for the current OS. `subx tools
install <name|all>` covers the installable rows (ffmpeg, MKVToolNix, SVT-AV1,
x265, qaac, opusenc, yt-dlp; VapourSynth stays link-only): download into
`tools/<name>/` while hashing, verify the publisher's checksum when one exists
(gyan.dev and MKVToolNix `.sha256`, BtbN `checksums.sha256`, yt-dlp `SHA2-256SUMS`;
otherwise print the SHA-256 that was downloaded and warn, since the Patman and
qaac releases publish none), extract `.zip` (`zip` crate), `.7z` (`sevenz-rust`)
or `.tar.xz` (system `tar`, present on every Linux/macOS), then re-run the
version probe. Versions are pinned in `tools.rs` (ffmpeg 9.0.2, MKVToolNix
103.0, yt-dlp 2026.08.19, the Patman tags); only BtbN is a rolling channel by
design. A `--latest` mode querying the release pages is a later addition.
Every command calls `tools::require(&[...])` before touching any file so the
failure is immediate and actionable.

## Pipeline stages

Each stage is a pure function from inputs on disk to outputs on disk, with a
verification step and an idempotent skip when the output already exists.

### Probe

`ffprobe -show_format -show_streams -show_chapters` for codecs, dimensions,
pixel format, frame rate, color primaries/transfer/matrix, and `mkvmerge -J`
for Matroska facts ffprobe reports poorly: per-track `minimum_timestamp`
(audio delay; 9 ms on typical BD remuxes), `language_ietf`, default/forced
flags, attachments, chapter count. Both results are cached per source file in
the job directory.

### Extract

- Subtitles: `mkvextract tracks` for MKV (keeps ASS headers byte-exact),
  ffmpeg for other containers. Naming stays `<name>.S<index>[.<title>].<lang>.<ext>`.
- Audio: stream copy, as today.
- Fonts: `mkvextract attachments` into `fonts/`, deduplicated by SHA-256, so a
  later mux can re-attach exactly the fonts a subtitle needs.
- Chapters: `mkvextract chapters` (XML and `--simple`).

### Subtitle operations

`clean`, `shift`, `filter`, `convert`, `translate`, `stats` keep their current
contracts. Two additions: `fonts` (list the fonts an ASS file references and
check them against `fonts/` and the system) and `--encoding auto` on readers
(`chardetng` is already a dependency; wire it or drop it).

### Encode

Video source backends, selected per profile:

- `ffmpeg` (default): `ffmpeg -i SRC -map 0:v:0 -vf <crop>,zscale=W:H:filter=spline36
  -pix_fmt yuv420p10le -f yuv4mpegpipe -strict -1 -`. `zscale` is libzimg,
  the same library behind VapourSynth's `resize.Spline36`; decoding is the same
  libavcodec ffms2 uses. For resize-only templates the frames are identical.
- `vapoursynth`: `vspipe --container y4m <script.vpy> -` where the script comes
  from the profile with `%source%`, `%width%`, `%height%`, `%crop_*%` macros.
  Needed only when a profile uses VS filters (denoise, deband, ...).

Encoders, fed through an OS pipe (`tokio::process::ChildStdout` → `Stdio`, no
userland copy):

- `svt-av1`: `SvtAv1EncApp -i stdin -b out.ivf --width W --height H
  --frames N <profile.video...> --color-primaries/--transfer-characteristics/
  --matrix-coefficients/--chroma-sample-position` derived from probe data,
  `--progress 2` parsed for the progress bar.
- `x265`: `x265 --y4m --input - --output out.hevc --output-depth 10 --frames N
  <profile.video...>` plus `--colorprim/--transfer/--colormatrix` from probe
  data, `--log-level info` progress parsed for the bar. The `.hevc` elementary
  stream carries no frame rate, so the mux passes `--default-duration
  0:<num>/<den>fps` from the probe.

Audio runs concurrently with video (as StaxRip already does): ffmpeg decodes
the chosen track to a pipe (`-f wav -c:a pcm_s24le`, or `-f flac` for
`opusenc`) feeding the codec's encoder. The track is chosen by `--audio-lang`
against `language` or `language_ietf`; its `minimum_timestamp` becomes the mux
`--sync`.

| `audio.codec` | Encoders tried in `aac_encoders` order | Command |
|---|---|---|
| `opus` | `opusenc` → ffmpeg `libopus` | `--vbr --bitrate N --no-phase-inv` / `-c:a libopus -b:a Nk -vbr on -apply_phase_inv 0` |
| `aac-lc` | `qaac` → ffmpeg `libfdk_aac` → `fdkaac` → ffmpeg `aac` | `qaac64 --cvbr N` / `-c:a libfdk_aac -profile:a aac_low -b:a Nk` / `fdkaac -p 2 -b N` / `-c:a aac -b:a Nk` |
| `he-aac` | `qaac` → ffmpeg `libfdk_aac` → `fdkaac` | `qaac64 --he --cbr N` / `-profile:a aac_he` / `fdkaac -p 5 -b N` |
| `he-aac-v2` | ffmpeg `libfdk_aac` → `fdkaac` (Apple's encoder has no v2) | `-profile:a aac_he_v2` / `fdkaac -p 29 -b N` |
| `xhe-aac` | `exhale` | `exhale <preset> in.wav out.m4a` |

A profile that asks for a codec no installed encoder can produce fails at plan
time with the `tools check` message for the missing encoder, never mid-batch.

Optional `crop = "auto"` runs `ffmpeg -vf cropdetect` on sampled frames and
rounds to the profile's `mod` (StaxRip's AutoCrop equivalent).

### Mux

`mkvmerge -o OUT --aspect-ratio 0:<DAR> out.ivf --language 0:<lang> --sync 0:<ms>
--default-track-flag 0:1 audio.opus [--language 0:<lang> --default-track-flag
0:<first> sub.ass]... --chapters chapters.xml [--attach-file font]...`.
External subtitles come from the same episode-matching used everywhere else;
fonts are attached only when a profile says so (the default for these releases
is none).

### Verify

`mkvmerge -J` on the result: exactly one video track with the expected
`pixel_dimensions`, one audio track in the expected language, the expected
subtitle count and languages, zero attachments unless requested, duration
within 0.5 s of the source. Only a verified episode deletes its work directory.

## Episode identity

One parser, `common/episode.rs`, replaces the three ad-hoc implementations
(`subs/batch.rs`, `mux.rs` stem matching, `encode.rs` regexes):

```
EpisodeId { season: Option<u8>, kind: Episode | Ova | Special | Movie, number: u16, version: Option<u8> }
```

Patterns, in priority order: `S01E09`, ` - 09`, `EP09`/`E09`, `OVA 1`/`OVA01`,
`SP1`, last standalone 1–3 digit number after stripping `[...]`, `(...)` and
`_`. Display form `09` / `OVA 1`; comparison form normalizes digits
(`OVA 01` == `OVA 1`, the current `key_token` bug). Collisions inside one folder
are an error, not a guess.

## Configuration and profiles

```toml
[tools]
dir = "tools"                 # relative to the executable
# staxrip_dir = "D:/Apps/StaxRip"   # reuse an existing bundle (local override)

[encode]
prefix = "[GROUP]"
audio_lang = "jpn"
sub_lang = "id"
jobs = 1
work_dir = ""                 # empty = <temp>/subx-encode
aac_encoders = ["qaac", "ffmpeg-fdk", "fdkaac", "ffmpeg-aac"]   # first available wins

[encode.profiles.anime480p]
tag = "BD 480p AV1.Opus"
width = 848
height = 480
mod = 4
crop = "none"                 # none | auto | "l,t,r,b"
source = "ffmpeg"             # ffmpeg | vapoursynth
# script = "scripts/anime.vpy"
encoder = "svt-av1"           # svt-av1 | x265
video = ["--preset", "4", "--crf", "35", "--tune", "2",
         "--adaptive-film-grain", "0", "--scd", "1", "--lookahead", "48"]
audio = { codec = "opus", bitrate = 96 }   # opus | aac-lc | he-aac | he-aac-v2 | xhe-aac
fonts = false

[encode.profiles.anime480p-hevc]
tag = "BD 480p HEVC.HE-AAC"
width = 848
height = 480
encoder = "x265"
video = ["--preset", "slow", "--crf", "22", "--output-depth", "10"]   # copy from StaxRip's x265 command line
audio = { codec = "he-aac", bitrate = 80 }
```

HEVC profiles are transcribed the same way as AV1 ones: open the template in
StaxRip, copy the x265 command line from its preview, paste it into `video`.

Secrets and machine paths go to `subx.local.toml` (gitignored) exactly as the
translate keys do today. `subx encode --profile anime480p` selects a profile;
every field is overridable by a flag. `video` is copied verbatim from the
encoder command line StaxRip logs for the template being replaced.

## Execution model

- Plan first: enumerate videos, match subtitles, compute targets, skip existing
  outputs, print the table. `--dry-run` stops here and prints the full command
  lines.
- Jobs run with `-j N` (default 1). SVT-AV1 at 480p reports a parallelism
  level of 6 on a 24-thread CPU, so 2–3 concurrent episodes are expected to
  raise total throughput; measure before changing the default.
- Progress: one `indicatif` bar per job, fed by parsed encoder output; `-q`
  prints only the final table.
- Temp layout: `<work_dir>/<episode>/{source.cache.json, video.ivf,
  audio.opus, chapters.xml, encode.log}`. Interrupted runs resume stage by
  stage (an existing verified `video.ivf` is reused).
- Timeouts are activity-based: a job is killed when its encoder output file has
  not grown for `stall_min` (default 5), not after a fixed wall clock.
- Logging: `tracing` to stderr plus `encode.log` per episode containing every
  command line and the tool output, so a failure can be replayed by hand.
- Exit codes: `0` all ok, `1` at least one episode failed, `2` usage/config
  error, `3` missing tool.

## Windows specifics

- Long paths: prefix absolute paths with `\\?\` when they exceed 240 chars;
  mkvmerge and ffmpeg accept it.
- Filenames: keep `common/fs::sanitize_filename` for generated names; never
  alter user-supplied output names.
- Process trees: kill with `taskkill /T /F` so ffmpeg does not outlive a killed
  encoder; on Unix use process groups.
- `tasklist` is only used by the legacy StaxRip engine; the native engine has
  no shared state and needs no running-instance check.

## Supported platforms and releases

"Binary" here means the compiled `subx` executable. External tools are never
shipped with it (see the official-only policy above).

| Tier | Target | Runner | Promise |
|---|---|---|---|
| 1 | `x86_64-pc-windows-msvc` | `windows-latest` | everything, including the StaxRip engine, VapourSynth and qaac paths; unit + e2e tests in CI |
| 2 | `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` | `ubuntu-latest`, `ubuntu-24.04-arm` | all subtitle commands; native encode with tools from `PATH` or `tools install ffmpeg`; unit + e2e tests (ffmpeg from apt) |
| 3 | `aarch64-pc-windows-msvc`, `aarch64-apple-darwin` | `windows-11-arm`, `macos-15` | built, `subx --version` smoke test and unit tests only |

musl targets give static binaries that run on any glibc; `reqwest` already uses
`rustls`, so no OpenSSL is involved. Every target builds natively on its own
runner (`cargo build --release --target <t>`, `musl-tools` from apt on Linux),
no cross toolchains.

`ci.yml` (every push and PR; `permissions: contents: read`; concurrency cancels
superseded runs):

- `check` (ubuntu): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, with `Swatinem/rust-cache`.
- `msrv` (ubuntu): `cargo check` on the toolchain named by `rust-version`.
- `deny` (ubuntu): `EmbarkStudios/cargo-deny-action` — advisories, licenses, duplicate crates; justified by the download code in `tools.rs`.
- `actionlint`: lints the workflows themselves.
- `build` (matrix over all tiers): release build per target, uploaded as a 7-day artifact so every PR yields testable binaries.
- `e2e` (ubuntu + windows): installs ffmpeg (apt / choco), runs `cargo test -- --ignored`.

`security.yml` (push, PR, weekly cron): `aquasecurity/trivy-action` filesystem
scan — Cargo.lock vulnerabilities, secrets, misconfigurations — failing on
fixable HIGH/CRITICAL findings; the weekly run catches CVEs published between
code changes. `make trivy` runs the same scan locally.

`release.yml` (tags `v*`; `permissions: contents: write`):

1. Guard: the tag must equal `Cargo.toml` `version`, otherwise fail before building.
2. Build matrix identical to `ci.yml` (shared through a reusable `workflow_call` job so the two files cannot drift).
3. Package per target: `subx-<version>-<target>.zip` on Windows, `.tar.xz` elsewhere, containing the binary, `README.md`, `LICENSE`, `subx.toml`, `subx.local.toml.example`.
4. `SHA256SUMS` over every archive; changelog from `cliff.toml` via `orhun/git-cliff-action` (replaces the current `docker run`).
5. `softprops/action-gh-release` with `fail_on_unmatched_files: true`.

Actions are pinned to full commit SHAs with the version in a comment, refreshed
by Dependabot (`github-actions` and `cargo` ecosystems, weekly). Versions
current on 2026-10-20: `actions/checkout` v7.0.1, `actions/upload-artifact`
v7.0.2, `actions/download-artifact` v8.0.2, `dtolnay/rust-toolchain` v1,
`Swatinem/rust-cache` v2.9.2, `softprops/action-gh-release` v3.0.3,
`orhun/git-cliff-action` v4.9.1, `EmbarkStudios/cargo-deny-action` v2.1.1,
`aquasecurity/trivy-action` v0.36.0, `rhysd/actionlint` v1.7.12. `[profile.release]` gets `lto = "fat"`,
`codegen-units = 1`, `panic = "abort"` next to the existing `strip = true`.

## Performance budget

Measured on the reference run (848x480, SVT-AV1 preset 4, 34 072 frames,
Ryzen 9 9900X): video 112 s, audio 11 s concurrent, demux + index + autocrop
+ work copy + StaxRip start ≈ 10 s. The native pipeline removes that last
block and the 250 MB work copy; the encoder still dominates, so per-episode
gains are under 10 %. The real lever is `-j`, available only without StaxRip.
Build-side: LTO, `codegen-units = 1`, `panic = "abort"`, and dropping unused
crates reduce binary size and CI time without touching runtime behavior.

## Parity test against StaxRip

Keep `--engine staxrip` until parity is proven, then encode one episode both
ways and compare:

1. `mkvmerge -J`: identical track layout, dimensions, languages, flags, delay.
2. `ffmpeg -i a.mkv -i b.mkv -lavfi "[0:v][1:v]ssim" -f null -` → SSIM ≥ 0.99
   (identical frames give 1.0; differences then come from encoder build/threads).
3. Audio: duration and `--sync` equal; loudness within 0.1 LU (`ebur128`).

The test is a `#[ignore]`d end-to-end test that takes the two files as
environment variables, so CI never needs the binaries.

## Crate changes

Remove: `chardetng`, `encoding_rs`, `console`, `rayon`, `similar`, `thiserror`
(unused), `once_cell` (use `std::sync::LazyLock`). Add: `zip` (deflate only),
`sevenz-rust`, `sha2`; `.tar.xz` goes through the system `tar`, so no C
dependency is linked. Trim: `reqwest` with `default-features = false` (one TLS stack),
`tokio` to `rt-multi-thread, macros, process, fs, io-util, time, sync`.
`Cargo.toml`: `rust-version`, release profile `lto = "fat"`, `codegen-units = 1`,
`panic = "abort"`. Remove the crate-level `#![allow(dead_code)]`.

## Roadmap

Each phase is one commit, lands green on `cargo fmt --check && cargo clippy --
-D warnings && cargo test`, and ends with a `--dry-run` or real run on a sample
folder the user provides.

| Phase | Scope | Acceptance |
|---|---|---|
| 0 CI and releases | rewrite `ci.yml`/`release.yml` per [Supported platforms and releases](#supported-platforms-and-releases): tier matrix, musl targets, reusable build job, tag guard, archives + `SHA256SUMS`, git-cliff action, cargo-deny, MSRV, actionlint, Dependabot, SHA-pinned actions, release profile | `actionlint` clean locally; a PR run uploads 5 artifacts; a `v*` tag on a test fork publishes 5 archives + `SHA256SUMS` + changelog; README release line updated |
| 1 Quick wins | crate changes above; fix `key_token` digit normalization; move personal defaults (`prefix`, `tag`, StaxRip path) from flags to `subx.toml` / `subx.local.toml`; wire `AppConfig` into `dispatch` | gates green; `--only "OVA 01"` matches; `--help` shows no machine paths |
| 2 Tools | `common/tools.rs`, `subx tools check\|install\|where`, all commands use `tools::require` | `tools check` lists every tool with version; `tools install mkvtoolnix` verifies `.sha256`; removing a tool from `PATH` yields exit 3 with a link |
| 3 Episode identity | `common/episode.rs`; `extract`, `mux --batch-dir`, `encode` share it | unit table of 20+ real file names; old behavior preserved for `Ep01.id.ass` stems |
| 4 Native encode | `--engine native` (default stays `staxrip` until phase 6), profiles, ffmpeg source, SVT-AV1 **and** x265, Opus **and** the AAC family (`aac_encoders` fallback chain, `qaac` install + Apple DLL check), mkvmerge, verify, `-j`, stall watchdog, per-episode log | one AV1+Opus and one HEVC+HE-AAC episode encode end to end and pass verify; a profile whose AAC encoder is missing fails at plan time with the install hint; `--dry-run` prints every command line |
| 5 Fonts, chapters, VapourSynth, crop | `extract` fonts/chapters, `fonts` command, `source = "vapoursynth"`, `crop = "auto"`, `xhe-aac` via exhale | VS backend runs a resize-only script with identical SSIM to the ffmpeg backend; `cropdetect` result matches StaxRip's AutoCrop on the reference episode |
| 6 Parity and switch | parity test, `--engine native` default, StaxRip engine kept as `--engine staxrip`, docs | SSIM ≥ 0.99 on the reference episode; `docs/USAGE.md` and `README.md` updated |
| 7 Optional | TUI progress (`ratatui`) or separate `subx-gui` crate | only if asked |

## Open questions

- SVT-AV1 and x265 come from the Patman releases without published checksums.
  Pin one tag per subx release (reproducible, matches the parity test) or let
  `--latest` be the default for people who want the newest encoder?
- Should `extract` default to `mkvextract` for MKV subtitles (byte-exact ASS)
  or keep ffmpeg for a single code path?
- HE-AAC: `qaac` is the only officially distributed encoder, and it needs Apple
  DLLs the user installs separately. The StaxRip HE-AAC template on the
  reference machine selected `fdkaac`, which is not installed there, so that
  template has never produced output — the native path must not inherit that
  silent failure (`tools check` must say so up front).
