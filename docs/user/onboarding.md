# Onboarding

The first time you start Kumochi, a window opens under the dock.

1. **Hi, I'm Kumo.** The assistant's name is in a box you can edit, with Kumo under it. Change it
   if you like, then press Enter or **Continue**.
2. **What should Kumo call you?** This is optional. Leave it blank to skip it. Your name is sent to
   the model you chat with, so the assistant can use it.
3. **Connect an account for Kumo.** Use the sign-in button beside ChatGPT or Claude Code.
   Scroll for more details when needed. You can also connect either later in
   [Settings → Accounts](accounts.md). The bottom button reads **Not now** until one is ready,
   then **Continue**; either goes on to the last step. A sign-in
   still waiting for your browser carries on after that, and closing the window later doesn't
   cancel it.
4. **Double-tap left Ctrl + left Alt to chat with Kumo.** Two quick presses of both keys finish
   onboarding and open the typing box under the dock. **Finish** finishes it too and opens the
   peek instead.

Both names can change later in [Settings](settings.md), and they follow the same rules there.

**Finish later**, Esc or closing the window hides onboarding until you next start Kumochi, which
opens it at the first step you haven't finished. Once you've signed in or chatted, it doesn't
come back by itself. A valid name you were editing is saved first;
one that can't be saved is dropped. Press Esc while editing a name to put the saved one back.
A ChatGPT or Claude Code sign-in you started at the account step that hasn't finished is cancelled;
for Claude Code, Kumochi ends the sign-in running in Claude Code's window.

While the window shows, the taskbar button and the dock bring it to the front, and the shortcut
does nothing until the last step. If the background process disconnects, the window stays on its
step and waits.

To see onboarding again from the start, set `onboarded = false` and delete any `onboarding_from`
line in `config.toml`, in the `Kumochi` folder under `%APPDATA%`, then restart Kumochi.
