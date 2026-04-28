use std::{
    env,
    ffi::OsString,
    io::{self, Cursor, Read, Write},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
};

use gpui::{
    App, AppContext, Application, AsyncApp, Bounds, Context, Div, Edges, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyBinding, KeyDownEvent, ParentElement, Render, Stateful,
    StatefulInteractiveElement, Styled, Timer, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowOptions, actions, div, px, rgb, size,
};
use gpui_terminal::{ColorPalette, TerminalConfig, TerminalView};

const DEFAULT_TAIL: usize = 20;
const TERMINAL_COLS: usize = 96;
const TERMINAL_FONT_SIZE: f32 = 11.0;
const TERMINAL_ROW_HEIGHT: f32 = 14.0;
const TERMINAL_VERTICAL_PADDING: f32 = 16.0;
const CONTROL_ROW_HEIGHT: f32 = 46.0;
const WINDOW_WIDTH: f32 = 720.0;
const HIDE_CURSOR: &str = "\x1b[?25l";

actions!(session_review, [Close]);

struct TerminalFeed {
    receiver: Receiver<Vec<u8>>,
    cursor: Cursor<Vec<u8>>,
}

impl TerminalFeed {
    fn new(receiver: Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            cursor: Cursor::new(Vec::new()),
        }
    }
}

impl Read for TerminalFeed {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let read = self.cursor.read(buffer)?;
            if read > 0 {
                return Ok(read);
            }

            match self.receiver.recv() {
                Ok(bytes) => self.cursor = Cursor::new(bytes),
                Err(_) => return Ok(0),
            }
        }
    }
}

struct NullWriter;

impl Write for NullWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ReviewApp {
    session_id: Option<String>,
    tail: usize,
    tail_buffer: String,
    status: String,
    terminal: Entity<TerminalView>,
    terminal_sender: Sender<Vec<u8>>,
    focus_handle: FocusHandle,
    tail_focus: FocusHandle,
    send_text_focus: FocusHandle,
    send_text_buffer: String,
    caret_visible: bool,
}

impl ReviewApp {
    fn new(session_id: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (terminal_sender, terminal_receiver) = mpsc::channel();
        let terminal = cx.new(|cx| {
            TerminalView::new(
                NullWriter,
                TerminalFeed::new(terminal_receiver),
                terminal_config(DEFAULT_TAIL),
                cx,
            )
        });

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);

        let mut app = Self {
            session_id,
            tail: DEFAULT_TAIL,
            tail_buffer: DEFAULT_TAIL.to_string(),
            status: String::from("Ready"),
            terminal,
            terminal_sender,
            focus_handle,
            tail_focus: cx.focus_handle(),
            send_text_focus: cx.focus_handle(),
            send_text_buffer: String::new(),
            caret_visible: true,
        };

