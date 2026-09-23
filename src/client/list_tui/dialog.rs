// Dialog state types: the small input-control struct (EditText), the two
// modal dialogs (CloneDialog, UpdateDialog), their field enums, and the
// helpers used to round-trip text inputs through the launch payload.
use super::proto::{CloneLaunch, SessionUpdate};
use crate::protocol::SessionSummary;
use crate::session::{MAX_SESSION_TITLE_LEN, normalize_session_tags, normalize_session_title};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloneField {
    Command,
    Args,
    Cwd,
    Title,
    Tags,
    Node,
    Rows,
    Cols,
    DisableNotifications,
    AttachAfterStart,
}

pub const CLONE_FIELDS: [CloneField; 10] = [
    CloneField::Command,
    CloneField::Args,
    CloneField::Cwd,
    CloneField::Title,
    CloneField::Tags,
    CloneField::Node,
    CloneField::Rows,
    CloneField::Cols,
    CloneField::DisableNotifications,
    CloneField::AttachAfterStart,
];

#[derive(Debug, Default, Eq, PartialEq)]
pub struct EditText {
    pub value: String,
    pub cursor: usize,
}

impl EditText {
    pub fn new(value: String) -> Self {
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn byte_index(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor)
            .map_or(self.value.len(), |(index, _)| index)
    }

    pub fn insert(&mut self, character: char) {
        let index = self.byte_index();
        self.value.insert(index, character);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.delete();
    }

