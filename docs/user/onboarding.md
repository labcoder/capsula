# Onboarding

The first time you start Kumochi, a window opens under the dock.

1. **Hi, I'm Kumo.** The assistant's name is in a box you can edit. Change it if you like, then
   press Enter or **Continue**.
2. **What should Kumo call you?** This is optional. Leave it blank to skip it. Your name is sent to
   the model you chat with, so the assistant can use it.

**Continue** on the second step finishes onboarding and opens the status panel. Both names can
change later in [Settings](settings.md), and they follow the same rules there.

**Finish later**, Esc or closing the window hides onboarding until you next start Kumochi. A name
you were editing is saved first. Press Esc while editing a name to put the saved one back.

While the window shows, the shortcut, the taskbar button and the dock bring it to the front
instead of opening anything else. If the background process disconnects, the window stays on its
step and waits.

To see onboarding again, set `onboarded = false` in `config.toml`, in the `Kumochi` folder under
`%APPDATA%`, then restart Kumochi.
