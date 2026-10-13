//! Windows console input for attach. Crossterm's Windows event source does not
//! decode bracketed paste: raw VT records lose ESC/control characters there.
//! Keep complete VT sequences on the wire and collect explicit paste boundaries;
//! native keyboard/mouse/resize records still become semantic terminal events.
//! Splitting a CSI across writes can make ConPTY inject its parameters as keys
//! instead of producing a native mouse/key record in the child.
use crossterm::event::Event;

pub(super) enum AttachInputEvent {
    Terminal(Event),
    #[cfg(windows)]
    Raw(Vec<u8>),
}

#[cfg(any(windows, test))]
#[derive(Default)]
struct PasteDecoder {
    pending: String,
    paste: Option<String>,
}

#[cfg(any(windows, test))]
impl PasteDecoder {
    // Ordinary text is immediate, but escape sequences must stay in one input
    // frame: ConPTY does not reliably parse CSI split across input writes.
    // Paste recognition runs first; its payload is never parsed as VT input.
    fn push(&mut self, ch: char) -> Vec<DecodedInput> {
        self.pending.push(ch);
        let mut out = Vec::new();
        loop {
            let marker = if self.paste.is_some() {
                "\x1b[201~"
            } else {
                "\x1b[200~"
            };
            if self.pending == marker {
                self.pending.clear();
                if let Some(paste) = self.paste.take() {
                    out.push(DecodedInput::Paste(paste));
                } else {
                    self.paste = Some(String::new());
                }
                break;
            }
            if marker.starts_with(&self.pending) {
                break;
            }
            if self.paste.is_none() && self.pending.starts_with('\x1b') {
                if !vt_sequence_complete(&self.pending) {
                    break;
                }
                out.push(DecodedInput::Raw(
                    std::mem::take(&mut self.pending).into_bytes(),
                ));
                break;
            }
            let ch = self.pending.remove(0);
            if let Some(paste) = self.paste.as_mut() {
                paste.push(ch);
            } else {
                out.push(DecodedInput::Raw(ch.to_string().into_bytes()));
            }
        }
        out
    }
}

// Input CSI (mouse, focus, modified keys) and SS3 (application keys) have a
// final byte in 0x40..=0x7e. Legacy mouse uses CSI M plus three encoded fields.
// Invalid/control bytes or an excessive prefix are flushed rather than held
// indefinitely. The reader separately times out an incomplete Escape prefix.
#[cfg(any(windows, test))]
fn vt_sequence_complete(sequence: &str) -> bool {
    let bytes = sequence.as_bytes();
    if bytes.len() > 256 {
        return true;
    }
    match bytes.get(1) {
        None => false,
        Some(b'[') if bytes.get(2) == Some(&b'M') => sequence.chars().count() >= 6,
        Some(b'[' | b'O') => bytes
            .get(2..)
            .is_some_and(|body| body.iter().any(|byte| !(0x20..=0x3f).contains(byte))),
        Some(_) => true,
    }
}

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq)]
enum DecodedInput {
    Raw(Vec<u8>),
    Paste(String),
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use std::{
        collections::VecDeque,
        io::{self, Write},
    };
    use windows_sys::Win32::{
        Foundation::{HANDLE, WAIT_TIMEOUT},
        System::{Console::*, Threading::WaitForSingleObject},
    };

    /// Owns VT input and mouse-mode changes for attach. RawModeGuard owns raw
    /// mode; this guard drops first so its baseline can be restored normally.
    pub(crate) struct VtInputGuard {
        handle: HANDLE,
        original_mode: u32,
    }

