# Drawings

Conversation history shows **Drawing unavailable in history** for stored drawing steps.
It keeps surrounding text, but does not reconstruct the picture there. This message does
not mean a drawing still visible on screen has failed, and reading history does not change it.

When Kumochi draws a 2D diagram, each step appears on screen once it is prepared, and its parts
draw or fade in one after another unless the drawing times them itself. Drawings that were on
screen come back after Kumochi reconnects. If you stop a drawing, the parts that
had already finished appearing stay on screen.

Asking for a change to the same subject continues its drawing. A new subject normally
replaces this conversation's earlier drawings once the new drawing's first step is saved.
Normally their last picture stays briefly until the new drawing starts on the same drawing
layer. This visual hold lasts at most 250 ms while the interface is running and can end sooner
when space is tight; it does not keep the old drawings available for further changes. If the
replacement then fails, the screen may be left empty and its conversation gets one notice that
the previous drawings were removed, even if the drawing call had already ended. This includes
a rejected later drawing record or a failed save after the replacement's first step was saved;
the error keeps its specific cause. Stopping a drawing yourself does not report a drawing failure.
The notice appears as soon as the replacement fails. Kumochi may still correct the drawing in
the same reply, so a new drawing can appear after the notice; the earlier drawings stay removed.
If Kumochi tries to draw but none of its drawing can be shown, its reply ends with the notice
**No drawing came back**. Ask again to retry.
Separate drawings can be kept for comparison, up to four on
screen; they can still overlap until side-by-side arrangement is available.

Simple node-and-arrow diagrams do not need a separate container: their nodes belong to
the scene itself, and each arrow follows the diagram shared by its endpoints. These
diagrams have the same 48-node and 96-arrow limits as explicitly grouped diagrams. In
practice, what can be painted is smaller: about 40 filled circles or ellipses across the
drawings on screen, or a diagram of about 30 boxes with labelled arrows. A step past that isn't
shown. If Kumochi is still drawing when that happens, it's told to use fewer or simpler shapes.

For function graphs, Kumochi asks the model for sampled values on a chart, with y growing
upward, rather than a hand-drawn path. Curves draw in along those samples; functions such
as 1/x use separate branches across a discontinuity. The model still supplies the values
and ranges: Kumochi does not calculate the function or guarantee its mathematics.

Drawings appear on the main display, over your other windows. They never take focus or catch
clicks, so the app underneath keeps working. While one is on screen, Esc clears it
(see [Esc](escape.md)). Kumochi centers each drawing in the free area beside its panel,
shrinks it to fit, or enlarges it up to twice its canvas size when there is room. A drawing
uses its declared canvas, or its content bounds when no canvas size was given. Moving the
dock, opening or closing a typing, Threads or Settings panel, or changing the display
re-places the drawing immediately, without redrawing its steps. When there is room, the free
area stays clear of the open panel and reserves room for the smaller status peek even when
it is hidden. On a display too small for both, an open panel can cover a drawing and its
entrance can play unseen.

This update changes how drawing coordinates are laid out. Drawings left on screen by an
older version are cleared on reconnect; conversation history is kept.

A shape whose outline crosses itself, like a star, is filled everywhere inside it, its middle
included. Labels stay upright even when the shape around them is turned. Where translucent
parts of a drawing overlap, the overlap looks darker, as with layered tracing paper.

A drawing at rest uses no animation frames, and when nothing is on screen the drawing layer is
hidden. If part of a drawing can't be painted, Kumochi clears the whole drawing rather than
leave only part of it on screen.
