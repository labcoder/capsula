# Quitting and recovery

Kumochi runs as two processes: the window you see and a background process that does the work.

- **Quit** in the tray or dock panel closes both. Moving away from the dock closes only its panel.
- If the window crashes, the background process reopens it, waiting a little longer each time. After four crashes within a minute it stops trying; start Kumochi again yourself.
- If the background process stops, the window restarts it once. If it stops again within a minute, the window shows **Retry**: press it to start the background process again, or Quit.
- **Drawings.** Quit clears what Kumochi has drawn, and so does Esc. If the window or the
  background process crashes instead, the drawings that were on screen come back when it
  reconnects.
