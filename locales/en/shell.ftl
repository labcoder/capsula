# The shell's text. Product names come from the constants file, never from here.

# Shell

status-connecting = Starting…
status-connected = Ready
status-reconnecting = Reconnecting…
status-failed = Can't reach the background process.
action-retry = Retry
action-stop = Stop
action-quit = Quit
action-chat = Chat…

# Dock

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
dock-error = Check settings
dock-brain-error = Needs attention
carry-perch-on = Drop on { $app }
carry-perch-untitled = Drop on this app

# Turns

recovery-sign-in = Sign in
dismiss-notice = Dismiss
chat-live-cut = Earlier streamed text isn't shown here.
chat-empty = Start a conversation.
chat-loading = Loading messages…
chat-history-failed = Couldn't load messages. Try again.
turn-busy = Still answering earlier messages. Try again in a moment.
turn-offline = Not sent: the background process isn't connected.
turn-too-long = That message is too long to send.

# Tasks

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
thread-no-visual = Drawing unavailable in history
thread-issue-title = Issue: { $title }

# Saved issue cards are read-only; these states never authorize a send.
issue-loading = Loading saved issue
issue-unavailable = Saved issue unavailable
issue-details-loading = Loading saved details…
issue-details-unavailable = The saved details aren't available in this view.
issue-read-only = This saved issue is read-only here. Filing and changes aren't available.
issue-consent-title = File this issue on GitHub?
issue-consent-open = File issue…
issue-consent-consequence = Confirming asks to file this exact saved issue using the bound GitHub account and repository. A queued decision does not mean an issue was filed.
issue-consent-cancel = Cancel
issue-consent-confirm = File issue
issue-duplicate = More than one issue may exist on GitHub.
issue-repository-unbound = Not selected
issue-account-unbound = Not bound
issue-details =
    Repository: { $repo }
    Bound account: { $account }

    { $body }
issue-status-draft = Draft
issue-status-sending = Preparing or sending
issue-status-filed =
    Filed #{ $number }
    { $confirmation ->
        [user] Confirmed by you.
       *[remote] Confirmed by GitHub.
    }
    { $url }
issue-status-failed =
    Not filed. No issue was created by this attempt.
    { $reason ->
        [connection] GitHub wasn't connected.
        [permission] The account didn't have permission.
        [repository] The repository wasn't found.
        [content] GitHub refused the issue's content.
        [rate] GitHub required a wait before another attempt.
        [network] The connection failed before sending.
        [preflight] The check before sending couldn't complete safely.
        [interrupted] The attempt was interrupted before sending.
       *[rejected] GitHub refused the attempt.
    }
issue-status-unknown =
    Outcome unknown. An issue may already exist on GitHub.
    { $lookup ->
        [none] No matching issue was visible; it may still exist.
        [several] More than one issue matched.
        [failed] The lookup failed or was incomplete.
        [unplaced] Candidate #{ $number } is not confirmed as this issue.
            { $url }
       *[pending] No completed lookup is available.
    }
issue-status-discarded =
    { $risk ->
        [possible] Dismissed. An issue may already exist on GitHub.
       *[none] Dismissed
    }
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
thread-delete-kumo = { $assistant } keeps a note that a task was deleted. Anything already in { $assistant }'s conversation stays, including names it used.
thread-deleted = You deleted a task.
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

# Settings

