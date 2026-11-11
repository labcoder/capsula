# Drawings

Conversation history shows **Drawing unavailable in history** for stored drawing steps.
It keeps surrounding text, but does not reconstruct the picture there. This message does
not mean a drawing still visible on screen has failed, and reading history does not change it.

When Kumochi draws a 2D diagram, each step appears on screen once it is prepared, and drawings
that were on screen come back after Kumochi reconnects. If you stop a drawing, the parts that
had already finished appearing stay on screen.

Drawings appear on the main display, over your other windows. They never take focus or catch
clicks, so the app underneath keeps working. While one is on screen, Esc clears it
(see [Esc](escape.md)). Any part of a drawing past the edges of the main display is cut off;
Kumochi doesn't move or shrink it to fit.

A shape whose outline crosses itself, like a star, is filled everywhere inside it, its middle
included. Labels stay upright even when the shape around them is turned. Where translucent
parts of a drawing overlap, the overlap looks darker, as with layered tracing paper.

A drawing at rest uses no animation frames, and when nothing is on screen the drawing layer is
hidden. If part of a drawing can't be painted, Kumochi clears the whole drawing rather than
leave only part of it on screen.
