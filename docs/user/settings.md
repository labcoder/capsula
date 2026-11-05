# Settings

Click the dock, then **Settings…**. The **General** tab lets you change the assistant's name
and your name. Press Enter or leave a field to save it. A field says **Saving…** until the
background process answers; a failed save restores its saved value and explains the problem
beside the field. Change the field to clear that error and try again.

Names use one line and 1–24 characters, with surrounding whitespace removed. Leave **Your
name** blank to unset it. Settings are stored locally; changing a name doesn't switch tasks
or discard your message draft. If the background process is disconnected, reconnect before
editing. **Accounts** manages your [ChatGPT sign-in](accounts.md).

**Max frame rate** caps how fast the dock animates. **Automatic** picks 60 or 72, whichever
suits your display; a higher cap is smoother and uses more power. Kumochi draws at a rate your
display's refresh divides evenly, so motion stays steady, and Settings shows that rate. On a
75–119 Hz display that's about half the refresh; choose 120 or 144 there for smoother motion.

**Dock position** offers the same nine anchors as the tray. Choosing one here moves the dock
without closing Settings; the panel follows its new anchor. The choice is saved for the next
launch. If a save is refused, the last saved position returns unless a newer choice is pending.
While disconnected, a pick still moves the dock and the latest choice is saved after reconnect.

The taskbar checkbox saves whether Kumochi should also have a Windows taskbar button. Its
button opens the status panel; **Close window** requests Quit. If Windows refuses the change,
General reports it without changing the saved preference. While enabled, Alt-Tab has a blank
preview. The installed-build checks for these Windows interactions are still pending.

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