settings-title = Settings
action-settings = Settings…
settings-close = Close
settings-general = General
settings-accounts = Accounts
github-waiting = GitHub connections are unavailable. Waiting for the brain.
github-signed-out = Signed out
github-removing = Removing saved sign-in…
github-removal-failed = Signed out. Couldn't remove the saved sign-in. Retry removal before connecting again.
github-not-durable = Signed out. Couldn't save the removal. Retry before restarting, or the saved sign-in may return.
github-requesting = Connecting to GitHub…
github-awaiting = Enter this code on GitHub
github-connected = Connected
github-not-installed = Install the GitHub App on your repositories, then sign out and connect again.
github-needs-sign-in = Sign in again to use this GitHub account.
github-not-verified = Saved sign-in, not verified
github-check-again = Check again
github-connect = Connect
github-sign-in-again = Sign in again
github-refresh = Refresh
github-sign-out = Sign out
github-timeout = GitHub sign-in timed out. Connect again when you're ready.
github-failure = Couldn't complete the GitHub operation. Try again.
github-offline = Couldn't reach GitHub. Check your connection and try again.
github-credentials = Couldn't access the saved GitHub sign-in. Try again.
github-refused = GitHub refused that operation. Check the connection and try again.
github-unavailable = GitHub connection controls aren't available in this build.
github-provider = GitHub
github-configuration = Connecting is unavailable until this build has a registered GitHub App client ID. No token or client secret can be entered here.
github-saving-default = Saving the default repository…
github-default-retained = Couldn't save the default repository. The previous default is unchanged. Try again.
github-default-repository = Default repository
github-choose-repository = Choose a repository
github-empty-repositories = No repositories are available. Refresh to check the GitHub App's repository access.
github-truncated-repositories = This list is incomplete. Refresh before choosing a default repository.
github-verification = Open GitHub verification
github-code-expired = This code is no longer available. Cancel and connect again for a new code.
github-canceling = Canceling GitHub sign-in…
github-refreshing = Refreshing repositories…
settings-voice = Voice
voice-speech-input = Speech input
voice-idle-policy = Unload speech input when idle
voice-idle-kept = Keep warm
voice-idle-15m = After 15 minutes
voice-idle-1h = After an hour
voice-idle-help = This preference applies when speech input is installed. It doesn't start recording or change spoken replies.
voice-status-unknown = Speech input status unavailable
voice-not-installed = Not installed
voice-verifying = Verifying speech input…
voice-downloading = Downloading speech input…
voice-loading = Loading or stopping speech input…
voice-ready = Installed · ready
voice-unloaded = Installed · unloaded
hold-preparing = Preparing speech input…
hold-to-talk = Hold to talk
hold-to-talk-help = Hold left Ctrl+Alt to speak, then release to send. Requires an installed speech input model.
hold-disabled = Hold to talk is off. Turn it on in Settings → Voice.
hold-listening = Listening…
hold-stopping = Stopping microphone…
hold-transcribing = Transcribing…
hold-thinking = Thinking…
hold-empty = No speech detected. Try again.
hold-cancelled = Speech input cancelled.
hold-unavailable = Speech input unavailable. Restart the app if the microphone stays active.
hold-microphone-unavailable = Microphone unavailable. Check microphone access and your input device, then try again.
hold-failed = Speech input failed. Try again.
voice-restart-required = Speech input unavailable. Restart the app.
voice-installed-failed = Installed · speech input unavailable
voice-not-installed-failed = Not installed · speech input unavailable
voice-default-model = Parakeet TDT 0.6B v3 · int8
voice-install-model = Install model
voice-retry-model = Retry setup
voice-load-model = Load model
voice-cancel-setup = Cancel voice setup
voice-cancel-help = Applies to pending setup for all voice features. Some loading or finishing work cannot be stopped. Installed files are kept.
voice-cancel-accessible = { voice-cancel-setup }. { voice-cancel-help }
voice-request-pending = Waiting for the request answer…
settings-assistant-name = Assistant's name
settings-user-name = Your name
settings-user-name-help = Leave blank to unset.
settings-save-help = Press Enter or leave a field to save.
settings-version = Version and build
settings-copy-build = Copy version and build
settings-position-unavailable = Couldn't move the dock.
settings-taskbar = Also show { $app } in the taskbar
taskbar-add-failed = Couldn't add to the taskbar.
taskbar-remove-failed = Couldn't remove the taskbar button.
settings-max-frame-rate = Max frame rate
settings-frame-rate-automatic = Automatic
settings-frame-rate-now = Drawing at { $fps } frames a second on this display.
settings-saving = Saving…
settings-offline = Reconnect to change settings.
rename-busy = The assistant's name is still saving. Try again in a moment.
settings-unavailable = Couldn't send that change. Try again in a moment.
settings-not-saved = Settings not saved
error-settings-refused = Couldn't save that setting. Try again.
error-config-unreadable = Your settings file can't be read, so changes aren't saved. Repair or delete config.toml, then restart.

# ChatGPT accounts

