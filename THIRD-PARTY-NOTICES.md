# Third-party source notices

## Pele flowchart parser

The bounded flowchart parser in `crates/scene/src/mermaid/` is adapted from
[Pele](https://github.com/obsidianmd/pele), revision
`3a2f075ee0c3d914f0fad3545fc0f193693975ac` (1.13.0),
`src/diagrams/flowchart/`. No renderer, icons or other assets are included.

MIT License

Copyright (c) 2026 kepano

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Mermaid-derived regression inputs

The input in `crates/scene/src/mermaid/read.rs`'s
`a_trailing_hyphen_belongs_to_the_id_before_an_odd_shape` test is reduced from
Pele's `tests/compat/flowchart/flow-text.spec.js:374` (the odd vertex whose id
ends in a minus). Pele records that input's source as
[Mermaid](https://github.com/mermaid-js/mermaid), version 12.1.0,
revision `21f72f07ea`.

In `crates/scene/src/mermaid/parse.rs`,
`bare_membership_mentions_reference_prior_explicit_nodes` combines the inputs
from `subgraph.spec.js:78` (the numeric-leading group id) and
`subgraph.spec.js:115` (the old-style group title). The class-suffix row in
`invalid_and_refused_sources_keep_their_codes` is minimized from
`flow-style.spec.js:181` (a class applied directly to a vertex). These are
regression strings embedded in the Rust tests, not vendored upstream suites.

`crates/scene/tests/mermaid.rs` reuses those inputs in
`unsupported_odd_shapes_after_a_trailing_hyphen_are_not_bad_arrows`,
`bare_membership_mentions_do_not_redeclare_a_shaped_node`,
`style_inside_an_unquoted_group_title_is_plain_text`, and
`a_class_suffix_after_a_shape_is_a_typed_refusal`. The five gate `.mmd` files
and `refused.json` are project-authored.

No upstream fixture directory or fixture inventory is vendored. The separate
offline compatibility audit is not shipped and is not a claim of full compatibility.

The MIT License (MIT)

Copyright (c) 2014 - 2022 Knut Sveidqvist

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## gpui-pre-windows 0.3.10 (`vendor/gpui-pre-windows/`)

- Upstream: Zed's `gpui_windows` crate (github.com/zed-industries/zed, `crates/gpui_windows`),
  published to crates.io as `gpui-pre-windows` 0.3.10, a snapshot of zed@08cbb88.
- License: Apache-2.0. The full text is in `vendor/gpui-pre-windows/LICENSE-APACHE`.
- Changes: Kumochi modifies six files in `src/` and adds `src/vsync/frame_demand.rs`, so that idle
  windows stop waking every vblank and hidden windows open at their requested size. Each modified
  file says so in its first line; the changes are described in [docs/development/gpui-patch.md](docs/development/gpui-patch.md).

## Retained wrapped-label painting

`crates/shell/src/presence/overlay.rs` adapts the glyph traversal, aligned row origins and
paint bounds from `gpui-pre` 0.3.10's `src/text_system/line.rs` (Zed GPUI,
https://github.com/zed-industries/zed), licensed Apache-2.0. The adaptation uses its public
layout/glyph APIs with one current color and no underline, strikethrough or background runs;
hard lines continue to use GPUI's public layout painter. No dependency source is modified.
The full Apache-2.0 license is included at `vendor/gpui-pre-windows/LICENSE-APACHE`.

## ptts 0.6.0 (`vendor/ptts/`)

- Upstream: [gradium-ai/xn-ptts](https://github.com/gradium-ai/xn-ptts), the crates.io
  `ptts` 0.6.0 package (SHA-256 `2b158aa0dc7770331c6b6efa64d292e28ff95952a042977fa9262390a2b9a824`).
- License: MIT OR Apache-2.0. Both full license texts are retained in `vendor/ptts/`.
- Changes: `src/synth.rs` uses fallible stream-worker creation and the existing stream
  cleanup to close and join a partially started pipeline. Focused startup regressions are
  included there. No model weights or native audio runtime are vendored.

## Voice models (downloaded on request, not in the repo)

Kumochi downloads these only when the user installs voice. Pins and provenance are in
[docs/development/voice.md](docs/development/voice.md).

- **Parakeet TDT 0.6B v3** by NVIDIA, licensed under
  [CC-BY-4.0](https://creativecommons.org/licenses/by/4.0/), from
  [nvidia/parakeet-tdt-0.6b-v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3). Kumochi
  uses the int8 ONNX export by the sherpa-onnx project
  (`csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8`), which quantizes and converts the
  weights.
- **Phonon 31M English** and the **Juniper** voice by Gradium, licensed under Apache-2.0, from
  [gradium/phonon-31m-en](https://huggingface.co/gradium/phonon-31m-en) and
  [gradium/phonon-voices](https://huggingface.co/gradium/phonon-voices).

## Voice native runtime (shipped beside `kumochid.exe`)

- **sherpa-onnx** 2.0.1 (`sherpa-onnx-c-api.dll`), Apache-2.0, from
  [k2-fsa/sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx). It includes kaldi-native-fbank,
  kaldi-decoder, kaldifst, OpenFst and simple-sentencepiece, each Apache-2.0.
- **ONNX Runtime** 1.28.2 (`onnxruntime.dll`), MIT, from
  [microsoft/onnxruntime](https://github.com/microsoft/onnxruntime). It includes protobuf, re2,
  abseil, flatbuffers, cpuinfo, ONNX, Eigen (MPL-2.0; source at
  [gitlab.com/libeigen/eigen](https://gitlab.com/libeigen/eigen)) and Boost (BSL-1.0), among the
  components its `ThirdPartyNotices.txt` lists. This is a summary: that file and the other upstream
  license and NOTICE texts ship beside the DLLs.

The audit behind this list is in [docs/development/voice-natives.md](docs/development/voice-natives.md).
