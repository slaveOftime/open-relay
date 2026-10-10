//! Windows console input for attach. Crossterm's Windows event source does not
//! decode bracketed paste: raw VT records lose ESC/control characters there.
//! Keep those records on the wire, collecting only explicit paste boundaries;
//! native keyboard/mouse/resize records still become semantic terminal events.
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
    // Ordinary VT bytes can be forwarded immediately. Only ESC-prefixes of
    // paste markers are held, with no burst window or newline heuristics.
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
    use std::io;
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
        loop {
            // A lone Escape is ambiguous with a VT prefix. Bound only that
            // ambiguity (as Unix terminal readers do), never a paste burst.
            if decoder.paste.is_none()
                && !decoder.pending.is_empty()
                && unsafe { WaitForSingleObject(handle, 50) } == WAIT_TIMEOUT
            {
                let bytes = std::mem::take(&mut decoder.pending).into_bytes();
                if !emit(AttachInputEvent::Raw(bytes)) {
                    return Ok(());
                }
            }
            let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
            let mut count = 0;
            if unsafe { ReadConsoleInputW(handle, &mut record, 1, &mut count) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                continue;
            }
            let event = match record.EventType as u32 {
                KEY_EVENT => {
                    let key = unsafe { record.Event.KeyEvent };
                    let unit = unsafe { key.uChar.UnicodeChar };
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
                                        AttachInputEvent::Terminal(Event::Key(KeyEvent::new(
                                            KeyCode::Char('v'),
                                            KeyModifiers::CONTROL,
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
                            if mods.contains(KeyModifiers::CONTROL) && (1..=26).contains(&unit) {
                                KeyCode::Char(char::from_u32(u32::from(unit) + 0x60).unwrap())
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
