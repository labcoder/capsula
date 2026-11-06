# Typing to Kumochi

Double-tap the [shortcut](shortcut.md) to open the typing panel. Enter sends your message to
Kumo, or to the task you explicitly [switched to](tasks.md). Browsing Tasks doesn't change
where your next message goes. Esc stops work, clears the drawing, then closes the panel;
closing and reopening keeps your draft.

The panel shows the focused conversation below the input, including live answers. **Tasks…**
opens the separate browser without changing where your next message goes. A task header
offers **Back to Kumo**. Tab or click into the message region to use Page Up and Page Down;
scrolling to its top or choosing **Older messages** loads earlier messages. A read failure keeps
the messages and offers Retry.
Long streamed answers show a trailing preview with a cut notice. After a successful turn,
a fresh history read replaces it with the separate stored answer sections. A failed or
stopped turn keeps its preview until another answer replaces it or the connection changes.

If a named run fails before answering because its connection needs sign-in again, Kumochi puts the submitted
message back into the empty input. It never overwrites a newer draft, switches you back to
another task, or sends the message again automatically. Stopping, changing tasks or losing
the background connection discards that recovery copy. A general account warning alone does
not restore a message.

For ChatGPT, open **Settings… → Accounts** to sign in again. Its sign-in error can offer this
action even while the old account details are still displayed. Controls wait for the current
account operation to finish; sign-in stays blocked while earlier sessions still need to end.
Submitting the restored message is a new turn, so the earlier stored user turn can remain
in the task history.

Accounts also shows Claude Code's connection controls; it doesn't replace an Anthropic
API key. A later-round failure after answering starts doesn't automatically put
the original message back into the composer.

An error can offer recovery controls beside the input. **Retry** on a failed message sends
that message as a new turn in its original task, including a task created by **New task**.
It requires an empty draft and the same selected task; even whitespace keeps the draft
untouched. The attempt is single-use, including when it can't be sent. Changing tasks,
stopping, clearing, dismissing the notice or losing the connection discards its saved copy.
Retry can repeat earlier work, including tool effects; it doesn't resume the failed turn.

Account and settings errors use their own recovery instead of resending a message.
**Settings…** opens the appropriate tab without closing an already-open settings window;
**Manage usage** opens ChatGPT's public usage page. **Dismiss** hides the notice. A matching
renewal warning appears once per incident, even if its account and failed-message reports
arrive separately.
