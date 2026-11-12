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

Graph functions with a line chart and series of [x,y] values in math coordinates,
not hand-written paths. Set x.min/max and y.min/max; Kumochi maps y upward.
Use radians for sin/cos and 40–80 samples in increasing x, including endpoints,
zeros and extrema. Check one period on 0..2π, increasing x³ and even x².
Keep samples in range; split 1/x into separate same-colour series on each side
of zero, never sampling or connecting across it. For "add cos x", continue the
listed scene with its header copied exactly and put a new series in its existing
chart, keeping the domains. A sampled sine chart looks like this:
```json
{"records":[{"begin":{"scene":"sine","size":[400,300],"alt":"One sine period from zero to two pi"}},{"step":{"id":"graph","put":[{"chart":{"id":"plot","kind":"line","box":[0,0,400,300],"x":{"min":0,"max":6.283185307179586,"label":"x (radians)"},"y":{"min":-1,"max":1}}},{"series":{"id":"sin","in":"plot","data":[[0,0],[0.15708,0.156434],[0.314159,0.309017],[0.471239,0.45399],[0.628319,0.587785],[0.785398,0.707107],[0.942478,0.809017],[1.099557,0.891007],[1.256637,0.951057],[1.413717,0.987688],[1.5707963267948966,1],[1.727876,0.987688],[1.884956,0.951057],[2.042035,0.891007],[2.199115,0.809017],[2.356194,0.707107],[2.513274,0.587785],[2.670354,0.45399],[2.827433,0.309017],[2.984513,0.156434],[3.141592653589793,0],[3.298672,-0.156434],[3.455752,-0.309017],[3.612832,-0.45399],[3.769911,-0.587785],[3.926991,-0.707107],[4.08407,-0.809017],[4.24115,-0.891007],[4.39823,-0.951057],[4.555309,-0.987688],[4.71238898038469,-1],[4.869469,-0.987688],[5.026548,-0.951057],[5.183628,-0.891007],[5.340708,-0.809017],[5.497787,-0.707107],[5.654867,-0.587785],[5.811946,-0.45399],[5.969026,-0.309017],[6.126106,-0.156434],[6.283185307179586,0]]}}]}},{"end":{}}]}
```

Composed scale is at most 64, including paths. Use restrained motion: explicit
tracks are bounded to 256 per scene. Child entrances wait for their parent's
entrance; staggered child delays can collapse under a later parent. Explicit
tracks use the step's clock, so they can play while a delayed child is hidden.
Do not use 3D views, solids, three-component keys, 3D tracks or solid info fields.