        cx.spawn(|this: WeakEntity<ReviewApp>, cx: &mut AsyncApp| {
            let mut cx = cx.clone();
            async move {
                loop {
                    Timer::after(Duration::from_millis(530)).await;
                    if this
                        .update(&mut cx, |view, cx| {
                            view.caret_visible = !view.caret_visible;
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
        .detach();

        app.refresh_logs();
        app
    }

    fn on_close(&mut self, _: &Close, window: &mut Window, cx: &mut Context<Self>) {
        window.remove_window();
        cx.quit();
    }

    fn send_key(&mut self, key: &'static str) {
        self.send_chunk(format!("key:{key}"), format!("Sent key:{key}"));
    }

    fn send_text(&mut self) {
        let text = self.send_text_buffer.clone();
        if text.is_empty() {
            self.status = String::from("Text input is empty");
            return;
        }

        self.send_text_buffer.clear();
        self.send_chunk(text, String::from("Sent text"));
    }

    fn send_chunk(&mut self, chunk: String, success_status: String) {
        let Some(session_id) = self.session_id.as_deref() else {
            self.write_terminal("\x1b[?25l\x1b[2J\x1b[H\x1b[31mMissing session id. Start with: oly-session-review <session-id>\x1b[0m\r\n");
            self.status = String::from("Missing session id");
            return;
        };

        let output = Command::new(oly_command())
            .arg("send")
            .arg(session_id)
            .arg(&chunk)
            .output();

        match output {
            Ok(output) if output.status.success() => {
                self.status = success_status;
            }
            Ok(output) => {
                self.status = String::from("oly send failed");
                self.write_command_failure("oly send", &output.stderr, &output.stdout);
            }
            Err(error) => {
                self.status = String::from("oly send failed");
                self.write_terminal(&format!(
                    "\x1b[?25l\x1b[2J\x1b[H\x1b[31mFailed to run oly send: {error}\x1b[0m\r\n"
                ));
            }
        }

        self.refresh_logs();
    }

    fn refresh_logs(&mut self) {
        let Some(session_id) = self.session_id.as_deref() else {
            self.write_terminal("\x1b[?25l\x1b[2J\x1b[H\x1b[31mMissing session id. Start with: oly-session-review <session-id>\x1b[0m\r\n");
            self.status = String::from("Missing session id");
            return;
        };

        let output = Command::new(oly_command())
            .arg("logs")
            .arg(session_id)
            .arg("--tail")
            .arg(self.tail.to_string())
            .arg("--keep-color")
            .arg("--no-truncate")
            .output();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[?25l\x1b[2J\x1b[H\x1b[38;5;244m$ oly logs ");
        bytes.extend_from_slice(session_id.as_bytes());
        bytes.extend_from_slice(b" --tail ");
        bytes.extend_from_slice(self.tail.to_string().as_bytes());
        bytes.extend_from_slice(b" --keep-color --no-truncate\x1b[0m\r\n\r\n");

        match output {
            Ok(output) if output.status.success() => {
                bytes.extend_from_slice(&normalize_newlines(&output.stdout));
                self.status = format!("Tail {}", self.tail);
            }
            Ok(output) => {
                bytes.extend_from_slice(b"\x1b[31moly logs failed\x1b[0m\r\n");
                bytes.extend_from_slice(&normalize_newlines(&output.stderr));
                bytes.extend_from_slice(&normalize_newlines(&output.stdout));
                self.status = String::from("oly logs failed");
            }
            Err(error) => {
                bytes.extend_from_slice(
                    format!("\x1b[31mFailed to run oly logs: {error}\x1b[0m\r\n").as_bytes(),
                );
                self.status = String::from("oly logs failed");
            }
        }

        bytes.extend_from_slice(HIDE_CURSOR.as_bytes());
        let _ = self.terminal_sender.send(bytes);
    }

    fn apply_tail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.tail_buffer.trim().parse::<usize>() {
            Ok(value) if value > 0 => {
                self.tail = value;
                self.tail_buffer = value.to_string();
                self.resize_terminal(window, cx);
                self.refresh_logs();
            }
            _ => {
                self.tail_buffer = self.tail.to_string();
                self.status = String::from("Tail must be a positive number");
            }
        }
    }

    fn resize_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminal_config = terminal_config(self.tail);
        self.terminal.update(cx, |terminal, cx| {
            terminal.update_config(terminal_config, cx)
        });
        window.resize(size(
            px(WINDOW_WIDTH),
            px(window_height_for_tail(self.tail)),
        ));
    }

    fn on_tail_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "enter" => self.apply_tail(window, cx),
            "backspace" => {
                self.tail_buffer.pop();
                self.status = String::from("Editing tail");
            }
            "escape" => {
                window.remove_window();
                cx.quit();
            }
            _ => {
                if let Some(key_char) = event.keystroke.key_char.as_deref() {
                    if key_char.chars().all(|character| character.is_ascii_digit()) {
                        self.tail_buffer.push_str(key_char);
                        self.status = String::from("Editing tail");
                    }
                }
            }
        }

        cx.stop_propagation();
        cx.notify();
    }

    fn on_send_text_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "enter" => self.send_text(),
            "backspace" => {
                self.send_text_buffer.pop();
                self.status = String::from("Editing text");
            }
            "space" => {
                self.send_text_buffer.push(' ');
                self.status = String::from("Editing text");
            }
            "escape" => {
                window.remove_window();
                cx.quit();
            }
            _ => {
                if let Some(key_char) = event.keystroke.key_char.as_deref() {
                    self.send_text_buffer.push_str(key_char);
                    self.status = String::from("Editing text");
                }
            }
        }

        cx.stop_propagation();
        cx.notify();
    }

    fn write_command_failure(&self, label: &str, stderr: &[u8], stdout: &[u8]) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[?25l\x1b[2J\x1b[H\x1b[31m");
        bytes.extend_from_slice(label.as_bytes());
        bytes.extend_from_slice(b" failed\x1b[0m\r\n");
        bytes.extend_from_slice(&normalize_newlines(stderr));
        bytes.extend_from_slice(&normalize_newlines(stdout));
        let _ = self.terminal_sender.send(bytes);
    }

    fn write_terminal(&self, text: &str) {
        let _ = self.terminal_sender.send(text.as_bytes().to_vec());
    }

    fn control_button(
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .h(px(32.0))
            .px_3()
            .flex()
            .items_center()
            .justify_center()
            .border_1()
            .border_color(rgb(0x2f5f50))
            .bg(rgb(0x101713))
            .text_color(rgb(0x9fffd2))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(0x173226)))
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            .child(label)
    }

    fn input_box(
        id: &'static str,
        width: f32,
        text: String,
        focus_handle: &FocusHandle,
        show_caret: bool,
    ) -> Stateful<Div> {
        let input = div()
            .id(id)
            .track_focus(focus_handle)
            .h(px(32.0))
            .px_2()
            .flex()
            .items_center()
            .border_1()
            .border_color(rgb(0x4c806d))
            .bg(rgb(0x090d0c))
            .text_color(rgb(0xc9ffe1))
            .cursor_pointer();

        let input = if width > 0.0 {
            input.w(px(width))
        } else {
            input.flex_1()
        };

        input.child(
            div()
                .flex()
                .items_center()
                .child(text)
                .child(div().w(px(1.0)).h(px(16.0)).bg(if show_caret {
                    rgb(0x9fffd2)
                } else {
                    rgb(0x090d0c)
                })),
        )
    }
}

impl Render for ReviewApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_send_caret = self.caret_visible && self.send_text_focus.is_focused(window);
        let show_tail_caret = self.caret_visible && self.tail_focus.is_focused(window);

        div()
            .key_context("SessionReview")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_close))
            .size_full()
            .bg(rgb(0x070a09))
            .text_color(rgb(0xd7ffe8))
            .font_family(coding_font_family())
            .text_size(px(12.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .h(px(terminal_height_for_tail(self.tail)))
                    .border_b_1()
                    .border_color(rgb(0x1d3d34))
                    .child(self.terminal.clone()),
            )
            .child(
                div()
                    .h(px(CONTROL_ROW_HEIGHT * 2.0))
                    .px_3()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap_2()
                    .bg(rgb(0x0b1110))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .w_full()
                            .child(
                                Self::input_box(
                                    "send-text-input",
                                    0.0,
                                    if self.send_text_buffer.is_empty() {
                                        if self.send_text_focus.is_focused(window) {
                                            String::new()
                                        } else {
                                            String::from("Input text and press Enter to send")
                                        }
                                    } else {
                                        self.send_text_buffer.clone()
                                    },
                                    &self.send_text_focus,
                                    show_send_caret,
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.send_text_focus.focus(window);
                                    this.caret_visible = true;
                                    cx.notify();
                                }))
                                .on_key_down(cx.listener(Self::on_send_text_key_down)),
                            )
                            .child(
                                Self::control_button("send-text", "Send", cx, |this, cx| {
                                    this.send_text();
                                    cx.notify();
                                })
                                .w(px(60.0)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .w_full()
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(Self::control_button("send-up", "Up", cx, |this, cx| {
                                        this.send_key("up");
                                        cx.notify();
                                    }))
                                    .child(Self::control_button(
                                        "send-down",
                                        "Down",
                                        cx,
                                        |this, cx| {
                                            this.send_key("down");
                                            cx.notify();
                                        },
                                    ))
                                    .child(Self::control_button(
                                        "send-enter",
                                        "Enter",
                                        cx,
                                        |this, cx| {
                                            this.send_key("enter");
                                            cx.notify();
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        Self::input_box(
                                            "tail-input",
                                            0.0,
                                            format!("Tail: {}", self.tail_buffer),
                                            &self.tail_focus,
                                            show_tail_caret,
                                        )
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.tail_focus.focus(window);
                                            this.caret_visible = true;
                                            cx.notify();
                                        }))
                                        .on_key_down(cx.listener(Self::on_tail_key_down)),
                                    )
                                    .child(
                                        Self::control_button("close", "Close", cx, |_this, cx| {
                                            cx.quit();
                                        })
                                        .w(px(60.0)),
                                    ),
                            ),
                    ),
            )
    }
}

