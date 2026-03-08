#[derive(Debug, Clone)]
pub enum NotificationKind {
    InputNeeded,
    StartupRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationTriggerRule {
    OsSignal,
    RegexPattern,
    Silence,
    LlmCheck,
}

impl NotificationTriggerRule {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OsSignal => "os_signal",
            Self::RegexPattern => "regex_pattern",
            Self::Silence => "silence",
            Self::LlmCheck => "llm_check",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "os_signal" => Some(Self::OsSignal),
            "regex_pattern" => Some(Self::RegexPattern),
            "silence" => Some(Self::Silence),
            "llm_check" => Some(Self::LlmCheck),
            _ => None,
        }
    }
}

impl NotificationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InputNeeded => "input_needed",
            Self::StartupRecovery => "startup_recovery",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NotificationEvent {
    pub kind: NotificationKind,
    pub summary: String,
    pub body: String,
    pub session_ids: Vec<String>,
    pub trigger_rule: Option<NotificationTriggerRule>,
    pub trigger_detail: Option<String>,
}

impl NotificationEvent {
    pub fn input_needed_with_trigger(
        session_id: String,
        body: String,
        trigger_rule: NotificationTriggerRule,
        trigger_detail: Option<String>,
    ) -> Self {
        Self {
            kind: NotificationKind::InputNeeded,
            summary: format!("input needed [{session_id}]"),
            body,
            session_ids: vec![session_id],
            trigger_rule: Some(trigger_rule),
            trigger_detail,
        }
    }

    #[allow(dead_code)]
    pub fn input_needed(session_id: String, body: String) -> Self {
        Self::input_needed_with_trigger(session_id, body, NotificationTriggerRule::Silence, None)
    }

    pub fn startup_recovery(sessions: &[crate::session::SessionMeta]) -> Self {
        let count = sessions.len();
        let examples = sessions
            .iter()
            .take(3)
            .map(|meta| meta.title.clone().unwrap_or_else(|| meta.id.clone()))
            .collect::<Vec<_>>();

        let body = if count <= 3 {
            format!(
                "Recovered {count} stale session(s) as failed: {}",
                examples.join(", ")
            )
        } else {
            format!(
                "Recovered {count} stale session(s) as failed: {} (+{} more)",
                examples.join(", "),
                count.saturating_sub(3)
            )
        };

        Self {
            kind: NotificationKind::StartupRecovery,
            summary: "stale sessions recovered".to_string(),
            body,
            session_ids: sessions.iter().map(|s| s.id.clone()).collect(),
            trigger_rule: None,
            trigger_detail: None,
        }
    }
}
