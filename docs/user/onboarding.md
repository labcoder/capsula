# Onboarding

The first time you start Kumochi, a window opens under the dock.

1. **Hi, I'm Kumo.** The assistant's name is in a box you can edit. Change it if you like, then
   press Enter or **Continue**.
2. **What should Kumo call you?** This is optional. Leave it blank to skip it. Your name is sent to
   the model you chat with, so the assistant can use it.
3. **Connect an account for Kumo.** ChatGPT and Claude Code, each as Settings → Accounts shows
   them (see [accounts](accounts.md)). Connect any you have, or none. The button reads
   **Not now** until one is ready, then **Continue**; either finishes onboarding and opens the
   peek. A sign-in still waiting for your browser carries on after that.

Both names can change later in [Settings](settings.md), and they follow the same rules there.

**Finish later**, Esc or closing the window hides onboarding until you next start Kumochi, which
opens it at the first step you haven't finished. Once you've signed in or chatted, it doesn't
come back by itself. A valid name you were editing is saved first;
one that can't be saved is dropped. Press Esc while editing a name to put the saved one back.
A ChatGPT or Claude Code sign-in you started in this window that hasn't finished is cancelled;
for Claude Code, Kumochi ends the sign-in running in Claude Code's window.

While the window shows, the taskbar button and the dock bring it to the front, and the shortcut
does nothing until you finish. If the background process disconnects, the window stays on its
step and waits.

To see onboarding again from the start, set `onboarded = false` and delete any `onboarding_from`
line in `config.toml`, in the `Kumochi` folder under `%APPDATA%`, then restart Kumochi.
