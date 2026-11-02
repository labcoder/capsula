# Esc

Esc does one thing at a time, in this order:

1. If Capsula is working or speaking, it stops at once. What it already showed stays. That
   includes a message you just sent that it hasn't started answering: its answer never shows.
2. Otherwise, if a drawing is on screen, it clears. The answer's text stays. That includes
   drawings still coming back after a reconnect: they don't come back.
3. Otherwise, it closes the typing box, if it's open and focused.

Switching to another window also hides the typing box and keeps your draft, without bringing
the previous app back. Esc and the box's close request still return focus to the previous app
only if the box still has it.

Capsula takes Esc from other apps only while it's speaking or showing a drawing. In this release
it doesn't speak, and its steps show only in the typing box, which hides when you switch away, so
Esc always reaches the app you're using unless the typing box has focus.
If Windows won't give Capsula the Esc key when it needs it, the dock's panel opens and stays open
with a Stop button that does the same thing. The typing panel shows Stop too while it's open.
