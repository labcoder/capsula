# The dock

The dock is a short line at the top center of your screen's work area. Move the pointer onto
it to open the peek: Kumo beside a small status panel, which shows Starting, Ready,
Reconnecting or Offline. Move away from both the dock and the peek to close it after a short
grace period. Opening the peek doesn't take focus from your current app. Pressing the line
does nothing for now.

The peek's buttons are **Chat…**, which opens the typing panel in the peek's place,
**Tasks…** and **Settings…**.

Only a thin band around the line takes the pointer: clicks beside it reach the app underneath.

The line itself has no label. Its status, Offline, **Check settings** and **Settings not
saved** show in the peek.

Use the tray menu to choose one of nine positions: top, middle or bottom, aligned left,
center or right. This choice currently lasts until the shell restarts. A later release moves
it into Settings → General and saves it. Moving the dock closes any open Settings panel,
saving an active edit as if you had left the field.

If the taskbar or display work area changes, choose the position again to realign the dock.
Display-scale changes currently require restarting the shell.

If the background process can't be reached, the peek and tray offer Retry. Quit in the tray
shuts down both processes. Closing a panel alone doesn't quit.

While you hold [the shortcut](shortcut.md), the line turns the accent colour to show it's
listening. A double tap opens a small
typing box under the dock; [Esc](escape.md) or switching to another window hides it and keeps
your draft. Enter sends what you typed and the answer appears in the box. A message that can't
be sent stays there, with the reason. Switching away leaves your chosen window in front.
**[Tasks…](tasks.md)** opens stored conversations without changing where you type next.
**[Settings…](settings.md)** opens the name fields. Opening the typing box or Tasks closes
Settings, saving an active edit as if you had left the field.

If the status panel says **Check settings**, open **Settings…** for the full explanation and retry the
field that failed. A successful retry clears that notice.

If the status panel says **Settings not saved**, an edit couldn't be sent or failed validation.
Open **Settings…** to correct or cancel the edit; the draft stays there until you do.
