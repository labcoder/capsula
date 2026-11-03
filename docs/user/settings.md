# Settings

Click the dock, then **Settings…**. The **General** tab lets you change the assistant's name
and your name. Press Enter or leave a field to save it. A field says **Saving…** until the
background process answers; a failed save restores its saved value and explains the problem
beside the field. Change the field to clear that error and try again.

Names use one line and 1–24 characters, with surrounding whitespace removed. Leave **Your
name** blank to unset it. Settings are stored locally; changing a name doesn't switch tasks
or discard your message draft. If the background process is disconnected, reconnect before
editing. **Accounts** is empty for now.

Settings stays open when you switch to another app. Esc first stops active work or clears a
drawing. Otherwise it cancels the field you're editing without saving; press Esc again to
close Settings. **Close** and **Settings…** again also close it, saving an active edit as if
you had left the field. Closing gives keyboard focus back to the previous app when Settings
still has it.

If the dock says **Check settings**, open Settings for the full explanation. A successful retry
of the field that failed clears that notice. If the settings file can't be read, repair or
delete `config.toml` in `%APPDATA%\Kumochi` and restart the app. Debug builds use
`%APPDATA%\Kumochi Dev`, separate from an installed copy.
