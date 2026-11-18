# Release notices

`cargo xtask notices` generates `assets/licenses/third-party.txt`. `--check` compares it
with a fresh generation and fails if it is missing or stale. Neither command launches
Kumochi, accesses its accounts or downloads voice assets.

The Windows closure starts at `kumochi-shell` and `kumochi-brain`, with their shipped
default features. Locked, platform-filtered Cargo metadata supplies package identities
and license paths. `cargo tree --locked --target x86_64-pc-windows-msvc -p kumochi-shell
-p kumochi-brain --edges normal` supplies the production-root selection: workspace
metadata alone unifies development features and otherwise includes test-only optional
dependencies. Build/dev edges and workspace-owned packages are not included. The
normal-edge graph deliberately keeps proc-macro crates and their host dependencies,
since code expanded by those macros ships. Ambiguous name/version identities fail
rather than guessing a package source.

The generator reads `deny.toml`'s license allow list, named exceptions and confidence
threshold. SPDX expressions select one permitted OR branch and every required AND
part, preferring license texts found in the package's files. If text for a permitted
choice is missing, the output names the choice and uses canonical SPDX text. It
preserves actual copyright notices, including continuations and scope, from the
package rather than turning them into a list of guessed holders. Where the published
legal files and source headers have no copyright line, it labels attribution from
the manifest's authors or repository. If authors aren't listed, it says “The <crate>
authors” without asserting a holder or year. A selected MPL-2.0 grant also carries an
exact-version source URL. No permitted choice, no usable attribution, or no
identifiable text for a `license-file` without an expression fails closed.

Identical license texts are grouped. Nested legal files and leading legal comments
in Rust and native source files appear separately; they cannot satisfy the crate's
own SPDX expression. The scan omits standard Cargo test, fixture, benchmark and
example directories, the root build script, and the package/version-specific
non-production paths listed with reasons in `notices/sources.rs`. It fails on
unreadable legal inputs. A git package may inherit its repository's top-level license.
A dependency change requires regeneration and review of the changed output.

Some published crates omit their repository's license files. Their exact-version
upstream texts live in `assets/licenses/upstream/<name>-<version>/`, with a provenance
notice naming the source revision, URL and text hash. Those files are reviewed input,
not a generic-license fallback. The generator never fetches them automatically and
cannot silently reuse one version's text for a new version.

`assets/licenses/inventory.json` separately pins model files, native libraries and
embedded Lucide/Feather icons. It records sources, licenses and required notice texts.
Every current voice-manifest file must be covered exactly once with its hash and source;
the icon package revision must match. The pinned native GPUI asset bundle contains icons,
not its web-story fonts. New embedded fonts or a new asset license require review.

Parakeet's CC-BY-4.0 model attribution includes its ONNX/int8 conversion.
Phonon and Juniper notices use the retained Gradium Phonon Apache-2.0 grant, not
GPUI's separately attributed Apache text. The native
runtime's existing Eigen MPL-2.0 and Boost BSL-1.0 exceptions do not broaden crate policy.
The full retained ONNX Runtime notice includes optional components; carrying that notice
does not establish their presence or an exact-build SBOM. The additional prerequisites
in [voice-natives.md](voice-natives.md) still apply before packaging the DLLs.

Output keeps handwritten source notices first, asset notices second and grouped crate
license texts last. CI checks freshness; this is the notice file for packaging and About.
