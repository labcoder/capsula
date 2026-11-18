# Release notices

`cargo xtask notices` generates `assets/licenses/third-party.txt`. `--check` compares it
with a fresh generation and fails if it is missing or stale. Neither command launches
Kumochi, accesses its accounts or downloads voice assets.

The Windows closure starts at `kumochi-shell` and `kumochi-brain`, with their shipped
default features. Locked, platform-filtered Cargo metadata supplies package identities
and license paths. `cargo tree --locked --target x86_64-pc-windows-msvc -p kumochi-shell
-p kumochi-brain --edges normal` supplies the production-root selection: workspace
metadata alone unifies development features and otherwise includes test-only optional
dependencies. Build/dev edges and workspace-owned packages are not included. Ambiguous
name/version identities fail rather than guessing a package source.

The generator reads `deny.toml`'s license allow list, named exceptions and confidence
threshold. SPDX expressions select one permitted OR branch and every required AND
part, preferring license texts found in the package's files. If text for a permitted
choice is missing, the output names the choice and uses canonical SPDX text with the
published crate's copyright notice. Where the license and notice files carry no
copyright notice, the output says so and gives the published manifest's authors and
repository URL. If the manifest has no authors, it says “The <crate> authors” without
asserting a copyright holder or year. The generator preserves unclassified legal
files and additional copyright notices under a separate label, so a classifier
miss doesn't discard embedded notices. No permitted choice, no usable attribution,
or no identifiable text for a `license-file` without an expression fails closed.

Identical license texts are grouped; each package's copyright and NOTICE files follow
too. A git package may inherit its repository's top-level license. A dependency change
requires regeneration and review of that output.

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

Parakeet's CC-BY-4.0 model attribution includes its ONNX/int8 conversion. The native
runtime's existing Eigen MPL-2.0 and Boost BSL-1.0 exceptions do not broaden crate policy.
The full retained ONNX Runtime notice includes optional components; carrying that notice
does not establish their presence or an exact-build SBOM. The additional prerequisites
in [voice-natives.md](voice-natives.md) still apply before packaging the DLLs.

Output keeps handwritten source notices first, asset notices second and grouped crate
license texts last. CI checks freshness; this is the notice file for packaging and About.
