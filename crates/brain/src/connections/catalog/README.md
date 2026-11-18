# Captured ChatGPT model list

`captured_models.json` is the model-list response captured on 2026-11-13 at
06:18 UTC, using the [Dev capture added in #264](https://github.com/labcoder/capsula/pull/264)
from source `aebdb7edfbad848ab43d65805bcbfe531567cf22`. [#324](https://github.com/labcoder/capsula/pull/324)
landed the trimmed body. The original scrubbed envelope has SHA-256
`131c271d3aae35b1378de5b14b082d3945f6fcc79c064b372905945620545cf0`;
the tracked fixture has SHA-256
`c2051a7846d470aeeec09c5545bb260f475e23e0e52aa61578a0ab94d788c670`.
The ordered slug, visibility, effort and default projections match.

The ten rows preserve their order and scrubbed model identities. Eight are
listed. The captured effort shape is `supported_reasoning_levels[].effort`;
a scrub placeholder is not an effort. The default field does not establish
support. This is historical evidence of a field's shape, not of any named
model's current availability or supported efforts.

The capture parses JSON into a Value before scrubbing. It preserves admission
only for unique-key objects and normalizes duplicate keys last-wins. Do not
infer the production decoder's duplicate-key rejection from the fixture.
