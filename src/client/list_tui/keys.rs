// Key routing: the `AppAction` enum consumed by the main input loop,
// the top-level keyboard dispatcher `route_key`, and the per-dialog
// dispatchers that consume keys while a clone or metadata-update dialog
// owns the terminal.
use crossterm::event::{KeyCode, KeyModifiers};

use super::app::App;
use super::dialog::{CloneDialog, EditText, RemoveDialog, UpdateDialog};
use super::proto::{CloneLaunch, SessionTarget, SessionUpdate};
use super::tree::{TreeEntry, ViewMode};

#[derive(Debug, Eq, PartialEq)]
pub enum AppAction {
    None,
    Quit,
    OpenInline,
    Start(CloneLaunch),
    Update(SessionUpdate),
    Stop(SessionTarget),
    /// Force-remove a session from the daemon (matches `oly rm -f <id>`).
    /// The RPC is forceful; the TUI asks for confirmation before dispatch.
    Remove(SessionTarget),
}

pub fn route_key(
    app: &mut App,
    key: crossterm::event::KeyEvent,
    list_node: Option<&str>,
) -> AppAction {
    if matches!(key.code, KeyCode::Char('c' | 'C')) && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return AppAction::Quit;
    }

    if app.clone_dialog.is_some() {
        return route_clone_dialog_key(app, key);
    }
    if app.update_dialog.is_some() {
        return route_update_dialog_key(app, key);
    }
    if app.remove_dialog.is_some() {
        return route_remove_dialog_key(app, key);
    }

    match key.code {
        _ if is_new_session_dialog_key(key) => {
            let mut dialog = CloneDialog::blank(list_node);
            if app.view_mode == ViewMode::Tree
                && let Some(TreeEntry::Folder { node, .. }) = app.tree.visible.get(app.tree.cursor)
            {
                let folder = &app.tree.nodes[*node];
                if let Some(cwd) = &folder.cwd {
                    dialog.cwd = EditText::new(cwd.clone());
                }
                // A folder in a node group belongs on that node, even in an
                // unscoped multi-node list. A scoped list keeps its scope.
                if list_node.is_none() {
                    dialog.node = EditText::new(folder.node.clone().unwrap_or_default());
                }
            }
            app.clone_dialog = Some(dialog);
            AppAction::None
        }
        _ if is_clone_dialog_key(key) => {
            let Some(session) = app.focused_session() else {
                app.set_action_message(Some("no session in focus to clone".to_string()));
                return AppAction::None;
            };
            app.clone_dialog = Some(CloneDialog::from_session(session, list_node));
            AppAction::None
        }
        _ if is_update_dialog_key(key) => {
            let Some(session) = app.focused_session() else {
                app.set_action_message(Some("no session in focus to update".to_string()));
                return AppAction::None;
            };
            app.update_dialog = Some(UpdateDialog::from_session(session, list_node));
            AppAction::None
        }
        KeyCode::Char('g' | 'G') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.toggle_view_mode();
            AppAction::None
        }
        KeyCode::Char('k' | 'K') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let Some(session) = app.focused_session() else {
                app.set_action_message(Some("no session in focus to stop".to_string()));
                return AppAction::None;
            };
            if !matches!(session.status.as_str(), "created" | "running") {
                app.set_action_message(Some(format!(
                    "{} cannot be stopped while {}",
                    session.id, session.status
                )));
                return AppAction::None;
            }
            AppAction::Stop(SessionTarget {
                id: session.id.clone(),
                node: session
                    .node
                    .clone()
                    .or_else(|| list_node.map(str::to_string)),
            })
        }
        KeyCode::Char('r' | 'R') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Require confirmation before force-removing the focused session (`oly rm -f <id>`).
            let Some(session) = app.focused_session() else {
                app.set_action_message(Some("no session in focus to remove".to_string()));
                return AppAction::None;
            };
            let target = SessionTarget {
                id: session.id.clone(),
                node: session
                    .node
                    .clone()
                    .or_else(|| list_node.map(str::to_string)),
            };
            app.remove_dialog = Some(RemoveDialog::new(target));
            AppAction::None
        }
        KeyCode::Char('s' | 'S') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.toggle_status_filter();
            AppAction::None
        }
        KeyCode::Char('o' | 'O') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.cycle_sort_strategy();
            AppAction::None
        }
        KeyCode::Esc => {
            app.clear_filter();
            AppAction::None
        }
        KeyCode::Backspace => {
            app.pop_filter();
            AppAction::None
        }
        KeyCode::Up => {
            match app.view_mode {
                ViewMode::Tree => app.navigate_tree(-1),
                ViewMode::List => app.previous(),
            }
            AppAction::None
        }
        KeyCode::Down => {
            match app.view_mode {
                ViewMode::Tree => app.navigate_tree(1),
                ViewMode::List => app.next(),
            }
            AppAction::None
        }
        KeyCode::Home => {
            match app.view_mode {
                ViewMode::Tree => app.tree_home(),
                ViewMode::List => app.first(),
            }
            AppAction::None
        }
        KeyCode::End => {
            match app.view_mode {
                ViewMode::Tree => app.tree_last(),
                ViewMode::List => app.last(),
            }
            AppAction::None
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.open_selected_terminal(list_node);
            AppAction::None
        }
        KeyCode::Enter => match app.view_mode {
            ViewMode::Tree => app.tree_enter(),
            ViewMode::List => AppAction::OpenInline,
        },
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            app.push_filter(character);
            AppAction::None
        }
        _ => AppAction::None,
    }
}