    impl VtInputGuard {
        pub(crate) fn new() -> io::Result<Self> {
            // SAFETY: STD_INPUT_HANDLE is the interactive console checked by
            // attach. The APIs validate it; mode pointers are valid locals.
            unsafe {
                let handle = GetStdHandle(STD_INPUT_HANDLE);
                let mut mode = 0;
                if GetConsoleMode(handle, &mut mode) == 0 {
                    return Err(io::Error::last_os_error());
                }
                if SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_INPUT) == 0 {
                    return Err(io::Error::last_os_error());
                }
                // VT input disables ConPTY's automatic native-key reporting.
                // Request it explicitly so bitmap-only terminal paste bindings
                // still deliver their key-up (text paste remains bracketed VT).
                let mut stdout = io::stdout();
                if let Err(err) = stdout
                    .write_all(b"\x1b[?9001h")
                    .and_then(|()| stdout.flush())
                {
                    SetConsoleMode(handle, mode);
                    return Err(err);
                }
                Ok(Self {
                    handle,
                    original_mode: mode,
                })
            }
        }
    }

    impl Drop for VtInputGuard {
        fn drop(&mut self) {
            // SAFETY: The borrowed standard console handle remains open.
            // Restore the raw-mode baseline before RawModeGuard drops.
            let mut stdout = io::stdout();
            let _ = stdout.write_all(b"\x1b[?9001l");
            let _ = stdout.flush();
            unsafe {
                SetConsoleMode(self.handle, self.original_mode);
            }
        }
    }

    pub(crate) fn sync_mouse_mode(enabled: bool) -> io::Result<()> {
        // SAFETY: Borrowed console handle, valid local mode pointer. Unlike
        // crossterm's mouse API this never clears raw mode or VT input.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0;
            if GetConsoleMode(handle, &mut mode) == 0 {
                return Err(io::Error::last_os_error());
            }
            mode |= ENABLE_EXTENDED_FLAGS;
            if enabled {
                mode = (mode | ENABLE_MOUSE_INPUT) & !ENABLE_QUICK_EDIT_MODE;
            } else {
                mode &= !ENABLE_MOUSE_INPUT;
            }
            if SetConsoleMode(handle, mode) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub(crate) fn read_events(mut emit: impl FnMut(AttachInputEvent) -> bool) -> io::Result<()> {
        // SAFETY: Reading one record into a correctly-sized initialized local;
        // each union member is accessed only for its documented EventType.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut decoder = PasteDecoder::default();
        let mut surrogate = None;
        let mut buttons = 0;
        let mut clipboard_key_down = false;
        let mut insert_key_down = false;
        let mut native_keys = VecDeque::new();
        let mut transport = String::new();
        loop {
            // A lone Escape is ambiguous with a VT prefix. Bound only that
            // ambiguity (as Unix terminal readers do), never a paste burst.
            if native_keys.is_empty()
                && decoder.paste.is_none()
                && (!decoder.pending.is_empty() || !transport.is_empty())
                && unsafe { WaitForSingleObject(handle, 50) } == WAIT_TIMEOUT
            {
                let mut bytes = std::mem::take(&mut decoder.pending).into_bytes();
                bytes.extend_from_slice(std::mem::take(&mut transport).as_bytes());
                if !emit(AttachInputEvent::Raw(bytes)) {
                    return Ok(());
                }
            }
            let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
            let from_transport = !native_keys.is_empty();
            if let Some(key) = native_keys.pop_front() {
                record.EventType = KEY_EVENT as u16;
                record.Event.KeyEvent = key;
            } else {
                let mut count = 0;
                if unsafe { ReadConsoleInputW(handle, &mut record, 1, &mut count) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                if count == 0 {
                    continue;
                }
            }
            let event = match record.EventType as u32 {
                KEY_EVENT => {
                    let key = unsafe { record.Event.KeyEvent };
                    let unit = unsafe { key.uChar.UnicodeChar };
                    if !from_transport && key.wVirtualKeyCode == 0 {
                        if key.bKeyDown == 0 {
                            continue;
                        }
                        // Unwrap ?9001 before paste/CSI decoding. ConPTY can
                        // mix raw ESC with an encoded '[' record; using one
                        // parser for transport and payload corrupts boundaries.
                        for ch in decode_utf16_unit(&mut surrogate, unit) {
                            transport.push(ch);
                            if transport.len() <= 256
                                && transport.starts_with('\x1b')
                                && (transport == "\x1b"
                                    || transport == "\x1b["
                                    || (transport.starts_with("\x1b[")
                                        && transport[2..]
                                            .bytes()
                                            .all(|b| b.is_ascii_digit() || b == b';')))
                            {
                                continue;
                            }
                            if let Some(key) = parse_win32_key(transport.as_bytes()) {
                                native_keys.push_back(key);
                            } else {
                                for unit in transport.encode_utf16() {
                                    native_keys.push_back(KEY_EVENT_RECORD {
                                        bKeyDown: 1,
                                        wRepeatCount: 1,
                                        wVirtualKeyCode: 0,
                                        wVirtualScanCode: 0,
                                        uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                                        dwControlKeyState: 0,
                                    });
                                }
                            }
                            transport.clear();
                        }
                        continue;
                    }
                    // VT input records have no virtual key. Don't interpret
                    // their CR/LF/ESC as native Enter/Escape keys, or run
                    // clipboard/detach shortcuts while inside a paste.
                    if key.wVirtualKeyCode == 0 || decoder.paste.is_some() {
                        // ConPTY can retain a native VK for Unicode paste
                        // characters. Only key-down text contributes; its
                        // synthetic key-up must not duplicate UTF-16 units.
                        if key.bKeyDown == 0 || (unit == 0 && key.wVirtualKeyCode != 0) {
                            continue;
                        }
                        for ch in decode_utf16_unit(&mut surrogate, unit) {
                            for decoded in decoder.push(ch) {
                                let event = match decoded {
                                    DecodedInput::Paste(text) => {
                                        AttachInputEvent::Terminal(Event::Paste(text))
                                    }
                                    DecodedInput::Raw(bytes) if bytes == b"\x04" => {
                                        AttachInputEvent::Terminal(Event::Key(KeyEvent::new(
                                            KeyCode::Char('d'),
                                            KeyModifiers::CONTROL,
                                        )))
                                    }
                                    DecodedInput::Raw(bytes) if bytes == b"\x16" => {
                                        clipboard_key_down = true;
                                        AttachInputEvent::Terminal(Event::Key(KeyEvent::new(
                                            KeyCode::Char('v'),
                                            KeyModifiers::CONTROL,
                                        )))
                                    }
                                    DecodedInput::Raw(bytes) if bytes == b"\x1b[2;2~" => {
                                        insert_key_down = true;
                                        // VT input replaces native Shift+Insert
                                        // records with xterm's modified CSI.
                                        // Preserve its local file/image paste
                                        // action, rather than sending it raw.
                                        AttachInputEvent::Terminal(Event::Key(KeyEvent::new(
                                            KeyCode::Insert,
                                            KeyModifiers::SHIFT,
                                        )))
                                    }
                                    DecodedInput::Raw(bytes) => AttachInputEvent::Raw(bytes),
                                };
                                if !emit(event) {
                                    return Ok(());
                                }
                            }
                        }
                        continue;
                    }
                    let mods = modifiers(key.dwControlKeyState);
                    // Windows Terminal may consume Ctrl+V key-down in its
                    // PasteFromClipboard binding. A screenshot has no text
                    // to emit, so only the native key-up reaches us. Recover V
                    // from its virtual key even when UnicodeChar is zero, and
                    // synthesize one press only if we didn't see key-down.
                    let clipboard_key = if key.wVirtualKeyCode == 0x56
                        && mods.contains(KeyModifiers::CONTROL)
                        && !mods.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
                    {
                        Some((
                            KeyCode::Char('v'),
                            KeyModifiers::CONTROL,
                            &mut clipboard_key_down,
                        ))
                    } else if key.wVirtualKeyCode == 0x2d
                        && mods.contains(KeyModifiers::SHIFT)
                        && !mods.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
                    {
                        Some((KeyCode::Insert, KeyModifiers::SHIFT, &mut insert_key_down))
                    } else {
                        None
                    };
                    if let Some((code, modifiers, seen_down)) = clipboard_key {
                        let down = key.bKeyDown != 0;
                        let emit_press = down || !*seen_down;
                        *seen_down = down;
                        if emit_press
                            && !emit(AttachInputEvent::Terminal(Event::Key(KeyEvent::new(
                                code, modifiers,
                            ))))
                        {
                            return Ok(());
                        }
                        continue;
                    }
                    let code = match key.wVirtualKeyCode {
                        0x10..=0x12 => continue, // modifier-only records
                        0x08 => KeyCode::Backspace,
                        0x09 if mods.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
                        0x09 => KeyCode::Tab,
                        0x0d => KeyCode::Enter,
                        0x1b => KeyCode::Esc,
                        0x21 => KeyCode::PageUp,
                        0x22 => KeyCode::PageDown,
                        0x23 => KeyCode::End,
                        0x24 => KeyCode::Home,
                        0x25 => KeyCode::Left,
                        0x26 => KeyCode::Up,
                        0x27 => KeyCode::Right,
                        0x28 => KeyCode::Down,
                        0x2d => KeyCode::Insert,
                        0x2e => KeyCode::Delete,
                        0x70..=0x87 => KeyCode::F((key.wVirtualKeyCode - 0x6f) as u8),
                        _ => {
                            // Native Ctrl-letter records carry the control
                            // byte in UnicodeChar; recover the semantic key.
                            if mods.contains(KeyModifiers::CONTROL)
                                && unit <= 26
                                && (0x41..=0x5a).contains(&key.wVirtualKeyCode)
                            {
                                KeyCode::Char(
                                    char::from_u32(u32::from(key.wVirtualKeyCode) + 0x20).unwrap(),
                                )
                            } else {
                                let chars = decode_utf16_unit(&mut surrogate, unit);
                                for ch in chars {
                                    let kind = if key.bKeyDown != 0 {
                                        KeyEventKind::Press
                                    } else {
                                        KeyEventKind::Release
                                    };
                                    for _ in 0..key.wRepeatCount.max(1) {
                                        if !emit(AttachInputEvent::Terminal(Event::Key(
                                            KeyEvent::new_with_kind(KeyCode::Char(ch), mods, kind),
                                        ))) {
                                            return Ok(());
                                        }
                                    }
                                }
                                continue;
                            }
                        }
                    };
                    let kind = if key.bKeyDown != 0 {
                        KeyEventKind::Press
                    } else {
                        KeyEventKind::Release
                    };
                    for _ in 0..key.wRepeatCount.max(1) {
                        if !emit(AttachInputEvent::Terminal(Event::Key(
                            KeyEvent::new_with_kind(code, mods, kind),
                        ))) {
                            return Ok(());
                        }
                    }
                    continue;
                }
                WINDOW_BUFFER_SIZE_EVENT => {
                    // Crossterm's size() gives visible dimensions, unlike the
                    // native record which can describe the scrollback buffer.
                    let (cols, rows) = crossterm::terminal::size()?;
                    Event::Resize(cols, rows)
                }
                FOCUS_EVENT => {
                    if unsafe { record.Event.FocusEvent.bSetFocus } != 0 {
                        Event::FocusGained
                    } else {
                        Event::FocusLost
                    }
                }
                MOUSE_EVENT => {
                    let mouse = unsafe { record.Event.MouseEvent };
                    let next = mouse.dwButtonState & 0x1f;
                    let button = if next & 1 != 0 {
                        MouseButton::Left
                    } else if next & 2 != 0 {
                        MouseButton::Right
                    } else {
                        MouseButton::Middle
                    };
                    let kind = match mouse.dwEventFlags {
                        MOUSE_WHEELED if (mouse.dwButtonState >> 16) as i16 > 0 => {
                            MouseEventKind::ScrollUp
                        }
                        MOUSE_WHEELED => MouseEventKind::ScrollDown,
                        MOUSE_HWHEELED if (mouse.dwButtonState >> 16) as i16 > 0 => {
                            MouseEventKind::ScrollRight
                        }
                        MOUSE_HWHEELED => MouseEventKind::ScrollLeft,
                        MOUSE_MOVED if next != 0 => MouseEventKind::Drag(button),
                        MOUSE_MOVED => MouseEventKind::Moved,
                        0 | DOUBLE_CLICK if next & !buttons != 0 => MouseEventKind::Down(button),
                        0 if buttons & !next != 0 => MouseEventKind::Up(if buttons & 1 != 0 {
                            MouseButton::Left
                        } else if buttons & 2 != 0 {
                            MouseButton::Right
                        } else {
                            MouseButton::Middle
                        }),
                        _ => {
                            buttons = next;
                            continue;
                        }
                    };
                    buttons = next;
                    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
                    let top = if unsafe {
                        GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info)
                    } != 0
                    {
                        info.srWindow.Top
                    } else {
                        0
                    };
                    Event::Mouse(MouseEvent {
                        kind,
                        column: mouse.dwMousePosition.X.max(0) as u16,
                        row: (mouse.dwMousePosition.Y - top).max(0) as u16,
                        modifiers: modifiers(mouse.dwControlKeyState),
                    })
                }
                _ => continue,
            };
            if !emit(AttachInputEvent::Terminal(event)) {
                return Ok(());
            }
        }
    }

    // Windows Terminal's ?9001 protocol: Vk;Sc;Uc;Kd;Cs;Rc_. Under VT
    // input ConPTY exposes the encoding as VK=0 characters, so reconstruct
    // the same record and use the native key path (no second key mapping).
    fn parse_win32_key(bytes: &[u8]) -> Option<KEY_EVENT_RECORD> {
        let body = std::str::from_utf8(bytes)
            .ok()?
            .strip_prefix("\x1b[")?
            .strip_suffix('_')?;
        let values = body
            .split(';')
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        let [vk, scan, unit, down, mods, repeat] = values.as_slice() else {
            return None;
        };
        if *down > 1 {
            return None;
        }
        Some(KEY_EVENT_RECORD {
            bKeyDown: *down as i32,
            wRepeatCount: u16::try_from(*repeat).ok()?,
            wVirtualKeyCode: u16::try_from(*vk).ok()?,
            wVirtualScanCode: u16::try_from(*scan).ok()?,
            uChar: KEY_EVENT_RECORD_0 {
                UnicodeChar: u16::try_from(*unit).ok()?,
            },
            dwControlKeyState: *mods,
        })
    }

    fn modifiers(state: u32) -> KeyModifiers {
        let mut mods = KeyModifiers::NONE;
        if state & SHIFT_PRESSED != 0 {
            mods |= KeyModifiers::SHIFT;
        }
        if state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 {
            mods |= KeyModifiers::ALT;
        }
        if state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0 {
            mods |= KeyModifiers::CONTROL;
        }
        mods
    }
}

