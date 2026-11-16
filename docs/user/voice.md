# Voice

Voice runs locally after its optional models are installed. The two parts download separately:

| Part | Downloads | Installed size |
|---|---|---|
| Speech input | Parakeet TDT 0.6B v3, int8 | 670 MB |
| Spoken replies | Phonon 31M (Q8), its tokenizer and configuration, and the Juniper voice | 65 MB |

Sizes are decimal megabytes. Files come from pinned revisions on Hugging Face and its
`hf.co` download servers, without an account or credentials. Installation and download
verification check each file's expected size and SHA-256. Start-up and idle reload do not
repeat SHA-256 verification. The native runtime ships with the app, not as a download.

Downloads need space for the remaining bytes plus 10%. Cancelled or interrupted downloads
keep their partial files for a later explicit attempt; they never resume just because the app
starts. **Settings → Voice → Install model** explicitly downloads, verifies and loads the default
Parakeet speech-input model. **Retry setup** retries a failed setup and may verify or fetch files
again, even when the status says installed. **Load model** reloads an installed, idle-unloaded
model without another download. There is no model picker.

**Settings → Voice** shows speech input's installed/loaded status and lets you keep its model
warm or unload it after 15 idle minutes or an idle hour. The policy takes effect only after
its setting is saved and speech input is ready. Active decoding finishes before idle unloading;
the status changes to unloaded only after the background owner reports release. This does not
certify how much native memory returned to Windows.

**Cancel voice setup** applies to pending setup for all voice features, not just speech input.
Some loading or finishing work cannot be stopped, including a model loading at startup.
Installed files and reusable partial downloads are kept. A request answer is not a cancellation
or release receipt: the current status can stay loading or later become ready. The tab shows
request-pending, send-failure and refusal feedback separately from the brain's model status.
Only that status establishes whether a model is ready or unloaded.

Tab and Shift+Tab reach the labeled controls; Enter or Space activates a focused model button.
These are explicit Settings actions; opening the tab or reconnecting does not start setup.
The tab does not remove models, open the microphone or control spoken replies.

## Hold to speak

**Hold to talk** starts on. Turn it off in **Settings → Voice** to disable this gesture and
its key-down model reloads; changing the idle-unload preference does not turn it back on.

Hold **left Ctrl + left Alt** and wait for **Listening** before speaking. Preparing or loading
is not recording permission. Releasing either key stops collection, transcribes locally and
sends nonblank words once through normal chat, using the captured target, model and connection.
An installed but idle-unloaded speech model can reload while the same hold is still down.
Missing models, an unavailable connection or an already-busy turn are refused before recording.
If normal turn capacity closes while speech is being transcribed, the hold ends as Busy
without saving the words or calling a model; it is not queued for a later retry.

The dock/status panel shows preparing, listening, stopping, transcribing and thinking.
The answer opens in the existing chat panel without taking focus from your app. Empty speech
creates no message. Recognized words become ordinary deletable chat history; raw audio is not
saved.

## Showing the window under your pointer

While **Share the window under your pointer** is on in **Settings → Voice**, a hold to talk also
sends your model one picture: the whole window your pointer is over when the hold starts,
including any part of it that other windows cover. Whatever that window holds is in the picture,
even where another window hides it from you, so point at another window, or turn the setting
off, when it holds something you don't want sent. The setting starts on.
Kumochi's own dock and panels are left out, and nothing else on your screen is captured.

Point at the dock to see what a hold will do. An eye beside the status means the named model
will get the picture. A struck-through eye means the hold is voice only, and its tooltip says
why: the model can't take pictures, screen sharing isn't ready yet, it is switched off, or the
task you're in uses its own model. A task with its own model is always voice only for now.

During the hold the same panel says which model will see the window. If the picture couldn't be
included it says why instead: nothing was under the pointer, the window can't be shared, it
changed, something was drawn over it so Kumochi couldn't tell which window you meant, or its
picture was too large. Turning the setting off while a picture is waiting to be sent withdraws
it.

In chat history, a small screen icon under your message means Kumochi took a picture to send
with it. Point at the icon to read that. When no picture was included, a line under the message
says why instead. The picture goes only to the model you chose, and Kumochi keeps no copy: it
holds the picture in memory until the reply ends and never writes it to disk. Every request
Kumochi makes for that reply carries the picture; later messages never do. The icon is recorded
with your message, before anything is sent, so it also shows on a message whose reply failed
before it started.

Not checked yet: displays at different scales, and more than one monitor.

Esc, a target/model/connection change, deletion or a new typed submission cancels pending
voice input. Another key cancels while the shortcut is held, not arbitrary typing after release.
Changing foreground apps alone does not cancel a global hold. Release
before Listening cancels rather than submitting startup audio. The source has a 60-second cap.
If microphone cleanup cannot be confirmed, input stays unavailable and another recording is
refused; a timeout never means the microphone was released. Follow the restart warning if it
stays active. A system-default-device change does not migrate an already-open microphone.
Losing the connection cancels that recording; it never retries your words. Once microphone
cleanup is confirmed, its stop receipt is retried across reconnects until acknowledged.
Another hold waits for that acknowledgement. Unconfirmed cleanup still keeps input unavailable.
