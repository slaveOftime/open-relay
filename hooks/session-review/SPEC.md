## Session Review Desktop App

Create a small Rust desktop app with GPUI (https://www.gpui.rs/) and gpui-terminal (https://github.com/zortax/gpui-terminal). The source lives directly in this folder and is exposed from the root Cargo manifest as a separate binary.

```text
 ----------------------------------------------------
|                                                    |
|  terminal to show oly logs tail 20 for the session |
|                                                    |
|----------------------------------------------------|
| [_Input text and enter to send___________] | Send  |
| Up Down Enter                    | tail 20 | Close |
 ----------------------------------------------------
```

The app has one window and one page.

## Startup

- The first command-line argument is the session id.
- If the session id is missing, the main window opens and shows an error instead of exiting silently.
- The native window title is `Oly Session Review - <session-id>` when a session is provided, or `Oly Session Review` otherwise.
- The initial log view runs `oly logs <session> --tail 20 --keep-color --no-truncate`.
- The terminal pane height is sized to the current tail value, so `tail 20` gives a 20-row terminal pane.

## Controls

- `Up`, `Down`, and `Enter` buttons run `oly send <session> key:<up|down|enter>`.
- The text input plus `Send` button runs `oly send <session> <text>` and refreshes the log pane. Spaces typed into the input are preserved.
- After a send succeeds or fails, the terminal pane refreshes with the current tail value.
- The tail field defaults to `20`. Editing the field and pressing `Enter` applies the value immediately and refreshes the log pane.
- Focused text inputs show a blinking cursor directly after the entered text.
- `Close` and `Esc` close the window/app.

## UI Style

- Keep the style geeky, compact, and simple.
- Use a dark terminal-like palette, system monospace coding fonts, sharp borders, and minimal decoration.
- The terminal pane should occupy most of the window, with two compact control rows at the bottom.
- Do not show a separate header/status strip above the terminal; keep metadata out of the main view.