fn terminal_config(rows: usize) -> TerminalConfig {
    TerminalConfig {
        cols: TERMINAL_COLS,
        rows,
        font_family: coding_font_family().into(),
        font_size: px(TERMINAL_FONT_SIZE),
        scrollback: 1_000,
        line_height_multiplier: 1.0,
        padding: Edges::all(px(8.0)),
        colors: ColorPalette::builder()
            .background(0x06, 0x08, 0x08)
            .foreground(0xd7, 0xff, 0xe8)
            .cursor(0x06, 0x08, 0x08)
            .black(0x06, 0x08, 0x08)
            .red(0xff, 0x66, 0x66)
            .green(0x75, 0xf0, 0xa0)
            .yellow(0xff, 0xd1, 0x66)
            .blue(0x7a, 0xb7, 0xff)
            .magenta(0xd5, 0x8c, 0xff)
            .cyan(0x5e, 0xf1, 0xd6)
            .white(0xd7, 0xff, 0xe8)
            .build(),
    }
}

fn terminal_height_for_tail(tail: usize) -> f32 {
    tail as f32 * TERMINAL_ROW_HEIGHT + TERMINAL_VERTICAL_PADDING
}

fn window_height_for_tail(tail: usize) -> f32 {
    terminal_height_for_tail(tail) + CONTROL_ROW_HEIGHT * 2.0
}