    pub fn delete(&mut self) {
        let start = self.byte_index();
        if start == self.value.len() {
            return;
        }
        let end = self.value[start..]
            .char_indices()
            .nth(1)
            .map_or(self.value.len(), |(offset, _)| start + offset);
        self.value.replace_range(start..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.value.chars().count());
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct CloneDialog {
    pub source_id: Option<String>,
    pub active: usize,
    pub command: EditText,
    pub args: EditText,
    pub cwd: EditText,
    pub title: EditText,
    pub tags: EditText,
    pub node: EditText,
    pub rows: EditText,
    pub cols: EditText,
    pub disable_notifications: bool,
    pub attach_after_start: bool,
    pub error: Option<String>,
}

impl CloneDialog {
    pub fn from_session(session: &SessionSummary, list_node: Option<&str>) -> Self {
        Self {
            source_id: Some(session.id.clone()),
            active: 0,
            command: EditText::new(session.command.clone()),
            args: EditText::new(format_terminal_words(&session.args)),
            cwd: EditText::new(session.cwd.clone().unwrap_or_default()),
            title: EditText::new(session.title.clone().unwrap_or_default()),
            tags: EditText::new(format_terminal_words(&session.tags)),
            node: EditText::new(
                session
                    .node
                    .as_deref()
                    .or(list_node)
                    .unwrap_or_default()
                    .to_string(),
            ),
            rows: EditText::new(
                session
                    .rows
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            cols: EditText::new(
                session
                    .cols
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            disable_notifications: !session.notifications_enabled,
            attach_after_start: false,
            error: None,
        }
    }

    pub fn blank(list_node: Option<&str>) -> Self {
        Self {
            source_id: None,
            active: 0,
            command: EditText::new(String::new()),
            args: EditText::new(String::new()),
            cwd: EditText::new(String::new()),
            title: EditText::new(String::new()),
            tags: EditText::new(String::new()),
            // Prefill the node the list is currently scoped to so the new
            // session lands where the user is looking; still editable.
            node: EditText::new(list_node.unwrap_or_default().to_string()),
            rows: EditText::new(String::new()),
            cols: EditText::new(String::new()),
            disable_notifications: false,
            attach_after_start: false,
            error: None,
        }
    }

    pub fn active_field(&self) -> CloneField {
        CLONE_FIELDS[self.active]
    }

    pub fn next(&mut self) {
        self.active = (self.active + 1) % CLONE_FIELDS.len();
        self.error = None;
    }

    pub fn previous(&mut self) {
        self.active = (self.active + CLONE_FIELDS.len() - 1) % CLONE_FIELDS.len();
        self.error = None;
    }

    pub fn active_text_mut(&mut self) -> Option<&mut EditText> {
        match self.active_field() {
            CloneField::Command => Some(&mut self.command),
            CloneField::Args => Some(&mut self.args),
            CloneField::Cwd => Some(&mut self.cwd),
            CloneField::Title => Some(&mut self.title),
            CloneField::Tags => Some(&mut self.tags),
            CloneField::Node => Some(&mut self.node),
            CloneField::Rows => Some(&mut self.rows),
            CloneField::Cols => Some(&mut self.cols),
            CloneField::DisableNotifications | CloneField::AttachAfterStart => None,
        }
    }

    pub fn toggle_active(&mut self) {
        match self.active_field() {
            CloneField::DisableNotifications => {
                self.disable_notifications = !self.disable_notifications
            }
            CloneField::AttachAfterStart => self.attach_after_start = !self.attach_after_start,
            _ => {}
        }
        self.error = None;
    }

    pub fn launch(&self) -> std::result::Result<CloneLaunch, String> {
        if self.command.value.trim().is_empty() {
            return Err("command is required".to_string());
        }
        let args = parse_terminal_words("args", &self.args.value)?;
        let tags = parse_terminal_words("tags", &self.tags.value)?;
        let rows = parse_dimension("rows", &self.rows.value)?;
        let cols = parse_dimension("cols", &self.cols.value)?;
        Ok(CloneLaunch {
            title: optional_text(&self.title.value),
            tags,
            command: self.command.value.clone(),
            args,
            cwd: optional_text(&self.cwd.value),
            node: optional_text(&self.node.value),
            rows,
            cols,
            disable_notifications: self.disable_notifications,
            attach_after_start: self.attach_after_start,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateField {
    Title,
    Tags,
    Notifications,
}

pub const UPDATE_FIELDS: [UpdateField; 3] = [
    UpdateField::Title,
    UpdateField::Tags,
    UpdateField::Notifications,
];

#[derive(Debug)]
pub struct UpdateDialog {
    pub target_id: String,
    pub target_node: Option<String>,
    pub active: usize,
    pub title: EditText,
    pub tags: EditText,
    pub original_title: Option<String>,
    pub original_tags: Vec<String>,
    pub notifications_enabled: bool,
    pub original_notifications_enabled: bool,
    pub summary: SessionSummary,
    pub available: bool,
    pub error: Option<String>,
}

impl UpdateDialog {
    pub fn from_session(session: &SessionSummary, list_node: Option<&str>) -> Self {
        Self {
            target_id: session.id.clone(),
            target_node: session
                .node
                .clone()
                .or_else(|| list_node.map(str::to_string)),
            active: 0,
            title: EditText::new(session.title.clone().unwrap_or_default()),
            tags: EditText::new(format_terminal_words(&session.tags)),
            original_title: session.title.clone(),
            original_tags: session.tags.clone(),
            notifications_enabled: session.notifications_enabled,
            original_notifications_enabled: session.notifications_enabled,
            summary: session.clone(),
            available: true,
            error: None,
        }
    }

    pub fn active_field(&self) -> UpdateField {
        UPDATE_FIELDS[self.active]
    }

    pub fn next(&mut self) {
        self.active = (self.active + 1) % UPDATE_FIELDS.len();
        self.error = None;
    }

    pub fn previous(&mut self) {
        self.active = (self.active + UPDATE_FIELDS.len() - 1) % UPDATE_FIELDS.len();
        self.error = None;
    }

    pub fn active_text_mut(&mut self) -> Option<&mut EditText> {
        match self.active_field() {
            UpdateField::Title => Some(&mut self.title),
            UpdateField::Tags => Some(&mut self.tags),
            UpdateField::Notifications => None,
        }
    }

    pub fn toggle_active(&mut self) {
        if self.active_field() == UpdateField::Notifications {
            self.notifications_enabled = !self.notifications_enabled;
        }
        self.error = None;
    }

    pub fn sync_summary(&mut self, summary: Option<&SessionSummary>) {
        let unavailable_message = format!(
            "session {} is no longer available in the current list",
            self.target_id
        );
        match summary {
            Some(summary) => {
                self.summary = summary.clone();
                self.available = true;
                if self.error.as_deref() == Some(unavailable_message.as_str()) {
                    self.error = None;
                }
            }
            None => {
                self.available = false;
                self.error = Some(unavailable_message);
            }
        }
    }

    pub fn update(&self) -> std::result::Result<SessionUpdate, String> {
        if !self.available {
            return Err(format!(
                "session {} is no longer available in the current list",
                self.target_id
            ));
        }

        let normalized_title = normalize_session_title(Some(self.title.value.clone()));
        if normalized_title
            .as_ref()
            .is_some_and(|title| title.chars().count() > MAX_SESSION_TITLE_LEN)
        {
            return Err(format!(
                "session title is too long (max {MAX_SESSION_TITLE_LEN} characters)"
            ));
        }

        let parsed_tags = parse_terminal_words("tags", &self.tags.value)?;
        let normalized_tags = normalize_session_tags(parsed_tags);
        let title = (normalized_title != self.original_title).then(|| self.title.value.clone());
        let tags = (normalized_tags != self.original_tags).then_some(normalized_tags);
        let notifications_enabled = (self.notifications_enabled
            != self.original_notifications_enabled)
            .then_some(self.notifications_enabled);

        Ok(SessionUpdate {
            id: self.target_id.clone(),
            node: self.target_node.clone(),
            title,
            tags,
            notifications_enabled,
        })
    }
}

pub fn format_terminal_words(words: &[String]) -> String {
    words
        .iter()
        .map(|word| {
            if word.is_empty() {
                return "\"\"".to_string();
            }
            if word
                .chars()
                .all(|character| !character.is_whitespace() && !matches!(character, '\'' | '"'))
            {
                return word.clone();
            }
            let escaped = word
                .chars()
                .flat_map(|character| {
                    if matches!(character, '"' | '\\') {
                        ['\\', character].into_iter().collect::<Vec<_>>()
                    } else {
                        [character].into_iter().collect()
                    }
                })
                .collect::<String>();
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_terminal_words(label: &str, value: &str) -> std::result::Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut quote = None;
    let mut characters = value.chars().peekable();

    while let Some(character) = characters.next() {
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    word.push(character);
                }
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                } else if character == '\\' {
                    match characters.peek().copied() {
                        Some('"' | '\\') => word.push(characters.next().unwrap()),
                        _ => word.push(character),
                    }
                } else {
                    word.push(character);
                }
            }
            Some(_) => unreachable!(),
            None if character.is_whitespace() => {
                if word_started {
                    words.push(std::mem::take(&mut word));
                    word_started = false;
                }
            }
            None if matches!(character, '\'' | '"') => {
                quote = Some(character);
                word_started = true;
            }
            None if character == '\\' => {
                word_started = true;
                match characters.peek().copied() {
                    Some(next) if next.is_whitespace() || matches!(next, '\'' | '"' | '\\') => {
                        word.push(characters.next().unwrap());
                    }
                    _ => word.push(character),
                }
            }
            None => {
                word.push(character);
                word_started = true;
            }
        }
    }

    if quote.is_some() {
        return Err(format!("{label} has an unclosed quote"));
    }
    if word_started {
        words.push(word);
    }
    Ok(words)
}

pub fn parse_dimension(label: &str, value: &str) -> std::result::Result<Option<u16>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|dimension| *dimension > 0)
        .map(Some)
        .ok_or_else(|| format!("{label} must be 1-65535"))
}

pub fn optional_text(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_string())
}
