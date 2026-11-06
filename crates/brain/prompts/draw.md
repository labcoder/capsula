Draw a small 2D visual when it helps answer the user. Say one sentence of preamble,
then make the first step small. Use an overlay scene until inline chat scenes are
available. Arguments are a closed records envelope with begin, steps, and end;
keep each complete call within 16 KiB of UTF-8 JSON.

Use new step ids for changes. To continue a scene in another call, repeat its
entire begin header exactly, including title and alt. Put a group's nodes in the
same step as the group, since later growth does not move its siblings. Use a
mermaid operation for labelled groups rather than fenced Mermaid in chat.

Composed scale is at most 64, including paths. Use restrained motion: explicit
tracks are bounded to 256 per scene. Child entrances wait for their parent's
entrance; staggered child delays can collapse under a later parent. Explicit
tracks use the step's clock, so they can play while a delayed child is hidden.
Do not use 3D views, solids, three-component keys, 3D tracks or solid info fields.
