# Drawings

Conversation history shows **Drawing unavailable in history** for stored drawing steps.
It keeps surrounding text, but does not reconstruct the picture there. This message does
not mean a drawing still visible on screen has failed, and reading history does not change it.

When Kumochi draws a 2D diagram, each step appears on screen once it is prepared, and drawings
that were on screen come back after Kumochi reconnects. If you stop a drawing, the parts that
had already finished appearing stay on screen.

Simple node-and-arrow diagrams do not need a separate container: their nodes belong to
the scene itself, and each arrow follows the diagram shared by its endpoints. These
diagrams have the same 48-node and 96-arrow limits as explicitly grouped diagrams.

Drawings appear on the main display, over your other windows. They never take focus or catch
clicks, so the app underneath keeps working. While one is on screen, Esc clears it
(see [Esc](escape.md)). Kumochi centers each drawing in the free area beside its panel,
shrinks it to fit, or enlarges it up to twice its canvas size when there is room. A drawing
uses its declared canvas, or its content bounds when no canvas size was given. Moving the
dock, opening or closing a typing, Threads or Settings panel, or changing the display
re-places the drawing immediately, without redrawing its steps. The free area stays clear
of the open panel and reserves room for the smaller status peek even when it is hidden.

This update changes how drawing coordinates are laid out. Drawings left on screen by an
older version are cleared on reconnect; conversation history is kept.

A shape whose outline crosses itself, like a star, is filled everywhere inside it, its middle
included. Labels stay upright even when the shape around them is turned. Where translucent
parts of a drawing overlap, the overlap looks darker, as with layered tracing paper.

A drawing at rest uses no animation frames, and when nothing is on screen the drawing layer is
hidden. If part of a drawing can't be painted, Kumochi clears the whole drawing rather than
leave only part of it on screen.
