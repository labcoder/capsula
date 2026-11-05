# ChatGPT accounts

Open the dock's **Settings…**, then **Accounts**. Choose **Continue with ChatGPT** to sign
in using your browser. Finish there, then return to the app. **Cancel** stops waiting;
**Open again** replaces the waiting sign-in attempt. Closing Settings doesn't cancel it.

The connected account shows its email, when provided, and **Using ChatGPT plan** when plan
use is enabled. If plan use is off, **Enable** asks for permission in your browser. If the
account or workspace can't use its plan here, the panel says so. **Manage usage** opens
ChatGPT's public usage page in your browser.

**Retry** tries a temporary failure again. **Sign in again** means your session ended and
needs a new browser sign-in. If another Windows session owns the account, close its copy of
the app, then use **Try again**. If the background process disconnects, account controls
wait until it reconnects and reports the current state.

If earlier sessions still need to end, new sign-in stays unavailable. Choose **Retry**;
the panel separates the immediate recovery from the expiry and ChatGPT settings help.

## Sign out

**Sign out…** asks first. **Cancel** or Escape leaves the account alone. Confirming stops
account use, tries to end its session at OpenAI, and removes its saved sign-in from Windows
Credential Manager. Tasks aren't deleted. If plan use wasn't available, sign-out also removes
the app registration; signing in again registers the app anew. Switching to a different
ChatGPT account isn't supported yet: an ordinary sign-out keeps the registration.

If Credential Manager refuses part of sign-out, the account stays unused and a warning
remains. **Retry** tries now; the app also tries at each start. If the sign-out couldn't be
recorded at all, the warning explains that restarting before Retry works signs the account
in again. Don't treat that warning as a completed sign-out.

Local cleanup and OpenAI's confirmation are separate. If OpenAI hasn't confirmed yet, the
panel says so. After local sign-out finishes, it may still say OpenAI didn't confirm that
the session ended. You can also disconnect the app in ChatGPT settings.
The panel labels the local risk **On this computer** and an unconfirmed remote outcome
**At OpenAI**. A cleanup waiting for Retry says **Sign-out needs attention**.

Your sign-in is held in the system credential store, never in the settings file. Development
builds use their own account storage, separate from an installed copy.

## Turning off Claude Code

Kumochi looks for an installed Claude Code when it starts. To stop that, add this to
`config.toml` in `%APPDATA%\Kumochi` and restart Kumochi:

```toml
[claude_code]
enabled = false
```

Turning it off doesn't sign Claude Code out or delete anything.
