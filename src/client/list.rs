use chrono::{DateTime, Local, Utc};
use serde_json::{Value, json};

use crate::{
    cli::ListArgs,
    config::AppConfig,
    db::Database,
    error::{AppError, Result},
    ipc,
    protocol::{ListQuery, ListSortField, RpcRequest, RpcResponse, SessionSummary, SortOrder},
};

pub async fn run_list(config: &AppConfig, list_args: ListArgs, node: Option<String>) -> Result<()> {
    const CMD_WIDTH: usize = 12;
    const AGE_WIDTH: usize = 6;
    const INPUT_WIDTH: usize = 8;
    const OUTPUT_WIDTH: usize = 12;
    const TITLE_WIDTH: usize = 12;
    const ARGS_WIDTH: usize = 12;

    let query = build_list_query(&list_args)?;
    let limit = query.limit;
    let mut used_db_fallback = false;

    let (mut sessions, total): (Vec<SessionSummary>, usize) = if let Some(node_name) = node {
        // Remote list via IPC NodeProxy.
        let inner = RpcRequest::List { query };
        let req = RpcRequest::NodeProxy {
            node: node_name,
            inner: Box::new(inner),
        };
        match ipc::send_request_checked(config, req).await? {
            RpcResponse::List { sessions, total } => (sessions, total),
            _ => return Err(AppError::Protocol("unexpected response type".to_string())),
        }
    } else {
        // Daemon handles DB + in-memory overlay; fall back to DB-only when unavailable.
        match ipc::send_request_checked(
            config,
            RpcRequest::List {
                query: query.clone(),
            },
        )
        .await
        {
            Ok(RpcResponse::List { sessions, total }) => (sessions, total),
            Ok(_) => return Err(AppError::Protocol("unexpected response type".to_string())),
            Err(AppError::DaemonUnavailable(_)) | Err(AppError::Protocol(_)) => {
                used_db_fallback = true;
                let db = Database::open(&config.db_file, config.sessions_dir.clone()).await?;
                let total = db.count_summaries(&query).await?;
                let sessions = db.list_summaries(&query).await?;
                (sessions, total)
            }
            Err(e) => return Err(e),
        }
    };

    sessions.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    if list_args.json {
        let items = sessions.iter().map(session_json).collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "items": items,
                "total": total,
                "offset": 0,
                "limit": limit,
            }))?
        );
        if used_db_fallback {
            eprintln!(
                "warning: daemon unavailable; falling back to direct DB access (data may be stale)"
            );
        }
        return Ok(());
    }

    if used_db_fallback {
        println!("⚠️ Daemon unavailable; falling back to direct DB access (data may be stale)");
    }

    if sessions.is_empty() {
        println!("No sessions. Start one with: oly start --detach <cmd>");
        return Ok(());
    }

    println!(
        "ID      STATUS    INPUT    OUTPUT       CMD          AGE    PID    CREATE_AT↓            TITLE        ARGS"
    );

    for session in sessions {
        print_session_row(
            &session,
            CMD_WIDTH,
            AGE_WIDTH,
            INPUT_WIDTH,
            OUTPUT_WIDTH,
            TITLE_WIDTH,
            ARGS_WIDTH,
        );
    }

    Ok(())
}

fn print_session_row(
    session: &SessionSummary,
    cmd_width: usize,
    age_width: usize,
    input_width: usize,
    _output_width: usize,
    title_width: usize,
    args_width: usize,
) {
    let command = truncate_display_value(&session.command, cmd_width);
    let title = truncate_display_value(session.title.as_deref().unwrap_or("-"), title_width);
    let args_text = if session.args.is_empty() {
        "-".to_string()
    } else {
        session.args.join(" ")
    };
    let args = truncate_display_value(&args_text, args_width);
    let age = truncate_display_value(&session.age, age_width);
    let pid = session
        .pid
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string());
    let created = format_created_at_local(session.created_at);
    let input = truncate_display_value(input_required_label(session.input_needed), input_width);
    let output = &session.total_bytes.to_string();
    println!(
        "{:<7} {:<9} {:<8} {:<12} {:<12} {:<6} {:<6} {:<21} {:<12} {}",
        session.id, session.status, input, output, command, age, pid, created, title, args
    );
}

