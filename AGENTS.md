# Capsula

A local-first desktop AI presence: a GPUI shell (`capsula.exe`) and a headless brain
(`capsulad.exe`). Windows first. How it works inside: `docs/development/architecture.md`.

## Commands
- Build: `cargo build --workspace` (the toolchain is pinned in `rust-toolchain.toml`)
- Test: `cargo test --workspace`. Tests never touch the network.
- Lint: `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings`
- Licenses, advisories, duplicates: `cargo deny check`
- Dev tasks: `cargo xtask help`
- CI (`.github/workflows/ci.yml`, Windows) runs fmt, deny, clippy and tests; all must pass.

## Layout
```
crates/protocol/   shared data: Secret, and constants.rs (every product name)
crates/scene/      the drawing format, validator and layout (empty until slice 4)
crates/brain/      bin capsulad: the headless process
crates/shell/      bin capsula: the GPUI app
xtask/             dev tasks in Rust; no scripts in other languages
docs/development/  how Capsula works inside
locales/en/        every user-facing string (Fluent)
```

## Code rules
- Names that change with the product live only in `crates/protocol/src/constants.rs`.
  Debug builds use the "Dev" names, so they never touch an installed Capsula.
- `unsafe` only in a crate's `platform` module, one operation per block, each with `// SAFETY:`.
- No `unwrap()` outside tests. `thiserror` between modules; `anyhow` only in `xtask`.
- tokio runs in the brain only. The shell's UI thread never blocks.
- Workspace lints live in `Cargo.toml`; silence one with `#[expect(..., reason = "...")]`.
- gpui-kit is pinned to a git revision in `[patch.crates-io]`; upgrading it is its own change.
- A module's `docs/development/` page changes in the same pull request as the module.
- A hot-path change (rendering, the pipe, stream parsing, audio) carries a before-and-after
  measurement from Rez.

## Commits
- Conventional Commits for commits and PR titles: `type(scope): summary`, with type feat, fix,
  docs, refactor, perf, test, build, ci or chore, a crate or area as scope, and a short
  lowercase imperative summary. PRs are squash-merged under their title.
- Commits are the owner's: no AI attribution, no `Co-authored-by` or "Generated with" lines.

## Rules that protect users
- No telemetry. Nothing leaves the machine except calls to accounts the user set up.
- Secrets live only in Windows Credential Manager: never in logs, command lines, files or the
  environment. Hold them in `Secret`, which prints `***`.
- Never log prompts, transcripts, screen text, images or tool arguments.
- Screenshots go only to the model the user chose.
- Never read or store Claude Code's credentials.
- Clean room: nothing from HeyClicky's code, protocol, schemas, prompts, assets or identifiers.
- New dependencies must pass `deny.toml`'s license list. Nothing GPL-family ships.
