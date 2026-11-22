# Settings

Click the dock, then **Settings…**. The **General** tab lets you change the assistant's name
and your name. Press Enter or leave a field to save it. A field says **Saving…** until the
background process answers; a failed save restores its saved value and explains the problem
beside the field. Change the field to clear that error and try again.

Names use one line and 1–24 characters, with surrounding whitespace removed. Leave **Your
name** blank to unset it. Settings are stored locally; changing a name doesn't switch tasks
or discard your message draft. If the background process is disconnected, reconnect before
editing. **Accounts** manages your [model accounts](accounts.md) and
[GitHub connection](github.md).

In Settings, Ctrl+Tab goes to the next tab and Ctrl+Shift+Tab to the previous one, wrapping
at either end. Focus returns to the Settings frame; use Tab or Shift+Tab to reach its controls
and Space to choose a focused radio.

**Voice** shows speech input's reported installation/load status and its idle preference:
**Keep warm**, **After 15 minutes**, or **After an hour**. A choice says **Saving…** while it
waits for the background process to save it. If saving is refused, the saved choice returns
and the error stays beside the control until a successful retry. Offline controls are disabled;
reconnecting reads the saved preference instead of assuming an interrupted save succeeded.
**Hold to talk** is the checkbox above it.

**Speak answers aloud** controls whether an installed voice reads replies. It saves when
you toggle it, even if you leave Settings, and you can choose it before a voice is installed.
It doesn't download a voice or start recording.

**Privacy** has one checkbox, named for your assistant: **Kumo can see what you see**. While
it's on, a hold to talk also sends the model you're using one screenshot of the screen your
pointer is on, taken when you let go. The screenshot goes to the model service you connected and
nowhere else, and Kumochi keeps no copy once the answer is done. The setting starts on, unless
you had turned off the earlier setting that shared one window.
[Voice](voice.md#showing-your-screen) says what is in the screenshot and what is left out.

The preference can be chosen before speech input is installed. It doesn't install models,
start recording, reload an unloaded model or change spoken replies. **Installed · unloaded**
means the installed speech-input files remain available without a ready recognizer; it is not
a measurement of freed memory. See [voice](voice.md).

**Max frame rate** caps how fast the dock animates. **Automatic** picks 60 or 72, whichever
suits your display; a higher cap is smoother and uses more power. Kumochi draws at a rate your
display's refresh divides evenly, so motion stays steady, and Settings shows that rate. On a
75–119 Hz display that's about half the refresh; choose 120 or 144 there for smoother motion.

**Model** lists the models your connected accounts offer: ChatGPT's, Claude Code's and the
models Kumochi knows from a checked Anthropic API key. Picking one makes it the default. A saved
default that no account offers any more stays selected, shown by its id, while it remains your
saved default, and messages that would use it are refused as unavailable. ChatGPT and Claude Code
can replace a missing default of theirs on their own; an Anthropic API key's saved default stays
until you pick another. **Model** says **Saving…** until the
background process answers; a refused save returns the saved model and explains why at the top of
the page. Kumo and every task without a
model of its own use the new default from their next message; a reply already under way finishes
on its model, and a hold to talk in progress is cancelled.

**Dock position** offers the same nine anchors as the tray. Choosing one here moves the dock
without closing Settings; the panel follows its new anchor. The choice is saved for the next
launch. If a save is refused, the last saved position returns unless a newer choice is pending.
While disconnected, a pick still moves the dock and the latest choice is saved after reconnect.

The taskbar checkbox saves whether Kumochi should also have a Windows taskbar button. Its
button opens the status panel; **Close window** requests Quit. If Windows refuses the change,
General reports it without changing the saved preference. While enabled, Alt-Tab has a blank
preview.

**Version and build** identifies the running app. Its copy button copies that complete value.
**Quit** ends Kumochi through the same shutdown path as the tray, and remains available offline.

Settings stays open when you switch to another app. Esc first stops active work or clears a
drawing. Otherwise it cancels the field you're editing without saving; press Esc again to
close Settings. **Close** also closes it, saving an active edit as if
you had left the field. Closing gives keyboard focus back to the previous app when Settings
still has it.

Opening the typing panel or Tasks, or choosing a tray position, also closes Settings. If an edit can't
be sent or fails validation, the dock says **Settings not saved**; reopen Settings to correct
or cancel it. The draft stays there. While Settings is open, the status panel is hidden;
close Settings to reach Retry. Retry and Quit are also in the dock's native menu.

If the dock says **Check settings**, open Settings for the full explanation. A successful retry
of the field that failed clears that notice. If the settings file can't be read, repair or
delete `config.toml` in `%APPDATA%\Kumochi` and restart the app. Debug builds use
`%APPDATA%\Kumochi Dev`, separate from an installed copy.
