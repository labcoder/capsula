# Esc

Desktop annotations are taken at the first press. If their answer is still running, that
press also stops it. A press that cancels held input belongs to that hold and does not clear
an unrelated ordinary drawing. Annotation expiry after 60 seconds does not apply to contained
drawings.

Otherwise, Esc does one thing at a time, in this order:

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

Kumochi takes Esc from other apps while held speech is active, while a drawing is on screen,
including steps shown in the open typing box, or while a reply is playing out loud. While it
has Esc, the app in front doesn't get the press at all. An Esc taken from another app while a
reply plays stops that reply's speech and nothing else: annotations, drawings and panels stay.
After a reply's audio ends, Kumochi keeps Esc for up to about half a second. If one of Kumochi's own panels has focus while it
speaks, that panel keeps Esc, and its first press stops the speech. A typing box that's open
without focus doesn't. Cancelling held speech consumes that press without also dismissing the typing box.
If Windows won't give Kumochi the Esc key when it needs it, the dock's panel opens and stays open
with a Stop button that does the same thing. The typing panel shows Stop too while it's open.
