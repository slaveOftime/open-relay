#[derive(Debug, Clone)]
pub enum NotificationKind {
    InputNeeded,
    StartupRecovery,
    Manual,
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
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NotificationEvent {
    pub kind: NotificationKind,
    pub title: String,
    pub description: String,
    pub body: String,
    pub navigation_url: Option<String>,
    pub session_ids: Vec<String>,
    pub trigger_rule: Option<NotificationTriggerRule>,
    pub trigger_detail: Option<String>,
    pub node: Option<String>,
}

impl NotificationEvent {
    pub fn input_needed_with_trigger(
        session_id: String,
        session_title: Option<String>,
        body: String,
        trigger_rule: NotificationTriggerRule,
        trigger_detail: Option<String>,
    ) -> Self {
        let description = match trigger_rule {
            NotificationTriggerRule::RegexPattern => format!(
                "{} matched a prompt and is waiting for input.",
                session_display_name(&session_id, session_title.as_deref())
            ),
            NotificationTriggerRule::Silence => format!(
                "{} went quiet and may be waiting for input.",
                session_display_name(&session_id, session_title.as_deref())
            ),
            NotificationTriggerRule::LlmCheck => format!(
                "{} looks like it is waiting for input.",
                session_display_name(&session_id, session_title.as_deref())
            ),
            NotificationTriggerRule::OsSignal => format!(
                "{} reported that it needs input.",
                session_display_name(&session_id, session_title.as_deref())
            ),
        };
        Self {
            kind: NotificationKind::InputNeeded,
            title: "Input required".to_string(),
            description,
            body: non_empty_or(
                body,
                "Open the session to review the latest output.".to_string(),
            ),
            navigation_url: Some(format!(
                "{}?mode=attach",
                session_navigation_url(&session_id)
            )),
            session_ids: vec![session_id],
            trigger_rule: Some(trigger_rule),
            trigger_detail,
            node: None,
        }
    }

    pub fn startup_recovery(sessions: &[crate::session::SessionMeta]) -> Self {
        let count = sessions.len();
        let examples = sessions
            .iter()
            .take(3)
            .map(|meta| meta.title.clone().unwrap_or_else(|| meta.id.clone()))
            .collect::<Vec<_>>();

        let body = if count <= 3 {
            format!("Recovered session(s): {}", examples.join(", "))
        } else {
            format!(
                "Recovered session(s): {} (+{} more)",
                examples.join(", "),
                count.saturating_sub(3)
            )
        };

        Self {
            kind: NotificationKind::StartupRecovery,
            title: "Startup recovery".to_string(),
            description: format!("Marked {count} stale session(s) as failed during startup."),
            body,
            navigation_url: (count == 1).then(|| session_navigation_url(&sessions[0].id)),
            session_ids: sessions.iter().map(|s| s.id.clone()).collect(),
            trigger_rule: None,
            trigger_detail: None,
            node: None,
        }
    }

    pub fn manual(
        source_session_id: Option<String>,
        title: String,
        description: Option<String>,
        body: Option<String>,
    ) -> Self {
        let session_ids = source_session_id.into_iter().collect::<Vec<_>>();
        let navigation_url = session_ids
            .first()
            .map(|session_id| format!("{}?mode=attach", session_navigation_url(&session_id)));

        Self {
            kind: NotificationKind::Manual,
            title: title.trim().to_string(),
            description: normalize_optional_text(description),
            body: normalize_optional_text(body),
            navigation_url,
            session_ids,
            trigger_rule: None,
            trigger_detail: None,
            node: None,
        }
    }

    pub fn into_session_event(self, last_total_bytes: u64) -> crate::session::SessionEvent {
        crate::session::SessionEvent::SessionNotification {
            kind: self.kind.as_str().to_string(),
            title: self.title,
            description: self.description,
            body: self.body,
            navigation_url: self.navigation_url,
            session_ids: self.session_ids,
            trigger_rule: self.trigger_rule.map(|rule| rule.as_str().to_string()),
            trigger_detail: self.trigger_detail,
            node: self.node,
            last_total_bytes,
        }
    }

    pub fn rendered_body(&self) -> String {
        match (
            self.description.trim().is_empty(),
            self.body.trim().is_empty(),
        ) {
            (true, true) => String::new(),
            (false, true) => self.description.trim().to_string(),
            (true, false) => self.body.trim().to_string(),
            (false, false) => format!("{}\n{}", self.description.trim(), self.body.trim()),
        }
    }
}

fn session_display_name(session_id: &str, session_title: Option<&str>) -> String {
    session_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(session_id)
        .to_string()
}

fn session_navigation_url(session_id: &str) -> String {
    format!("/session/{session_id}")
}

fn non_empty_or(value: String, fallback: String) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed.to_string()
    }
}

fn normalize_optional_text(value: Option<String>) -> String {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_needed_sets_new_notification_fields() {
        let event = NotificationEvent::input_needed_with_trigger(
            "session-123".to_string(),
            Some("Deploy prod".to_string()),
            "Recent output".to_string(),
            NotificationTriggerRule::RegexPattern,
            Some("(?i)password:".to_string()),
        );

        assert_eq!(event.title, "Input required");
        assert_eq!(
            event.description,
            "Deploy prod matched a prompt and is waiting for input."
        );
        assert_eq!(event.body, "Recent output");
        assert_eq!(
            event.navigation_url.as_deref(),
            Some("/session/session-123?mode=attach")
        );
    }

    #[test]
    fn rendered_body_combines_description_and_body() {
        let event = NotificationEvent {
            kind: NotificationKind::InputNeeded,
            title: "Input required".to_string(),
            description: "Session needs attention.".to_string(),
            body: "Password:".to_string(),
            navigation_url: Some("/session/session-123".to_string()),
            session_ids: vec!["session-123".to_string()],
            trigger_rule: Some(NotificationTriggerRule::RegexPattern),
            trigger_detail: None,
            node: None,
        };

        assert_eq!(event.rendered_body(), "Session needs attention.\nPassword:");
    }

    #[test]
    fn manual_notification_uses_optional_source_session() {
        let event = NotificationEvent::manual(
            Some("session-123".to_string()),
            "Deploy ready".to_string(),
            Some("Build finished".to_string()),
            Some("Open the session for details.".to_string()),
        );

        assert_eq!(event.kind.as_str(), "manual");
        assert_eq!(event.title, "Deploy ready");
        assert_eq!(event.description, "Build finished");
        assert_eq!(event.body, "Open the session for details.");
        assert_eq!(event.session_ids, vec!["session-123".to_string()]);
        assert_eq!(
            event.navigation_url.as_deref(),
            Some("/session/session-123")
        );
    }
}
