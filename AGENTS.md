# AGENTS.md — subx (repo-local conventions)

Project conventions for AI agents working in this repo. The global pi workflow
(`Inspect → Implement → Validate → Review`, smallest correct change, never run
`git commit` unless explicitly asked) still applies.

## Language policy (enforced, decided 2026-09-24)

- Code comments (`//`, `///` — including rustdoc that surfaces in `--help`): **English**.
- User-facing messages (CLI output, `--help`, errors, logs): **English**.
- User docs (`README.md`, `docs/USAGE.md`,
  `subx.toml` comments, `subx.local.toml.example`): **English**.
- Commit messages: **English**.
- Chat with the user: Indonesian by default; code and docs stay English.

## Secrets

- API keys only via CLI flag, env (`OPENAI_API_KEY`, `OPENROUTER_API_KEY`,
  `DEEPSEEK_API_KEY`, `GEMINI_API_KEY`, `SUBX_API_KEY`, ...), or
  `subx.local.toml` (gitignored). Never print keys to logs, never commit them.
- `subx.toml` (committed) must never contain a real key — placeholders only.

## Rust project

- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` must be
  green after every change. Windows-first (PowerShell paths, sanitize filenames).
- Keep each task scoped; no out-of-scope refactors.
