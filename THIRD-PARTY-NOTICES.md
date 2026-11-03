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
  windows stop waking every vblank. Each modified file says so in its first line; the change is
  described in [docs/development/gpui-patch.md](docs/development/gpui-patch.md).
