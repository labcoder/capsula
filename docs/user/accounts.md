# ChatGPT accounts

Open the dock's **Settings…**, then **Accounts**. Choose **Continue with ChatGPT** to sign
in using your browser. Finish there, then return to the app. **Cancel** stops waiting for the
browser; once you've finished there, the sign-in completes even if you press Cancel.
**Open again** replaces the waiting sign-in attempt. Closing Settings doesn't cancel it.
Onboarding's account step offers the same sign-in buttons; closing that window cancels a
sign-in it started that hasn't finished.

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

Turning it off doesn't start a sign-out or deletion. If a previous Disconnect is still waiting
to delete Kumochi's Claude Code folder, Kumochi still tries to finish that cleanup at startup.

## Claude Code

Settings → Accounts shows Claude Code beside ChatGPT. Looking for Claude Code means discovery
is still running; **Connect…** becomes available only after an installation is found and signed
out. **Look again** retries discovery. A version, policy or launch-audit problem is shown with
its reason.

Choose **Connect…** to review the guidance without starting sign-in. Kumochi shows this notice:
your Team or Enterprise organization's server-managed Claude Code settings apply to Kumochi's
questions too. Kumochi can't see those settings. Personal plans have no organization-managed
settings.

Then choose **Connect Claude Code** to open Claude Code's own window. If it asks how to sign in,
choose **Claude account with subscription**, not the Anthropic Console API option. If your
browser doesn't open, open the link shown in that window yourself, sign in and authorize access.
Copy the code from the page,
paste it into the Claude Code window, and press Enter. This paste-code fallback is the expected
path for now (OSC-244); Kumochi never reads or relays the link or code.

Sign-in stops after five minutes. **Cancel** stops it sooner; if a sign-in was already saved,
Cancel does not sign it out. **Disconnect Claude Code** asks for confirmation, signs Claude Code
out of Kumochi, and removes Kumochi's Claude Code folder. Your other apps' sign-ins are not
changed. When Claude Code reports its analytics as on, the row says so.

**Cleanup pending** means removing the folder has not finished. Choose **Look again** to retry
that cleanup without another confirmation; Kumochi also retries at startup.

**In use** can mean another Kumochi session is using Claude Code. Close that session, then choose
**Look again**. **Unavailable** means Kumochi cannot start Claude Code work; restart Kumochi
before trying again.

## When Claude Code fails Kumochi's check

Before Kumochi sends Claude Code anything of yours, it checks how that Claude Code is set up. If
the check fails, Kumochi stops using that version of Claude Code and remembers it, even after
Disconnect, signing in again or a restart. Installing another version doesn't clear it: Kumochi
keeps refusing that version even if you go back to it later.
