# Esc

Esc does one thing at a time, in this order:

1. If held speech is preparing, recording, transcribing or waiting for its reply to start, it
   cancels that input first.
   Otherwise, if Kumochi is working or speaking, it stops at once. What it already showed stays. That
   includes a message you just sent that it hasn't started answering: its answer never shows.
2. Otherwise, if a drawing is on screen, it clears. The answer's text stays. That includes
   drawings still coming back after a reconnect: they don't come back.
3. Otherwise, it cancels an active inline rename, Settings field edit or
   [onboarding](onboarding.md) name edit, without saving. Stopping work or clearing a drawing
   leaves the edit intact.
4. Otherwise, it closes the focused typing box, Tasks or Settings panel, or the onboarding
   window until you next start Kumochi. After cancelling an edit, another Esc closes the panel.

Switching to another window also hides the typing box and keeps your draft, without bringing
the previous app back. Esc and the box's close request still return focus to the previous app
only if the box still has it.

Kumochi takes Esc from other apps while held speech is active or while a drawing is on screen,
including steps shown in the open typing box. Spoken replies don't take Esc yet. Cancelling held
speech consumes that press without also dismissing the typing box.
If Windows won't give Kumochi the Esc key when it needs it, the dock's panel opens and stays open
with a Stop button that does the same thing. The typing panel shows Stop too while it's open.
