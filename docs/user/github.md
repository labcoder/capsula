# GitHub connection

Open **Settings → Accounts** for GitHub. Debug builds can connect to the
**Kumochi Dev** GitHub App. Release builds do not yet have a registered app:
the section explains that GitHub is not configured and Connect is unavailable.
Kumochi cannot use a browser's saved sign-in, gh, another app's credentials or a
pasted token instead.

In a debug build, **Connect** requests a short-lived code. Enter that code
on the fixed GitHub device page opened by **Open GitHub verification** and authorize Kumochi's
own App in your browser. **Cancel** ends the attempt; an expired or canceled code
cannot be revived. The connection shows the verified account and repositories
available to both you and the App. If none appear, check the App's installation
and repository access, then refresh the list.

A failed or over-limit refresh does not offer a partial list for choosing a
default. Check the App's repository access and try refreshing again.

If **Cancel** stops a check of an already saved account, the sign-in stays saved
but is shown as not verified. It cannot authorize an action. **Check again**
rechecks that account without starting another device flow; **Sign out** removes
the saved sign-in. A canceled check cannot later restore the connection.

Choose a fetched repository as the default for future drafts. It appears as the
saved default only after the local settings write commits. A failed save keeps
the previous default. Another GitHub account cannot inherit it, and changing the
default does not retarget an issue already bound to a repository.

The sign-in uses expiring access and refresh credentials stored only in the
system credential store. Kumochi renews them before access expires. If renewal
cannot be completed safely, or GitHub refuses the account, **Sign in again**
starts a new device flow; an uncertain refresh is never sent twice.

**Sign out** stops local account use before removing the saved sign-in. If
credential cleanup fails, the section explains the problem and offers a retry.
Closing Settings or reconnecting the background process is not sign-out.

Connecting does not create or file an issue. Local drafting and filing are not
available in this build; [saved issue cards](issues.md) remain read-only.
