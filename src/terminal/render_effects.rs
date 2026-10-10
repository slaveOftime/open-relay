//! Renderer-only side effects, captured by the SAME parser and terminal engine.
//! A screen diff cannot represent scrollback or buffer switches. This adapter
//! delegates all terminal semantics to alacritty, collecting rows only when the
//! engine actually scrolls them into history (never by matching screen text).
use super::{QueueListener, styled_row};
use alacritty_terminal::{
    grid::Dimensions,
    index::Line,
    term::{Term, TermMode},
    vte::ansi::{self, Handler},
};

#[derive(Debug)]
pub(crate) enum RenderEffect {
    HistoryRow(Vec<u8>),
    AlternateScreen(bool),
    ClearHistory,
}

pub(super) struct RenderHandler<'a> {
    pub term: &'a mut Term<QueueListener>,
    pub effects: &'a mut Vec<RenderEffect>,
}

impl RenderHandler<'_> {
    fn apply(&mut self, operation: impl FnOnce(&mut Term<QueueListener>)) {
        let alt_before = self.term.mode().contains(TermMode::ALT_SCREEN);
        let retained = self.term.screen_lines();
        let before = if alt_before {
            0
        } else {
            self.term.grid().history_size()
        };
        if !alt_before {
            // Give this one operation headroom even when retained history is
            // full. Growth then identifies new rows without text matching or
            // losing rows at the cap. Keep one viewport afterwards for reflow.
            self.term.grid_mut().update_history(before + retained);
        }
        operation(self.term);
        let alt_after = self.term.mode().contains(TermMode::ALT_SCREEN);
        if alt_after != alt_before {
            self.effects.push(RenderEffect::AlternateScreen(alt_after));
        }
        if !alt_after {
            let grid = self.term.grid();
            let added = if alt_before {
                0
            } else {
                grid.history_size().saturating_sub(before)
            };
            for line in -(added as i32)..0 {
                self.effects
                    .push(RenderEffect::HistoryRow(styled_row(&grid[Line(line)])));
            }
            self.term.grid_mut().update_history(retained);
        }
    }
}

// Keeping delegation mechanical is important: no independently implemented
// cursor/wrap/scroll-region semantics, and no second ANSI parser.
macro_rules! forward {
    ($(fn $name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(fn $name(&mut self, $($arg: $ty),*) {
            self.apply(|term| term.$name($($arg),*));
        })*
    };
}

impl Handler for RenderHandler<'_> {
    fn clear_screen(&mut self, mode: ansi::ClearMode) {
        if matches!(mode, ansi::ClearMode::Saved) {
            self.effects.push(RenderEffect::ClearHistory);
        }
        self.apply(|term| term.clear_screen(mode));
    }

    fn reset_state(&mut self) {
        self.effects.push(RenderEffect::ClearHistory);
        self.apply(|term| term.reset_state());
    }

    forward! {
        fn set_title(title: Option<String>);
        fn set_cursor_style(style: Option<ansi::CursorStyle>);
        fn set_cursor_shape(shape: ansi::CursorShape);
        fn input(c: char);
        fn goto(line: i32, col: usize);
        fn goto_line(line: i32);
        fn goto_col(col: usize);
        fn insert_blank(count: usize);
        fn move_up(count: usize);
        fn move_down(count: usize);
        fn identify_terminal(intermediate: Option<char>);
        fn device_status(status: usize);
        fn move_forward(col: usize);
        fn move_backward(col: usize);
        fn move_down_and_cr(row: usize);
        fn move_up_and_cr(row: usize);
        fn put_tab(count: u16);
        fn backspace();
        fn carriage_return();
        fn linefeed();
        fn bell();
        fn substitute();
        fn newline();
        fn set_horizontal_tabstop();
        fn scroll_up(count: usize);
        fn scroll_down(count: usize);
        fn insert_blank_lines(count: usize);
        fn delete_lines(count: usize);
        fn erase_chars(count: usize);
        fn delete_chars(count: usize);
        fn move_backward_tabs(count: u16);
        fn move_forward_tabs(count: u16);
        fn save_cursor_position();
        fn restore_cursor_position();
        fn clear_line(mode: ansi::LineClearMode);
        fn clear_tabs(mode: ansi::TabulationClearMode);
        fn set_tabs(interval: u16);
        fn reverse_index();
        fn terminal_attribute(attr: ansi::Attr);
        fn set_mode(mode: ansi::Mode);
        fn unset_mode(mode: ansi::Mode);
        fn report_mode(mode: ansi::Mode);
        fn set_private_mode(mode: ansi::PrivateMode);
        fn unset_private_mode(mode: ansi::PrivateMode);
        fn report_private_mode(mode: ansi::PrivateMode);
        fn set_scrolling_region(top: usize, bottom: Option<usize>);
        fn set_keypad_application_mode();
        fn unset_keypad_application_mode();
        fn set_active_charset(index: ansi::CharsetIndex);
        fn configure_charset(index: ansi::CharsetIndex, charset: ansi::StandardCharset);
        fn set_color(index: usize, color: ansi::Rgb);
        fn dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        fn reset_color(index: usize);
        fn clipboard_store(clipboard: u8, data: &[u8]);
        fn clipboard_load(clipboard: u8, terminator: &str);
        fn decaln();
        fn push_title();
        fn pop_title();
        fn text_area_size_pixels();
        fn text_area_size_chars();
        fn set_hyperlink(hyperlink: Option<ansi::Hyperlink>);
        fn report_keyboard_mode();
        fn push_keyboard_mode(mode: ansi::KeyboardModes);
        fn pop_keyboard_modes(count: u16);
        fn set_keyboard_mode(mode: ansi::KeyboardModes, behavior: ansi::KeyboardModesApplyBehavior);
        fn set_modify_other_keys(mode: ansi::ModifyOtherKeys);
        fn report_modify_other_keys();
        fn set_scp(path: ansi::ScpCharPath, mode: ansi::ScpUpdateMode);
    }
}
