# Kumochi

A local-first desktop AI presence: a GPUI shell (`kumochi.exe`) and a headless brain
(`kumochid.exe`). Windows first. How it works inside: `docs/development/architecture.md`.

## Commands
- Build: `cargo build --workspace` (the toolchain is pinned in `rust-toolchain.toml`)
- Test: `cargo test --workspace`. Tests never touch the network. The one exception: a test may bind `127.0.0.1:0` and talk to that socket. No other address, port, host name or DNS.
- Lint: `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings`
- Licenses, advisories, duplicates: `cargo deny check`
- Dev tasks: `cargo xtask help`
- CI (`.github/workflows/ci.yml`, Windows) runs fmt, clippy and tests; all must pass.
  Deny runs for dependency/CI configuration changes and every scheduled or manual run.
  Cloud runs: `cloud-ci` PRs into main, `batch/**` pushes, manual dispatch, daily main checks.

## Layout
```
crates/protocol/   shared data: Secret, and constants.rs (every product name)
crates/scene/      the drawing format, validator and layout
crates/brain/      bin kumochid: the headless process
crates/shell/      bin kumochi: the GPUI app
xtask/             dev tasks in Rust; no scripts in other languages
docs/development/  how Kumochi works inside
docs/user/         how people use Kumochi
locales/en/        every user-facing string (Fluent)
THIRD-PARTY-NOTICES.md  notices for incorporated third-party source
```

## Code rules
- Names that change with the product live only in `crates/protocol/src/constants.rs`.
  Debug builds use the "Dev" names, so they never touch an installed Kumochi.
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
- The one exception: Claude Code's own credential file may exist only inside Kumochi's Claude
  Code folder, readable by the user and SYSTEM only. Kumochi never reads, copies or relays its
  contents, and Disconnect removes it (logout, then delete). Nothing else is exempt.
- Never log prompts, transcripts, screen text, images or tool arguments.
- Screenshots go only to the model the user chose.
- Use only the connections the user sets up inside Kumochi. Never read, copy or depend on credentials, tokens, sessions or config that other apps or the developer's machine hold (Claude Code, Codex, gh, browsers and the like).
- Clean room: nothing from HeyClicky's code, protocol, schemas, prompts, assets or identifiers.
- Kumochi is `AGPL-3.0-only`, but no one else's GPL, LGPL or AGPL code ships in it: new
  dependencies must pass `deny.toml`'s license list.
