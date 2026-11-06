# Tasks

Click the dock, then **Tasks…**, to find an earlier conversation. Choose a task to read its
messages without changing where your next message goes. The list loads more tasks as you
scroll, and Older messages loads earlier history.
Page Up and Page Down scroll the focused messages area.

Search matches a phrase in stored message text. Clear the search to return to recent tasks.
A shortened message is labelled; reading the rest of that one message isn't supported yet.

Kumo is the assistant's conversation, pinned first in the recent list and never deletable. It
uses the assistant's current name. Input starts in Kumo each time you launch Kumochi.

To continue the task you're reading, choose **Switch to this task**. Double-tap left Ctrl +
left Alt to type; the typing panel names the task you're in. **Back to Kumo** returns input to
the assistant's conversation and shows its history. **New task** opens
the typing panel for a separate conversation, created only when you send its first message.
Closing the tasks panel doesn't erase anything or change where input goes. A switch affects
future messages, never a message already sent.

If you send again before a new task's first message is acknowledged, the second message stays
in the typing box with a busy notice. Retry once the task is ready to continue it, or choose
**New task** again to start a separate conversation.

Choose **Rename** on the task you're viewing to edit its name inline. Enter sends the edit; invalid
text shows an error beside the field. Task titles must be one line, 1–80 characters after
trimming, with no control characters. The displayed title changes only after the saved title
is read back. A new task starts with the first words of its first message as its title. After
one of its runs finishes, Kumochi may ask the task's model for a short title to replace those
words; if that fails, the first words stay, and a name you set yourself is never replaced. Renaming Kumo
changes the assistant's name instead: it has the same rules with a 1–24-character limit, and
the displayed name waits for confirmation from the background
process. A name update keeps your draft and doesn't switch tasks or move keyboard focus.

Esc stops work first, clears a drawing next, then cancels an active rename before it can
dismiss the focused panel. Stopping or clearing doesn't cancel the edit; another Esc does.
Switching away hides the panel. It reloads after reconnecting to the background process. If a
new task hadn't been confirmed as stored when that process restarted, input returns to Kumo.

To delete the open task, choose **Delete** and confirm. It stays in the list, marked
Deleting…, until it's removed. If it was your input target, input immediately returns to Kumo;
deleting a task you were only reading doesn't change the target. The copy on disk is
scrubbed shortly after. Kumo keeps a note that a task was deleted, but not its name. Kumochi can't remove issues you already filed on GitHub, or what your
model provider keeps under its own terms. If deleting fails, the task stays and you can try
again; input doesn't automatically switch back to it.

The confirmation starts with **Cancel** focused. Enter on Cancel or Escape closes it
without deleting. To confirm with the keyboard, Tab to **Delete**, then press Enter or
Space; clicking Delete also confirms. Holding Enter as the confirmation opens does not
delete the task.
