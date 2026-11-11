# Saved GitHub issues

Ask Kumochi to draft an issue to save it locally for review. Drafting does not
file anything on GitHub. Filing requires your own GitHub connection and the
explicit confirmation below; no screenshot or attachment is sent.

When a saved issue appears in a task or chat history, its card shows the saved
title, body, repository and bound account. The bound account is part of the
saved record, not proof that the account is still signed in.

- **Draft** means the issue is saved locally, not filed.
- **Not filed** means this attempt created no issue. It does not mean nothing was sent.
- **Outcome unknown** means an issue may already exist. No visible match does not
  prove otherwise, and a candidate is not automatically confirmed.
- **Filed** shows the issue number and who confirmed it. A separate warning says
  when more than one issue may exist; no warning is not an exactly-once guarantee.

Details show as loading while a read is pending, or unavailable when they cannot
be read in this view. Unavailable does not prove the issue was deleted.

For a draft or definite failure with a usable bound account, repository and
approval, **File issue…** opens a literal preview of the saved destination,
title and body. **Cancel** is focused first. **File issue** sends only the
exact approval you reviewed, while that same history window is active and
visible. GitHub will interpret mentions, links and Markdown in the saved text.
A queued action is not proof that GitHub created an issue. A definite
failure requires a new confirmation before another filing attempt.

**Dismiss** removes a draft, definite failure or uncertain action from further
filing without deleting any remote issue. If a saved destination is unusable,
dismiss the draft and ask for a replacement after connecting the intended
account and repository. Changing the default repository never retargets it.

Only **Outcome unknown** offers **Check again**. It looks for the existing
issue with GET requests and never resends it. Busy and disconnected responses
remain visible and allow another check; there is no “file anyway” or candidate
confirmation action. Dismissal preserves the warning that an issue may exist.

Pending actions disable duplicate controls across both histories. Queue failures
stay visible and retryable. A reply from an earlier attempt or connection cannot
replace the current attempt's feedback. Cards do not edit saved content, and
their saved URLs do not open a browser. Scroll or use PageUp and PageDown in the
history as usual.
