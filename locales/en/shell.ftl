# The shell's text. Product names come from the constants file, never from here.

status-connecting = Starting…
status-connected = Ready
status-reconnecting = Reconnecting…
status-failed = Can't reach the background process.
status-listening = Listening…

action-retry = Retry
action-stop = Stop
action-quit = Quit

dock-offline = Offline
dock-position = Dock position
position-top-left = Top left
position-top-center = Top center
position-top-right = Top right
position-middle-left = Middle left
position-center = Center
position-middle-right = Middle right
position-bottom-left = Bottom left
position-bottom-center = Bottom center
position-bottom-right = Bottom right

turn-busy = Still answering earlier messages. Try again in a moment.
turn-offline = Not sent: the background process isn't connected.
turn-too-long = That message is too long to send.

threads-title = Tasks
action-threads = Tasks…
action-new-thread = New task
threads-search = Search tasks
threads-empty = No tasks yet. Start a new task.
threads-search-empty = No matching tasks.
thread-gone = This task is no longer available.
thread-select = Select a task to read its messages.
thread-cut = This message was shortened.
thread-older = Older messages
threads-close = Close
thread-user = You
thread-assistant = { $assistant }
thread-step = Drawing step
threads-offline = Reconnect to load tasks.
threads-search-too-long = That search is too long to send.
threads-failed = Couldn't load tasks. Close this panel and open it again to retry.

thread-delete = Delete
thread-delete-title = Delete this task?
thread-delete-body = This removes the task and its drawings from this PC. It can't remove issues you already filed on GitHub, or what your model provider keeps under its own terms.
thread-delete-cancel = Cancel
thread-deleting = Deleting…
thread-delete-failed = Couldn't delete the task. It's still here; try again.
thread-deletes-full = Wait for the other deletes to finish.

action-back-to-kumo = Back to { $assistant }
action-switch-task = Switch to this task
action-rename-task = Rename
action-save-rename = Save
action-cancel-rename = Cancel
rename-empty = Enter a name.
rename-task-too-long = Use 80 characters or fewer.
rename-name-too-long = Use 24 characters or fewer.
rename-one-line = Use one line without control characters.
task-fallback = a task
task-focus = You're in: { $task }

settings-title = Settings
action-settings = Settings…
settings-close = Close
settings-general = General
settings-accounts = Accounts
settings-assistant-name = Assistant's name
settings-user-name = Your name
settings-user-name-help = Leave blank to unset.
settings-save-help = Press Enter or leave a field to save.
settings-max-frame-rate = Max frame rate
settings-frame-rate-automatic = Automatic
settings-frame-rate-now = Drawing at { $fps } frames a second on this display.
settings-saving = Saving…
settings-offline = Reconnect to change settings.
dock-error = Check settings
rename-busy = The assistant's name is still saving. Try again in a moment.
settings-unavailable = Couldn't send that change. Try again in a moment.
settings-not-saved = Settings not saved
dock-brain-error = Needs attention

error-settings-refused = Couldn't save that setting. Try again.
error-config-unreadable = Your settings file can't be read, so changes aren't saved. Repair or delete config.toml, then restart.
error-sign-in-timed-out = Sign-in took too long. Try again.
error-sign-in-failed = Couldn't finish signing in to ChatGPT. Try again.
error-sign-in-other-account = That's a different ChatGPT account from the one this computer signed in with. Sign in with the same account.
error-browser-unavailable = Couldn't open your browser to sign in.
error-account-offline = Can't reach ChatGPT right now. Try again.
error-credentials-unavailable = Windows Credential Manager couldn't read or save your ChatGPT sign-in. Try again.
error-revocation-backlog = Still trying to end earlier ChatGPT sessions, so a new sign-in can't start yet. Retry tries now, and the app tries again the next time it starts. A session that can't be ended is normally dropped from this list about 32 days after it was added, once it has expired; if this computer's clock was changed, restart the app with the right time first. You can also disconnect this app in your ChatGPT settings.
error-trust-store-unavailable = Couldn't load trusted certificates from Windows, so the app can't connect to ChatGPT safely.
error-plan-not-enabled = Plan use wasn't turned on. Enable it to use your ChatGPT plan.
error-account-misconfigured = ChatGPT didn't accept this app's sign-in. Trying again won't help.
