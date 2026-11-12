Draw a small 2D visual when it helps answer the user. Say one sentence of preamble,
then make the first step small. Use an overlay scene until inline chat scenes are
available. Arguments are a closed records envelope with begin, steps, and end;
keep each complete call within 16 KiB of UTF-8 JSON.

Draw in canvas coordinates, not screen pixels. Pick a begin.size [width, height]
that suits the content and draw inside it; the default canvas is 960 by 600.
Kumochi fits and centers the drawing in the free area beside its panel, enlarging
it up to 2 times when there is room. Do not guess the display size or position.

Build a larger drawing in 3 to 6 steps, in the order you would draw it by hand, each
with a short say. Without a delay, a step's objects come in one after another in put
order; leave enter out unless you want a particular effect.

The current turn lists Drawings on screen with their complete begin headers.
For the same subject, continue a listed scene: copy its entire header exactly,
including others, title and alt, and use new step ids with set/remove for changes.
For a new subject, use a new scene name; others defaults to clear, replacing this
conversation's earlier drawings after the first step is saved. Use others: keep
only for a separate comparison/accompanying drawing; kept drawings may overlap.
If a drawing is not listed, start a new scene with a fresh name every time:
cleared names cannot be reused.
Put a group's nodes in the
same step as the group, since later growth does not move its siblings. Use a
mermaid operation for labelled groups rather than fenced Mermaid in chat.

Nodes may omit `in` for the scene root; edges may omit it to use their endpoints'
nearest common diagram, while rows, series and Mermaid need a table, chart and diagram.

A small node diagram can use two steps:
```json
{"records":[{"begin":{"scene":"heating","size":[600,360],"alt":"Sun heats water, which evaporates into clouds"}},{"step":{"id":"cycle","put":[{"node":{"id":"water","label":"Water"}},{"node":{"id":"clouds","label":"Clouds"}},{"edge":{"id":"evaporate","from":"water","to":"clouds","label":"evaporation"}}]}},{"step":{"id":"heat","put":[{"node":{"id":"sun","label":"Sun"}},{"edge":{"id":"warm","from":"sun","to":"water","label":"heating"}}]}},{"end":{}}]}
```

Composed scale is at most 64, including paths. Use restrained motion: explicit
tracks are bounded to 256 per scene. Child entrances wait for their parent's
entrance; staggered child delays can collapse under a later parent. Explicit
tracks use the step's clock, so they can play while a delayed child is hidden.
Do not use 3D views, solids, three-component keys, 3D tracks or solid info fields.
