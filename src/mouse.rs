//! Shared mouse encoding for interactive attach and `oly send mouse:...`.
//!
//! Operator syntax: `mouse:<left|middle|right>[+N]@col,row` or
//! `mouse:wheel[+N|-N][@col,row]`. Counts default to one, are bounded to
//! 1..=1000, and repeat clicks or wheel ticks (not rows). Only wheel accepts
//! negative counts (down); positive is up. Coordinates are 1-based and
//! required for clicks; only wheel defaults to the live viewport center.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

pub const MAX_MOUSE_EVENTS: u32 = 1000;
pub const MOUSE_USAGE: &str = "use mouse:left@col,row, mouse:middle@col,row, mouse:right@col,row, \
    or mouse:wheel; append +N for 1..1000 clicks/ticks (wheel -N scrolls down); \
    1-based @col,row is required for clicks and optional for wheel, e.g. mouse:left+2@2,3";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseInput {
    kind: MouseEventKind,
    repeat: u32,
    position: Option<(u16, u16)>,
}

/// Parse once, independent of live terminal state. All input must be consumed.
pub fn parse_mouse_spec(spec: &str) -> Result<MouseInput, String> {
    let invalid = || MOUSE_USAGE.to_string();
    let (action, position) = match spec.split_once('@') {
        Some((action, coords)) => {
            let (col, row) = coords.split_once(',').ok_or_else(invalid)?;
            let parse_coord = |s: &str| -> Result<u16, String> {
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                s.parse::<u16>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .ok_or_else(invalid)
            };
            (action, Some((parse_coord(col)?, parse_coord(row)?)))
        }
        None => (spec, None),
    };
    let count_start = action.find(['+', '-']).unwrap_or(action.len());
    let (button, count) = action.split_at(count_start);
    let down = count.starts_with('-');
    let repeat = if count.is_empty() {
        1
    } else {
        let digits = &count[1..];
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let n = digits.parse::<u32>().map_err(|_| invalid())?;
        if !(1..=MAX_MOUSE_EVENTS).contains(&n) {
            return Err(invalid());
        }
        n
    };
    let kind = match (button, down) {
        ("left", false) => MouseEventKind::Down(MouseButton::Left),
        ("middle", false) => MouseEventKind::Down(MouseButton::Middle),
        ("right", false) => MouseEventKind::Down(MouseButton::Right),
        ("wheel", false) => MouseEventKind::ScrollUp,
        ("wheel", true) => MouseEventKind::ScrollDown,
        _ => return Err(invalid()),
    };
    if matches!(kind, MouseEventKind::Down(_)) && position.is_none() {
        return Err(format!(
            "mouse:{button} requires @col,row (1-based viewport coordinates), e.g. mouse:{button}@2,3"
        ));
    }
    Ok(MouseInput {
        kind,
        repeat,
        position,
    })
}