pub fn route_remove_dialog_key(app: &mut App, key: crossterm::event::KeyEvent) -> AppAction {
    match key.code {
        KeyCode::Enter | KeyCode::Char('y' | 'Y') => app
            .remove_dialog
            .take()
            .map_or(AppAction::None, |dialog| AppAction::Remove(dialog.target)),
        KeyCode::Esc | KeyCode::Char('n' | 'N') => {
            app.remove_dialog = None;
            app.set_action_message(Some("removal cancelled".to_string()));
            AppAction::None
        }
        _ => AppAction::None,
    }
}

pub fn is_clone_dialog_key(key: crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{4}'))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('d' | 'D')))
}

pub fn is_new_session_dialog_key(key: crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{e}'))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('n' | 'N')))
}

pub fn is_update_dialog_key(key: crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{15}'))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('u' | 'U')))
}

pub fn route_clone_dialog_key(app: &mut App, key: crossterm::event::KeyEvent) -> AppAction {
    if key.code == KeyCode::Esc {
        let message = if app
            .clone_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.source_id.is_some())
        {
            "duplicate cancelled"
        } else {
            "new session cancelled"
        };
        app.clone_dialog = None;
        app.set_action_message(Some(message.to_string()));
        return AppAction::None;
    }

    let dialog = app.clone_dialog.as_mut().expect("dialog checked above");
    match key.code {
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::CONTROL) => dialog.previous(),
        KeyCode::Tab => dialog.next(),
        KeyCode::BackTab => dialog.previous(),
        KeyCode::Enter => match dialog.launch() {
            Ok(launch) => return AppAction::Start(launch),
            Err(error) => dialog.error = Some(error),
        },
        KeyCode::Char(' ') if dialog.active_text_mut().is_none() => dialog.toggle_active(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if let Some(field) = dialog.active_text_mut() {
                field.insert(character);
                dialog.error = None;
            }
        }
        KeyCode::Backspace => {
            if let Some(field) = dialog.active_text_mut() {
                field.backspace();
                dialog.error = None;
            }
        }
        KeyCode::Delete => {
            if let Some(field) = dialog.active_text_mut() {
                field.delete();
                dialog.error = None;
            }
        }
        KeyCode::Left => {
            if let Some(field) = dialog.active_text_mut() {
                field.left();
            }
        }
        KeyCode::Right => {
            if let Some(field) = dialog.active_text_mut() {
                field.right();
            }
        }
        KeyCode::Home => {
            if let Some(field) = dialog.active_text_mut() {
                field.cursor = 0;
            }
        }
        KeyCode::End => {
            if let Some(field) = dialog.active_text_mut() {
                field.cursor = field.value.chars().count();
            }
        }
        _ => {}
    }
    AppAction::None
}

pub fn route_update_dialog_key(app: &mut App, key: crossterm::event::KeyEvent) -> AppAction {
    if key.code == KeyCode::Esc {
        app.update_dialog = None;
        app.set_action_message(Some("update cancelled".to_string()));
        return AppAction::None;
    }

    let dialog = app.update_dialog.as_mut().expect("dialog checked above");
    match key.code {
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::CONTROL) => dialog.previous(),
        KeyCode::Tab => dialog.next(),
        KeyCode::BackTab => dialog.previous(),
        KeyCode::Enter => match dialog.update() {
            Ok(update) => return AppAction::Update(update),
            Err(error) => dialog.error = Some(error),
        },
        KeyCode::Char(' ') if dialog.active_text_mut().is_none() => dialog.toggle_active(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if let Some(field) = dialog.active_text_mut() {
                field.insert(character);
                dialog.error = None;
            }
        }
        KeyCode::Backspace => {
            if let Some(field) = dialog.active_text_mut() {
                field.backspace();
                dialog.error = None;
            }
        }
        KeyCode::Delete => {
            if let Some(field) = dialog.active_text_mut() {
                field.delete();
                dialog.error = None;
            }
        }
        KeyCode::Left => {
            if let Some(field) = dialog.active_text_mut() {
                field.left();
            }
        }
        KeyCode::Right => {
            if let Some(field) = dialog.active_text_mut() {
                field.right();
            }
        }
        KeyCode::Home => {
            if let Some(field) = dialog.active_text_mut() {
                field.cursor = 0;
            }
        }
        KeyCode::End => {
            if let Some(field) = dialog.active_text_mut() {
                field.cursor = field.value.chars().count();
            }
        }
        _ => {}
    }
    AppAction::None
}
