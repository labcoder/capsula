# The shortcut

Kumochi listens for **left Ctrl + left Alt**, from any app, without taking those keys from it.

- **Press and hold** both keys. The dock shows Listening as soon as both are down. After about
  300 ms the press counts as a hold. Talking to Kumochi with a hold arrives in a later release;
  for now, releasing the keys just returns the dock to its status.
- **Double tap** both keys, two short presses with their releases less than 300 ms apart. A small
  typing box opens under the dock and takes focus. [Esc](escape.md) hides it after stopping
  work and clearing any drawing. Whatever you typed is kept for next time, and the app you
  were using gets focus back. Input goes to Kumo unless you explicitly [switch tasks](tasks.md).

Pressing any other key while holding the shortcut cancels it, and that key still reaches your
app. AltGr, used for characters like @ or € on many keyboard layouts, never triggers the shortcut.
The right Ctrl and Alt keys don't either.

Locking the screen, Ctrl+Alt+Del, sleep and unplugging the keyboard cancel a press in progress.
