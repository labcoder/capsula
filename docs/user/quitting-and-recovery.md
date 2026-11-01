# Quitting and recovery

Capsula runs as two processes: the window you see and a background process that does the work.

- **Quit** closes both. Closing the status window is the same as Quit.
- If the window crashes, the background process reopens it, waiting a little longer each time. After four crashes within a minute it stops trying; start Capsula again yourself.
- If the background process stops, the window restarts it once. If it stops again within a minute, the window shows **Retry**: press it to start the background process again, or Quit.
