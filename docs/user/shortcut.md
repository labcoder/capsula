# The shortcut

Kumochi listens for **left Ctrl + left Alt**, from any app, without taking those keys from it.

- **Press and hold** both keys. After about 300 ms, Kumochi prepares speech input. Wait for
  **Listening** and the dock's accent line before speaking, then release either key to send.
  Speech is transcribed locally and sent once to the model and connection chosen for your
  [input target](tasks.md), like an ordinary chat message. This hold does not take a screenshot.
  [Speech input](voice.md) must already be installed; the shortcut never downloads a model.
  If Kumochi only notices the hold as you release it, nothing was recorded and it says
  **Speech input failed. Try again.** Once the reply starts, Esc no longer cancels speech input.
- **Double tap** both keys, two short presses with their releases less than 300 ms apart. A small
  typing box opens under the dock and takes focus. [Esc](escape.md) hides it after stopping
  work and clearing any drawing. Whatever you typed is kept for next time, and the app you
  were using gets focus back. Input goes to Kumo unless you explicitly [switch tasks](tasks.md).

Pressing any other key while holding the shortcut cancels it, and that key still reaches your
app. AltGr, used for characters like @ or € on many keyboard layouts, never triggers the shortcut.
The right Ctrl and Alt keys don't either.

Locking the screen, Ctrl+Alt+Del, sleep and unplugging the keyboard cancel a press in progress.