accounts-provider = ChatGPT
accounts-waiting = Waiting for account status…
accounts-offline = Reconnect to manage your ChatGPT account.
accounts-use-plan = Use your ChatGPT plan
accounts-continue = Continue with ChatGPT
accounts-signed-out = Signed out on this computer
accounts-signing-in = Finish signing in with your browser
accounts-replace-help = Opening again replaces this sign-in attempt.
accounts-cancel = Cancel
accounts-open-again = Open again
accounts-busy = Account in use in another Windows session
accounts-busy-help = Close the other session's copy of the app, then try again.
accounts-try-again = Try again
accounts-identity = ChatGPT account
accounts-using-plan = Using ChatGPT plan
accounts-plan-off = Plan use is off
accounts-enable = Enable
accounts-unavailable = ChatGPT plan use isn't available for this account or workspace.
accounts-attention = ChatGPT needs attention
accounts-retry-help = Something temporarily failed. Retry to try the account again.
accounts-sign-in-again = Sign in again
accounts-session-ended = Sign in to ChatGPT again
accounts-manage-usage = Manage usage
accounts-sign-out = Sign out…
accounts-sign-out-title = Sign out of ChatGPT?
accounts-sign-out-body = This stops using your account now, tries to end its session at OpenAI, and removes its saved sign-in from Windows Credential Manager. It doesn't delete your tasks.
accounts-sign-out-registration = This also removes the app registration. Signing in again registers the app anew.
accounts-sign-out-confirm = Sign out
accounts-dialog-cancel = Cancel
accounts-signing-out = Signing out…
accounts-stopped = The app has stopped using this account.
accounts-cleanup-refused = Your saved sign-in may still be on this computer.
accounts-not-durable = Until Retry succeeds, restarting the app will sign you in again.
accounts-revocation-pending = OpenAI hasn't confirmed that the session ended yet. You can also disconnect this app in ChatGPT settings.
accounts-revocation-unconfirmed = OpenAI didn't confirm that the session ended. You can disconnect this app in ChatGPT settings.
accounts-send-failed = Couldn't send that account action. Try again.
accounts-local = On this computer
accounts-remote = At OpenAI
accounts-sign-out-needs-retry = Sign-out needs attention
accounts-cleanup-refused-help = The app stopped using this account, but Windows Credential Manager refused part of the sign-out. Retry tries again now; the app also retries at each start.
accounts-not-durable-help = The app stopped using this account for now, but couldn't record the sign-out. Choose Retry to finish.
accounts-backlog-lead = Still ending earlier ChatGPT sessions. A new sign-in can't start yet.
accounts-backlog-retry = Retry tries now. The app also retries the next time it starts.
accounts-backlog-expiry = An expired session is normally removed from this list about 32 days after it was added. If this computer's clock changed, restart the app with the right time first.
accounts-backlog-recovery = You can also disconnect this app in your ChatGPT settings.
error-sign-in-timed-out = Sign-in took too long. Try again.
error-sign-in-failed = Couldn't finish signing in to ChatGPT. Try again.
error-sign-in-other-account = That's a different ChatGPT account from the one this computer signed in with. Sign in with the same account.
error-browser-unavailable = Couldn't open your browser to sign in.
error-account-offline = Can't reach ChatGPT right now. Try again.
error-credentials-unavailable = Windows Credential Manager couldn't read or save your ChatGPT sign-in. Try again.
error-trust-store-unavailable = Couldn't load trusted certificates from Windows, so the app can't connect to ChatGPT safely.
error-plan-not-enabled = Plan use wasn't turned on. Enable it to use your ChatGPT plan.
error-account-misconfigured = ChatGPT didn't accept this app's sign-in. Trying again won't help.
error-sign-out-not-durable = Couldn't record the sign-out in the system's credential store. The account stays unused, but if the app restarts before Retry works, it will be signed in again.
error-session-ended = Your ChatGPT session ended. Sign in again to keep using your plan.
error-renewal-unconfirmed = ChatGPT didn't confirm your sign-in renewal. Sign in again to keep going.
error-plan-unavailable = This ChatGPT account can't use its plan in this app.
error-usage-limit = You've reached a ChatGPT usage limit for this app. Check your usage in ChatGPT settings.
error-account-refused = ChatGPT didn't accept this account for that request. Signing in again may help.
error-request-refused = That request couldn't be accepted. Trying again won't help.
error-response-unreadable = The answer couldn't be read. Try again.
error-sign-in-needed = Sign in to ChatGPT to get an answer.
error-model-unavailable = This thread's model isn't available on your account. Pick another in Settings.

# Anthropic