fn coding_font_family() -> &'static str {
    if cfg!(target_os = "windows") {
        "Cascadia Mono"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "monospace"
    }
}

fn normalize_newlines(bytes: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(bytes)
        .replace('\n', "\r\n")
        .into_bytes()
}

fn oly_command() -> OsString {
    if let Some(command) = env::var_os("OLY_BIN") {
        return command;
    }

    if let Ok(current_exe) = env::current_exe() {
        let candidate = current_exe.with_file_name(format!("oly{}", env::consts::EXE_SUFFIX));
        if candidate.exists() {
            return candidate.into_os_string();
        }
    }

    OsString::from("oly")
}

fn main() {
    let session_id = env::args().nth(1).filter(|arg| !arg.trim().is_empty());
    let window_title = session_id
        .as_deref()
        .map(|id| format!("Oly Session Review - {id}"))
        .unwrap_or_else(|| String::from("Oly Session Review"));
    let app = Application::new();

    app.run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("escape", Close, Some("SessionReview"))]);

        let bounds = Bounds::centered(
            None,
            size(px(WINDOW_WIDTH), px(window_height_for_tail(DEFAULT_TAIL))),
            cx,
        );
        cx.open_window(
            WindowOptions {
                focus: true,
                titlebar: Some(TitlebarOptions {
                    title: Some(window_title.clone().into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| ReviewApp::new(session_id.clone(), window, cx)),
        )
        .expect("failed to open session review window");

        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
}
