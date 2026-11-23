# Dependency audits for the Linux test host

The Linux test host builds the shared crates, not the GPUI shell. The default
`deny.toml` still audits the Windows and macOS target graphs; invoking it from
Linux doesn't change those targets. To audit the current portable Linux graph,
exclude the GPUI shell and explicitly select Linux:

```sh
cargo deny --locked --offline --target x86_64-unknown-linux-gnu \
  --exclude kumochi-shell check licenses bans sources
```

This includes protocol, scene, audio, shell-core, brain, and xtask, and
keeps build and development dependencies. Recheck the scope if the workspace
gains another native shell. Run `check advisories` on the same graph with an
available, sufficiently fresh RustSec database. If the database can't be read
offline, the advisory check hasn't run.

## Why the full Linux graph differs

The pinned GPUI shell brings in
`gpui-kit → gpui-pre-platform → gpui-pre-linux`. Its X11 support enables
`zed-xim`, whose `ahash/compile-time-rng` feature pulls in
`const-random → const-random-macro → tiny-keccak 2.0.2`. The last crate
provides SHAKE at proc-macro build time and declares only **CC0-1.0**. Other
consumers of the same `ahash` version may appear in a whole-workspace graph
because Cargo unifies features; they don't require this feature on the
portable graph.

Keep the Linux audit scoped while the Linux host is for portable tests only.
If a full Linux GUI graph later needs to pass, either review an owner-approved,
version-specific exception for that build-time dependency, or remove the
feature dependency and verify X11 input-method behavior still works. A scoped
pass isn't approval of the full Linux GUI graph or a reason to loosen the
global license list.