error-anthropic-key-rejected = Anthropic didn't accept this API key. Check it and paste it again.
error-anthropic-key-format = That doesn't look like an Anthropic API key.
error-anthropic-key-subscription-token = This is a Claude subscription token. Paste an Anthropic API key instead.
error-anthropic-key-admin-key = Admin keys can't send messages. Paste an Anthropic API key instead.
error-anthropic-key-workspace-required = This key needs a workspace header. Paste a key scoped to a workspace instead.
error-anthropic-unreachable = Anthropic couldn't be reached. Check your connection and try again.
error-anthropic-rate-limited = Anthropic is limiting requests for this key. Try again in a moment.
error-anthropic-credit = Your Anthropic account is out of credit.
error-anthropic-answer-limit = The answer reached the limit Kumochi sets for one reply. Try asking for something shorter.

# Claude Code

error-claude-code-not-found = Claude Code isn't installed, or isn't where Kumochi looks for it.
error-claude-code-unsupported = Kumochi can't use this Claude Code install yet.
error-claude-code-signed-out = Claude Code isn't signed in. Connect it in Accounts.
error-claude-code-audit-failed = Claude Code didn't pass Kumochi's safety check, so Kumochi stopped it. Your message may already have reached Claude, but no tool ran.
error-claude-code-policy = A policy or safety check keeps Kumochi from using Claude Code.
error-claude-code-input-unsupported = Claude Code can't take a screenshot here. Switch connection to send one.
error-claude-code-budget = Kumochi's Claude Code allowance for now is used up.
error-claude-code-cleanup-pending = Disconnecting Claude Code isn't finished yet. Kumochi keeps retrying.
claude-provider = Claude Code
claude-review = Connect…
claude-connect = Connect Claude Code
claude-disconnect = Disconnect Claude Code
claude-find = Look again
claude-policy-notice = Your Team or Enterprise organization’s server-managed Claude Code settings apply to Kumochi’s questions too. Kumochi can’t see those settings. Personal plans have no organization-managed settings.
claude-before = A Claude Code window will open. If it asks how to sign in, choose “Claude account with subscription,” not the Anthropic Console API option.
claude-during = Continue in the Claude Code window. If it asks how to sign in, choose “Claude account with subscription,” not the Anthropic Console API option.
claude-fallback = If your browser doesn’t open, open the link shown in that window yourself, sign in and authorize access. Copy the code from the page, paste it into the Claude Code window, and press Enter.
claude-deadline = Sign-in stops after five minutes. You can cancel it or try again afterwards.
claude-analytics = Claude Code reports its own analytics as on.
claude-disconnect-body = This signs Claude Code out of Kumochi and removes Kumochi’s Claude Code folder. Your other apps’ sign-ins are not changed.
claude-status-waiting = Waiting for Claude Code status…
claude-status-finding = Looking for Claude Code…
claude-status-not-found = Claude Code was not found.
claude-status-off = Claude Code is turned off in config.
claude-status-out = Claude Code { $version } is signed out.
claude-status-ready = Claude Code { $version } is ready.
claude-status-signing = Signing in…
claude-status-cleanup = Cleanup is pending. Choose Look again to retry removing Kumochi’s Claude Code folder.
claude-status-in-use = Another Kumochi session is using Claude Code. Close that session, then look again.
claude-status-unavailable = Claude Code is unavailable. Restart Kumochi before trying again.
claude-status-unsupported =
    { $reason ->
        [shim] This Claude Code installation is a wrapper or shim, not the native executable.
        [store] This build cannot use Claude Code’s sign-in store.
       *[version] Claude Code { $version } has not been reviewed for this build.
    }
claude-status-audit =
    Claude Code’s launch audit failed: { $reason ->
        [version] version mismatch
        [commands] unexpected slash commands
        [agents] unexpected agents
        [mcp] unexpected MCP server
        [tools] unexpected tools
        [plugins] unexpected plugins
        [skills] unexpected skills
        [key] unexpected API key source
        [token] unexpected sign-in source
        [deadline] audit timed out
       *[malformed] unreadable launch facts
    }.
claude-status-policy =
    { $reason ->
        [managed] A managed policy is present or could not be ruled out. Claude Code cannot be used.
       *[turns] This Claude Code version’s turn limit is not verified.
    }

# Voice

error-voice-no-space = There isn't enough free disk space to install voice.
error-voice-download = Voice couldn't be downloaded. Check your connection and try again.
error-voice-corrupt = A voice file didn't match what Kumochi expected, so it wasn't used.
error-voice-storage = Kumochi couldn't read, write or delete its voice files.
error-voice-load = Voice couldn't start.
error-voice-model-path = The speech model folder goes through a link or network location, or has unsafe permissions. Use an ordinary local folder.
error-voice-unavailable = Voice can't be installed or removed right now.
