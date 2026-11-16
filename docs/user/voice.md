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

## Showing your screen

While **Kumo can see what you see** is on in **Settings → Privacy** (the checkbox carries your
assistant's name), a hold to talk also sends your model one screenshot: the screen your pointer
is on, as it is at the moment you let go of the shortcut, shrunk to fit 1280 by 720 before it
goes. Everything visible on that screen is in the screenshot, including passwords,
notifications and windows running as administrator. Kumochi doesn't look for or hide anything
in it, so turn the setting off, or move what you don't want sent out of view, before you hold.
The setting starts on, unless you had turned off the earlier setting that shared one window.

Three things are left out: Kumochi's own dock and panels; windows of apps that ask Windows to
keep them out of screenshots, which show blank; and some videos, which show black.

If you move the pointer while you hold, Kumochi draws its path on your screen as a pink line,
and the model gets the same line on its screenshot. A pointer that stays put draws nothing. The
line is drawn on your main display only: if you draw there and let go with the pointer on
another display, the screenshot is of that other display, without the line.

Nothing is captured while you hold. A hold cancelled before you let go captures nothing, and
one cancelled after you let go, before the answer starts, sends nothing.

Between holds the dock says nothing about the screenshot: not that a hold will send one, not
that the setting is off, and not that something else will stop it. A hold is voice only,
without a word, while screen sharing isn't ready yet, in a task that uses its own model
(always, for now) and with a model that doesn't support screenshots.

A hold says nothing more about the screenshot while you hold. If the screenshot couldn't be
included, the dock's panel says why: the screen wasn't ready, the display changed, the
screenshot was too large, the model doesn't support screenshots, or you turned the setting off
during the hold. Turning the setting off while a screenshot is waiting to be sent withdraws it.

In chat history, a small screen icon under your message means a screenshot was sent with it.
Point at the icon to read that. When no screenshot was included, a line under the message says
why instead. The screenshot goes to the model service you connected, for the model you chose,
and nowhere else. Kumochi keeps no copy: it holds the screenshot in memory until the reply ends
and never saves it to a file. Every request Kumochi makes for that reply carries the
screenshot; later messages never do. The icon is never missing from a message whose screenshot
was sent. In two rare cases it can show on a message whose screenshot never left your computer:
when the request failed just as it started, before its upload began, or when Kumochi crashed.

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
