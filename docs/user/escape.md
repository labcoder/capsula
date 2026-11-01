# Esc

Esc does one thing at a time, in this order:

1. If Capsula is working or speaking, it stops.
2. Otherwise, if a drawing is on screen, it clears.
3. Otherwise, it closes the typing box, if it's open and focused.

Capsula takes Esc from other apps only while it's speaking or showing a drawing. Neither exists
in this release, so Esc always reaches the app you're using unless the typing box has focus.
If Windows won't give Capsula the Esc key when it needs it, the dock's panel opens and stays open
with a Stop button that does the same thing. The typing panel shows Stop too while it's open.