fn session_json(session: &SessionSummary) -> Value {
    json!({
        "id": session.id,
        "title": session.title,
        "tags": session.tags,
        "command": session.command,
        "args": session.args,
        "pid": session.pid,
        "status": session.status,
        "age": session.age,
        "created_at": format_created_at_local(session.created_at),
        "cwd": session.cwd,
        "input_needed": session.input_needed,
        "last_total_bytes": session.total_bytes,
    })
}

fn format_created_at_local(created_at: DateTime<Utc>) -> String {
    created_at
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn input_required_label(input_needed: bool) -> &'static str {
    if input_needed { "required" } else { "-" }
}

fn build_list_query(args: &ListArgs) -> Result<ListQuery> {
    let since = parse_datetime_arg(args.since.as_deref(), "since")?;
    let until = parse_datetime_arg(args.until.as_deref(), "until")?;

    Ok(ListQuery {
        search: args.search.as_ref().map(|text| text.trim().to_string()),
        statuses: args
            .status
            .iter()
            .map(|status| status.as_str().to_string())
            .collect(),
        since,
        until,
        limit: args.limit.max(1),
        offset: 0,
        sort: ListSortField::CreatedAt,
        order: SortOrder::Desc,
    })
}

fn parse_datetime_arg(value: Option<&str>, flag: &str) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = value else {
        return Ok(None);
    };

    let parsed = DateTime::parse_from_rfc3339(value).map_err(|err| {
        AppError::Protocol(format!(
            "invalid --{flag} value `{value}`; expected RFC3339 datetime: {err}"
        ))
    })?;

    Ok(Some(parsed.with_timezone(&Utc)))
}

pub fn truncate_display_value(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if value.chars().count() <= max_width {
        return value.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let mut truncated = value.chars().take(max_width - 1).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::{build_list_query, format_created_at_local, input_required_label, session_json};
    use crate::{cli::ListArgs, protocol::SessionSummary};
    use chrono::{TimeZone, Utc};

    #[test]
    fn build_list_query_preserves_json_flag_as_output_only_concern() {
        let args = ListArgs {
            search: Some("demo".to_string()),
            json: true,
            status: vec![],
            since: None,
            until: None,
            limit: 25,
            node: None,
        };

        let query = build_list_query(&args).expect("query should build");

        assert_eq!(query.search.as_deref(), Some("demo"));
        assert_eq!(query.limit, 25);
        assert!(query.statuses.is_empty());
    }

    #[test]
    fn session_json_includes_formatted_time_and_input_required_fields() {
        let created_at = Utc.with_ymd_and_hms(2026, 3, 21, 10, 11, 12).unwrap();
        let session = SessionSummary {
            id: "sess-123".to_string(),
            title: Some("demo".to_string()),
            tags: vec!["prod".to_string()],
            command: "cargo".to_string(),
            args: vec!["test".to_string()],
            pid: Some(42),
            status: "running".to_string(),
            age: "5m".to_string(),
            created_at,
            cwd: Some("C:/work".to_string()),
            input_needed: true,
            notifications_enabled: false,
            node: None,
            total_bytes: 4096,
        };

        let value = session_json(&session);
        let expected_created_at = format_created_at_local(created_at);

        assert_eq!(value["created_at"], serde_json::json!(expected_created_at));
        assert_eq!(value["input_needed"], serde_json::json!(true));
        assert_eq!(value["last_total_bytes"], serde_json::json!(4096));
    }

    #[test]
    fn input_required_label_is_explicit() {
        assert_eq!(input_required_label(true), "required");
        assert_eq!(input_required_label(false), "-");
    }
}