impl MouseInput {
    /// Resolve live modes and geometry on the owning daemon before encoding.
    pub fn encode(
        self,
        rows: u16,
        cols: u16,
        mouse_report: bool,
        sgr: bool,
    ) -> Result<Vec<u8>, String> {
        if !mouse_report {
            return Err("mouse reporting is disabled; use the application's keys (for example key:pgup or key:pgdn)".into());
        }
        let (column, row) = self.position.unwrap_or((cols / 2, rows / 2));
        if column >= cols || row >= rows {
            return Err(format!(
                "mouse position is outside the live viewport ({cols} columns, {rows} rows)"
            ));
        }
        let event = MouseEvent {
            kind: self.kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let legacy_error = || {
            "mouse position cannot be represented by legacy X11 encoding (maximum column/row 223); use a smaller position or enable SGR mouse reporting".to_string()
        };
        let mut tick = encode_mouse_event(event, sgr).ok_or_else(legacy_error)?;
        if let MouseEventKind::Down(button) = self.kind {
            tick.extend(
                encode_mouse_event(
                    MouseEvent {
                        kind: MouseEventKind::Up(button),
                        ..event
                    },
                    sgr,
                )
                .ok_or_else(legacy_error)?,
            );
        }
        Ok(tick.repeat(self.repeat as usize))
    }
}

/// Encode one attach event in the child's negotiated encoding: SGR (1006)
/// when enabled, otherwise legacy X11. Legacy motion is dropped, as before.
pub fn encode_mouse_event(mouse: MouseEvent, sgr: bool) -> Option<Vec<u8>> {
    let mut cb: u16 = match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => 0,
        MouseEventKind::Down(MouseButton::Middle) | MouseEventKind::Up(MouseButton::Middle) => 1,
        MouseEventKind::Down(MouseButton::Right) | MouseEventKind::Up(MouseButton::Right) => 2,
        MouseEventKind::Drag(MouseButton::Left) => 32,
        MouseEventKind::Drag(MouseButton::Middle) => 33,
        MouseEventKind::Drag(MouseButton::Right) => 34,
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    if sgr {
        let cx = u32::from(mouse.column) + 1;
        let cy = u32::from(mouse.row) + 1;
        let suffix = if matches!(mouse.kind, MouseEventKind::Up(_)) {
            'm'
        } else {
            'M'
        };
        Some(format!("\x1b[<{cb};{cx};{cy}{suffix}").into_bytes())
    } else {
        if matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Moved) {
            return None;
        }
        let base = if matches!(mouse.kind, MouseEventKind::Up(_)) {
            3
        } else {
            cb
        };
        let cb = u8::try_from(base).ok()?.checked_add(32)?;
        let cx = u8::try_from(mouse.column).ok()?.checked_add(33)?;
        let cy = u8::try_from(mouse.row).ok()?.checked_add(33)?;
        Some(vec![0x1b, b'[', b'M', cb, cx, cy])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(spec: &str, sgr: bool) -> Vec<u8> {
        parse_mouse_spec(spec)
            .unwrap()
            .encode(24, 80, true, sgr)
            .unwrap()
    }

    #[test]
    fn compact_positions_and_click_counts() {
        for (spec, button, count) in [
            ("left@2,3", 0, 1),
            ("middle+2@2,3", 1, 2),
            ("right+3@2,3", 2, 3),
        ] {
            let pair = format!("\x1b[<{button};2;3M\x1b[<{button};2;3m");
            assert_eq!(encode(spec, true), pair.as_bytes().repeat(count));
        }
    }

    #[test]
    fn clicks_require_explicit_positions_even_with_repeat_counts() {
        for button in ["left", "middle", "right"] {
            for count in ["", "+1", "+2", "+1000"] {
                let spec = format!("{button}{count}");
                assert!(
                    parse_mouse_spec(&spec)
                        .unwrap_err()
                        .contains("requires @col,row"),
                    "{spec}"
                );
                assert!(parse_mouse_spec(&format!("{spec}@2,3")).is_ok());
            }
        }
    }

    #[test]
    fn wheel_counts_are_signed_bounded_ticks() {
        assert_eq!(encode("wheel", true), b"\x1b[<64;41;13M");
        assert_eq!(encode("wheel+3", true), b"\x1b[<64;41;13M".repeat(3));
        assert_eq!(encode("wheel-3@2,3", true), b"\x1b[<65;2;3M".repeat(3));
        assert_eq!(encode("wheel+1000", true), b"\x1b[<64;41;13M".repeat(1000));
        assert_eq!(encode("wheel-1000", true), b"\x1b[<65;41;13M".repeat(1000));
        assert_eq!(
            encode("left+1000@41,13", true),
            b"\x1b[<0;41;13M\x1b[<0;41;13m".repeat(1000)
        );
    }

    #[test]
    fn malformed_specs_are_never_partially_accepted() {
        for spec in [
            "",
            " ",
            "LEFT",
            "wheel-up",
            "wheel*3",
            "left+shift",
            "left-drag",
            "move",
            "left(2,3)",
            "left @2,3",
            "left@ 2,3",
            "wheel+0",
            "wheel-0",
            "wheel+1001",
            "wheel-1001",
            "left-1",
            "left+",
            "wheel-",
            "wheel+2junk",
            "wheel+2+3",
            "wheel+4294967296",
            "wheel-2147483648",
            "wheel--3",
            "left@2",
            "left@0,3",
            "left@2,0",
            "left@2,3,4",
            "left@@2,3",
            "left@2,3junk",
            "left@2,3)",
            "left@2,3@4,5",
            "left@2,3+2",
            "left@65536,1",
            "left@+2,3",
        ] {
            assert!(parse_mouse_spec(spec).is_err(), "accepted {spec:?}");
        }
    }

    #[test]
    fn disabled_reporting_and_outside_viewport_fail_explicitly() {
        let input = parse_mouse_spec("left@1,1").unwrap();
        assert!(
            input
                .encode(24, 80, false, true)
                .unwrap_err()
                .contains("mouse reporting is disabled")
        );
        for spec in ["left@81,1", "wheel+2@1,25"] {
            assert!(
                parse_mouse_spec(spec)
                    .unwrap()
                    .encode(24, 80, true, true)
                    .unwrap_err()
                    .contains("outside the live viewport")
            );
        }
        assert!(input.encode(0, 0, true, true).is_err());
        assert_eq!(
            input.encode(1, 1, true, true).unwrap(),
            b"\x1b[<0;1;1M\x1b[<0;1;1m"
        );
    }

    #[test]
    fn legacy_clicks_and_wheels_match_attach() {
        assert_eq!(
            encode("left+2@2,3", false),
            [0x1b, b'[', b'M', 32, 34, 35, 0x1b, b'[', b'M', 35, 34, 35].repeat(2)
        );
        assert_eq!(
            encode("wheel+3", false),
            [0x1b, b'[', b'M', 96, 73, 45].repeat(3)
        );
        assert_eq!(
            encode("wheel-2@2,3", false),
            [0x1b, b'[', b'M', 97, 34, 35].repeat(2)
        );
    }

    #[test]
    fn encoding_handles_coordinate_limits_without_overflow() {
        let at_limit = parse_mouse_spec("wheel@223,223").unwrap();
        assert_eq!(
            at_limit.encode(300, 300, true, false).unwrap(),
            [0x1b, b'[', b'M', 96, 255, 255]
        );
        for spec in ["wheel@224,1", "left@1,224", "left@65535,65535", "wheel"] {
            assert!(
                parse_mouse_spec(spec)
                    .unwrap()
                    .encode(u16::MAX, u16::MAX, true, false)
                    .unwrap_err()
                    .contains("legacy X11")
            );
        }
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: u16::MAX,
            row: u16::MAX,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            encode_mouse_event(event, true).unwrap(),
            b"\x1b[<0;65536;65536M"
        );
        assert!(encode_mouse_event(event, false).is_none());
    }

    #[test]
    fn interactive_legacy_modifiers_are_preserved() {
        let event = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
        };
        assert_eq!(
            encode_mouse_event(event, false).unwrap(),
            [0x1b, b'[', b'M', 120, 33, 33]
        );
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            modifiers: KeyModifiers::CONTROL,
            ..event
        };
        assert_eq!(
            encode_mouse_event(click, false).unwrap(),
            [0x1b, b'[', b'M', 48, 33, 33]
        );
    }
}
