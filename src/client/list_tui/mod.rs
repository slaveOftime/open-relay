pub(super) mod app;
pub(super) mod constants;
pub(super) mod dialog;
pub(super) mod dialog_render;
pub(super) mod effects;
pub(super) mod keys;
pub(super) mod proto;
pub(super) mod refresh;
pub(super) mod runner;
pub(super) mod spawn;
pub(super) mod table;
pub(super) mod terminal;
pub(super) mod tree;
pub(super) mod tree_render;
pub(super) mod view;

#[allow(unused_imports)] // Preserve the documented client::list_tui API.
pub(super) use constants::LIST_WINDOW_TITLE;
pub(crate) use constants::{TITLE_RESTORE_BYTES, TUI_RESTORE_BYTES};
pub(super) use runner::run;

// Keep the original in-module test fixtures stable while the implementation
// is organized into siblings. Production modules import from their owner.
#[cfg(test)]
pub(super) use app::{
    App, OpenedTerminal, RateState, SortStrategy, StatusFilter, is_active_status,
    open_selected_inline, open_session_inline, session_is_active, session_key, session_search_text,
    session_sort_label, sort_sessions,
};
#[cfg(test)]
pub(super) use constants::{
    ANIMATION_REDRAW_INTERVAL, ATTENTION_PULSE_BG, ATTENTION_PULSE_BG_SELECTED, CLONE_DIALOG_HELP,
    COMPACT_SPARKLINE_WIDTH, DIALOG_FIELD_BG, DIALOG_LABEL_WIDTH, INPUT_POLL_INTERVAL,
    RATE_HISTORY_LEN, REDRAW_INTERVAL, REFRESH_INTERVAL, REFRESH_TIMEOUT, REMOVE_DIALOG_HELP,
    RESUME_DIALOG_HELP, SELECTED_ROW_BG, SPARK_BLOCKS, SPARKLINE_WIDTH, STOP_GRACE_SECONDS,
    TITLE_SAVE_BYTES, UPDATE_DIALOG_HELP,
};
#[cfg(test)]
pub(super) use dialog::{
    CLONE_FIELDS, CloneDialog, CloneField, EditText, RESUME_FIELDS, RemoveDialog, ResumeDialog,
    ResumeField, UPDATE_FIELDS, UpdateDialog, UpdateField, format_terminal_words, optional_text,
    parse_dimension, parse_terminal_words,
};
#[cfg(test)]
pub(super) use dialog_render::{
    centered_rect, checkbox, checkbox_spans, clone_cursor_visible, clone_field_line,
    dialog_field_line, dialog_footer, dialog_value_width, display_words, edit_text_viewport,
    format_dialog_timestamp, render_clone_dialog, render_dialog, render_remove_dialog,
    render_resume_dialog, render_update_dialog, section_header, text_value_spans, tip_separator,
    update_field_line, update_read_only_line, update_read_only_values,
};
#[cfg(test)]
pub(super) use effects::{attention_pulse_key, render_effects};
#[cfg(test)]
pub(super) use keys::{
    AppAction, is_clone_dialog_key, is_new_session_dialog_key, is_update_dialog_key,
    route_clone_dialog_key, route_key, route_remove_dialog_key, route_resume_dialog_key,
    route_update_dialog_key,
};
#[cfg(test)]
pub(super) use proto::{CloneLaunch, SessionTarget, SessionUpdate, wrap_node};
#[cfg(test)]
pub(super) use refresh::{
    SessionRefresh, apply_refresh, apply_update_response, drain_pending_events,
    drain_pending_events_with, fetch_sessions, is_transient_terminal_error, panic_payload_message,
    read_terminal_event, read_terminal_event_with, remove_request, remove_sessions,
    set_clone_error, set_update_error, start_clone, stop_session, update_session,
};
#[cfg(test)]
pub(super) use spawn::{
    WindowRect, arrange_window, powershell_encoded_command, session_command, shell_command,
    shell_quote, spawn_session_terminal, terminal_marker, windows_screen_geometry,
};
#[cfg(test)]
pub(super) use table::{
    LayoutMode, aggregate_sparkline_data, aligned_cell, format_bytes, named_color, pad_truncated,
    parse_hex_color, parse_terminal_color, rate_color, scale_hex_component, session_row,
    session_status_style, session_table_alignments, session_table_header, session_table_widths,
    sparkline, status_glyph, status_label, truncate,
};
#[cfg(test)]
pub(super) use terminal::{
    TuiTerminal, enter_list_title, restore_tui_state, wait_for_ctrl_d, write_list_title,
};
#[cfg(test)]
pub(super) use tree::{
    TREE_AUTO_DEPTH, TreeEntry, TreeNode, TreeView, ViewMode, append_tree_node, common_path_prefix,
    ensure_tree_path, walk_tree_branch,
};
#[cfg(test)]
pub(super) use tree_render::{
    TREE_STATUS_WIDTH, blank_line, folder_line, format_tree_start, render_tree, session_line,
    tree_connector,
};
#[cfg(test)]
pub(super) use view::render;

#[cfg(test)]
mod tests;