#[cfg(windows)]
pub(super) use windows::{VtInputGuard, read_events, sync_mouse_mode};

#[cfg(any(windows, test))]
fn decode_utf16_unit(surrogate: &mut Option<u16>, unit: u16) -> Vec<char> {
    if (0xd800..=0xdbff).contains(&unit) {
        let previous = surrogate.replace(unit);
        return previous.map(|_| vec!['\u{fffd}']).unwrap_or_default();
    }
    if let Some(high) = surrogate.take() {
        char::decode_utf16([high, unit])
            .map(|ch| ch.unwrap_or('\u{fffd}'))
            .collect()
    } else {
        char::decode_utf16([unit])
            .map(|ch| ch.unwrap_or('\u{fffd}'))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(text: &str) -> Vec<DecodedInput> {
        let mut decoder = PasteDecoder::default();
        text.chars().flat_map(|ch| decoder.push(ch)).collect()
    }

    #[test]
    fn explicit_paste_preserves_newlines_controls_and_unicode() {
        assert_eq!(
            decode("\x1b[200~one\r\ntwo\n工作😀\x04\x16\x1b[201~"),
            vec![DecodedInput::Paste("one\r\ntwo\n工作😀\x04\x16".into())]
        );
    }

    #[test]
    fn keys_before_after_and_inside_paste_have_distinct_boundaries() {
        assert_eq!(
            decode("\r\x1b[200~\r\x1b[A\x1b[201~\r"),
            vec![
                DecodedInput::Raw(vec![b'\r']),
                DecodedInput::Paste("\r\x1b[A".into()),
                DecodedInput::Raw(vec![b'\r']),
            ]
        );
    }

    #[test]
    fn ordinary_vt_sequences_are_forwarded_without_key_rewriting() {
        let events = decode("\x1b[A\x1b[1;5D\r\n\x1bx");
        let raw = events
            .into_iter()
            .flat_map(|ev| match ev {
                DecodedInput::Raw(bytes) => bytes,
                DecodedInput::Paste(_) => panic!("ordinary keys became a paste"),
            })
            .collect::<Vec<_>>();
        assert_eq!(raw, b"\x1b[A\x1b[1;5D\r\n\x1bx");
    }

    #[test]
    fn mouse_and_key_sequences_each_remain_one_input_frame() {
        for sequence in [
            "\x1b[<35;54;26M",
            "\x1b[<0;54;26m",
            "\x1b[A",
            "\x1b[1;5D",
            "\x1bOP",
            "\x1b[I",
            "\x1b[O",
            "\x1b[M !!",
            "\x1bx",
        ] {
            assert_eq!(
                decode(sequence),
                vec![DecodedInput::Raw(sequence.as_bytes().to_vec())],
                "split VT sequence: {sequence:?}"
            );
        }
        assert_eq!(
            decode("a\x1b[<35;54;26Mb"),
            vec![
                DecodedInput::Raw(b"a".to_vec()),
                DecodedInput::Raw(b"\x1b[<35;54;26M".to_vec()),
                DecodedInput::Raw(b"b".to_vec()),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "isolated console input helper"]
    fn clipboard_shortcut_probe() {
        use crossterm::event::{KeyCode, KeyEventKind};
        use std::io::Write;
        let cwd = std::env::current_dir().unwrap();
        if !cwd.join("shortcut-probe.enabled").is_file() {
            return;
        }
        let _raw = crate::terminal_guards::RawModeGuard::new().unwrap();
        let _vt = VtInputGuard::new().unwrap();
        println!("SHORTCUT_READY");
        std::io::stdout().flush().unwrap();
        let raw_win32_keys = cwd.join("raw-win32-keys.enabled").exists();
        let config = crate::config::AppConfig::load().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (mut input_writer, captured) = runtime.block_on(async {
            use interprocess::local_socket::tokio::prelude::*;
            let listener = crate::ipc::bind(&config).unwrap();
            let client = crate::ipc::connect(&config).await.unwrap();
            let server = listener.accept().await.unwrap();
            let (read, _) = tokio::io::split(server);
            let (_, write) = tokio::io::split(client);
            let captured = tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(read);
                let mut frames = Vec::new();
                let count = if raw_win32_keys { 2 } else { 4 };
                for _ in 0..count {
                    let crate::protocol::RpcRequest::AttachInput { id, data, .. } =
                        crate::ipc::read_request_from_reader(&mut reader)
                            .await
                            .unwrap()
                    else {
                        panic!("expected forwarded attachment path")
                    };
                    assert_eq!(id, "fixture");
                    frames.push(String::from_utf8(data).unwrap());
                }
                frames
            });
            (write, captured)
        });
        let mut captured = Some(captured);
        let mut shortcuts = Vec::new();
        let mut saved_paths = Vec::new();
        let mut raw = Vec::new();
        let mut pastes = Vec::new();
        // Fixture injection must not be re-encoded by the outer test ConPTY.
        // Production enables ?9001; these fixtures cover the desktop trace
        // itself and the older fallback wire protocol without double wrapping.
        print!("\x1b[?9001l");
        std::io::stdout().flush().unwrap();
        if raw_win32_keys {
            use windows_sys::Win32::System::Console::*;
            // Replay exactly what the desktop probe observed: ?9001 input is
            // itself carried as VK=0 UTF-16 records while VT input is active.
            let trace = "\x1b[17;29;0;1;8;1_\x1b[200~\x1b[201~\x1b[86;47;22;0;8;1_\x1b[17;29;0;0;0;1_\x1b[16;42;0;1;16;1_\x1b[200~\x1b[201~\x1b[45;82;0;0;272;1_\x1b[16;42;0;0;0;1_\x1b[65;30;97;1;0;1_\x1b[65;30;97;0;0;1_#";
            let records = trace
                .encode_utf16()
                .map(|unit| {
                    let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
                    record.EventType = KEY_EVENT as u16;
                    record.Event.KeyEvent = KEY_EVENT_RECORD {
                        bKeyDown: 1,
                        wRepeatCount: 1,
                        wVirtualKeyCode: 0,
                        wVirtualScanCode: 0,
                        uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                        dwControlKeyState: 0,
                    };
                    record
                })
                .collect::<Vec<_>>();
            let mut written = 0;
            // SAFETY: The isolated helper owns this console; the initialized
            // record slice and output count remain valid throughout the call.
            assert_ne!(
                unsafe {
                    WriteConsoleInputW(
                        GetStdHandle(STD_INPUT_HANDLE),
                        records.as_ptr(),
                        records.len() as u32,
                        &mut written,
                    )
                },
                0
            );
            assert_eq!(written as usize, records.len());
        }
        read_events(|event| {
            let done = match event {
                AttachInputEvent::Terminal(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if super::super::attach::is_clipboard_paste_key(key) {
                        let image_paste = key.code == KeyCode::Char('v');
                        let path = crate::clipboard::handle_clipboard_paste_with(
                            &config, "fixture", true,
                            || Ok(if image_paste { None } else { Some(vec![cwd.join("source image.png")]) }),
                            || Ok(Some(crate::clipboard::ClipboardImage {
                                width: 1, height: 1, bytes: vec![17, 34, 51, 255],
                            })),
                            || panic!("file/image paste must not read clipboard text"),
                        ).unwrap().unwrap();
                        runtime.block_on(super::super::attach::send_attach_input(
                            &mut input_writer, "fixture",
                            super::super::attach::wrap_paste_input(path.clone(), true), true,
                        )).unwrap();
                        saved_paths.push(path);
                        shortcuts.push(format!("{:?}", key.code));
                    }
                    key.code == KeyCode::Char('#')
                }
                AttachInputEvent::Raw(bytes) if bytes == b"#" => true,
                AttachInputEvent::Raw(bytes) => { raw.extend(bytes); false }
                AttachInputEvent::Terminal(Event::Paste(text)) => { pastes.push(text); false }
                _ => false,
            };
            if done {
                let frames = runtime.block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(2), captured.take().unwrap()).await.unwrap().unwrap()
                });
                std::fs::write(cwd.join("shortcuts.json"), serde_json::to_vec(&serde_json::json!({
                    "shortcuts": shortcuts, "raw": raw, "saved_paths": saved_paths, "pastes": pastes, "frames": frames,
                })).unwrap()).unwrap();
            }
            !done
        }).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn vt_clipboard_shortcuts_save_images_and_files() {
        run_shortcut_probe(false);
    }

    #[cfg(windows)]
    #[test]
    fn desktop_win32_key_trace_saves_and_forwards_screenshot() {
        run_shortcut_probe(true);
    }

    #[cfg(windows)]
    fn run_shortcut_probe(raw_win32_keys: bool) {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        use std::{
            io::{Read, Write},
            sync::mpsc,
            time::{Duration, Instant},
        };
        struct Child(Box<dyn portable_pty::Child + Send + Sync>);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let cwd = std::env::temp_dir().join(format!("oly-shortcut-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let report = cwd.join("shortcuts.json");
        let _ = std::fs::remove_file(&report);
        std::fs::write(cwd.join("shortcut-probe.enabled"), b"").unwrap();
        if raw_win32_keys {
            std::fs::write(cwd.join("raw-win32-keys.enabled"), b"").unwrap();
        }
        let source = cwd.join("source image.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([99, 88, 77, 255]))
            .save(&source)
            .unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "client::attach_input::tests::clipboard_shortcut_probe",
            "--ignored",
            "--nocapture",
        ]);
        command.cwd(&cwd);
        command.env("OLY_STATE_DIR", cwd.join("oly"));
        command.env(
            "OLY_SOCKET_NAME",
            format!("oly-shortcut-{}", uuid::Uuid::new_v4()),
        );
        let mut child = Child(pair.slave.spawn_command(command).unwrap());
        drop(pair.slave);
        let mut writer = pair.master.take_writer().unwrap();
        writer.write_all(b"\x1b[1;1R").unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = mpsc::channel();
        let pump = std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&output).contains("SHORTCUT_READY") {
            assert!(
                Instant::now() < deadline,
                "helper not ready: {:?}",
                String::from_utf8_lossy(&output)
            );
            if let Ok(bytes) = rx.recv_timeout(Duration::from_millis(100)) {
                output.extend(bytes);
            }
        }
        if !raw_win32_keys {
            // Ctrl-V and Shift+Insert in VT input mode. Neither touches the actual
            // clipboard; fake providers exercise the real file/image persistence.
            writer.write_all(b"\x16").unwrap();
            // Native Ctrl+V records may carry no UnicodeChar (e.g. a terminal
            // key binding). Crossterm previously recovered the key from its VK.
            // Windows Terminal's Ctrl+V PasteFromClipboard binding consumes the
            // key-down. Bitmap-only paste emits no text, leaving just key-up.
            writer.write_all(b"\x1b[86;47;0;0;8;1_").unwrap();
            // When key-down does reach oly, key-up must not collect a second PNG.
            writer.write_all(b"\x1b[86;47;22;1;8;1_").unwrap();
            writer.write_all(b"\x1b[86;47;22;0;8;1_").unwrap();
            writer.write_all(b"\x1b[2;2~").unwrap();
            // Shortcut bytes inside explicit text paste must stay payload, not
            // trigger another file/image collection (no clipboard access).
            writer
                .write_all(b"\x1b[200~\x16\x1b[2;2~\x1b[201~")
                .unwrap();
            writer.write_all(b"#").unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "helper did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        while let Ok(bytes) = rx.recv_timeout(Duration::from_millis(100)) {
            output.extend(bytes);
        }
        assert!(
            report.exists(),
            "helper did not save report: {}",
            String::from_utf8_lossy(&output)
        );
        let result: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        let expected = if raw_win32_keys {
            vec!["Char('v')", "Insert"]
        } else {
            vec!["Char('v')", "Char('v')", "Char('v')", "Insert"]
        };
        assert_eq!(
            result["shortcuts"],
            serde_json::json!(expected),
            "clipboard shortcut bypassed handler: {result}"
        );
        assert_eq!(
            result["raw"],
            serde_json::json!([]),
            "clipboard key leaked to child: {result}"
        );
        assert_eq!(
            result["pastes"],
            if raw_win32_keys {
                serde_json::json!(["", ""])
            } else {
                serde_json::json!(["\x16\x1b[2;2~"])
            }
        );
        let paths = result["saved_paths"].as_array().unwrap();
        assert_eq!(paths.len(), expected.len());
        assert_eq!(
            result["frames"],
            serde_json::json!(
                paths
                    .iter()
                    .map(|path| { format!("\x1b[200~{}\x1b[201~", path.as_str().unwrap()) })
                    .collect::<Vec<_>>()
            )
        );
        for path in &paths[..paths.len() - 1] {
            let saved_image = std::path::Path::new(path.as_str().unwrap());
            assert_eq!(
                image::open(saved_image).unwrap().to_rgba8().into_raw(),
                vec![17, 34, 51, 255]
            );
            assert!(saved_image.starts_with(cwd.join("oly/sessions/fixture/files")));
        }
        let saved_file = std::path::Path::new(paths.last().unwrap().as_str().unwrap());
        assert_eq!(
            std::fs::read(saved_file).unwrap(),
            std::fs::read(source).unwrap()
        );
        assert!(saved_file.starts_with(cwd.join("oly/sessions/fixture/files")));
        drop(writer);
        drop(pair.master);
        drop(rx);
        pump.join().unwrap();
        std::fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn empty_and_consecutive_pastes() {
        assert_eq!(
            decode("\x1b[200~\x1b[201~\x1b[200~x\x1b[201~"),
            vec![
                DecodedInput::Paste(String::new()),
                DecodedInput::Paste("x".into())
            ]
        );
    }

    #[test]
    fn utf16_surrogates_are_joined_without_losing_bmp_text() {
        let mut surrogate = None;
        assert!(decode_utf16_unit(&mut surrogate, 0xd83d).is_empty());
        assert_eq!(decode_utf16_unit(&mut surrogate, 0xde00), vec!['😀']);
        assert_eq!(decode_utf16_unit(&mut surrogate, '中' as u16), vec!['中']);
    }
}
