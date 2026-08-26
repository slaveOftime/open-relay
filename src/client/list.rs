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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ListTarget {
    pub(super) node: Option<String>,
}

pub(super) fn list_targets(args: &ListArgs) -> Vec<ListTarget> {
    let mut nodes = Vec::new();
    for node in &args.node {
        if !nodes.contains(node) {
            nodes.push(node.clone());
        }
    }

    let mut targets = Vec::new();
    if args.node_local || nodes.is_empty() {
        targets.push(ListTarget { node: None });
    }
    targets.extend(
        nodes
            .into_iter()
            .map(|node| ListTarget { node: Some(node) }),
    );
    targets
}

pub async fn run_list(config: &AppConfig, list_args: ListArgs) -> Result<()> {
    let targets = list_targets(&list_args);
    if list_args.follow {
        return super::list_tui::run(config, &list_args, targets).await;
    }

    const CMD_WIDTH: usize = 12;
    const INPUT_WIDTH: usize = 8;
    const TITLE_WIDTH: usize = 12;
    const ARGS_WIDTH: usize = 12;

    let query = build_list_query(&list_args)?;
    let limit = query.limit;
    let mut used_db_fallback = false;

    let multiple_targets = targets.len() > 1;
    let mut sessions = Vec::new();
    let mut total = 0;
    for target in targets {
        let response = match target.node.as_ref() {
            Some(node) => {
                let request = RpcRequest::NodeProxy {
                    node: node.clone(),
                    inner: Box::new(RpcRequest::List {
                        query: query.clone(),
                    }),
                };
                ipc::send_request_checked(config, request).await
            }
            None => {
                ipc::send_request_checked(
                    config,
                    RpcRequest::List {
                        query: query.clone(),
                    },
                )
                .await
            }
        };
        let (mut target_sessions, target_total) = match response {
            Ok(RpcResponse::List { sessions, total }) => (sessions, total),
            Ok(_) => return Err(AppError::Protocol("unexpected response type".to_string())),
            Err(AppError::DaemonUnavailable(_)) | Err(AppError::Protocol(_))
                if target.node.is_none() && !multiple_targets =>
            {
                used_db_fallback = true;
                let db = Database::open(&config.db_file, config.sessions_dir.clone()).await?;
                let total = db.count_summaries(&query).await?;
                let sessions = db.list_summaries(&query).await?;
                (sessions, total)
            }
            Err(error) => return Err(error),
        };
        if let Some(node) = target.node {
            for session in &mut target_sessions {
                session.node = Some(node.clone());
            }
        }
        sessions.extend(target_sessions);
        total += target_total;
    }

    sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    sessions.truncate(limit);
    sessions.reverse();

    if list_args.json {
        let items = sessions
            .iter()
            .map(|session| {
                let mut item = session_json(session);
                if multiple_targets {
                    item["node"] = json!(session.node.as_deref().unwrap_or("local"));
                }
                item
            })
            .collect::<Vec<_>>();
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

    if multiple_targets {
        println!(
            "NODE         ID      STATUS    INPUT    OUTPUT       CMD          AGE    PID    CREATE_AT↓            TITLE        ARGS"
        );
    } else {
        println!(
            "ID      STATUS    INPUT    OUTPUT       CMD          AGE    PID    CREATE_AT↓            TITLE        ARGS"
        );
    }

    for session in sessions {
        if multiple_targets {
            let node = truncate_display_value(session.node.as_deref().unwrap_or("local"), 12);
            print!("{node:<12} ");
        }
        print_session_row(&session, CMD_WIDTH, INPUT_WIDTH, TITLE_WIDTH, ARGS_WIDTH);
    }

    Ok(())
}

fn print_session_row(
    session: &SessionSummary,
    cmd_width: usize,
    input_width: usize,
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

    let age = if session.status == "running" {
        format_age(session.created_at, session.started_at, session.ended_at)
    } else {
        "-".to_string()
    };

    let pid = session
        .pid
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string());
    let created = format_timestamp_local(session.created_at);

    let input = truncate_display_value(input_required_label(session.input_needed), input_width);
    let output = &session.last_total_bytes.to_string();
    println!(
        "{:<7} {:<9} {:<8} {:<12} {:<12} {:<6} {:<6} {:<21} {:<12} {}",
        session.id, session.status, input, output, command, age, pid, created, title, args
    );
}

pub fn format_age(
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
) -> String {
    let age = match (started_at, ended_at) {
        (Some(started), Some(ended)) => ended - started,
        (Some(started), None) => Utc::now() - started,
        (None, Some(ended)) => ended - created_at,
        (None, None) => Utc::now() - created_at,
    };

    if age.num_hours() > 0 {
        format!("{}h", age.num_hours())
    } else if age.num_minutes() > 0 {
        format!("{}m", age.num_minutes())
    } else {
        format!("{}s", age.num_seconds().max(0))
    }
}

fn session_json(session: &SessionSummary) -> Value {
    json!({
        "id": session.id,
        "title": session.title,
        "tags": session.tags,
        "command": session.command,
        "arguments": session.args,
        "current_working_directory": session.cwd,
        "pid": session.pid,
        "status": session.status,
        "created_at": session.created_at.to_rfc3339() ,
        "started_at": session.started_at.map(|dt| dt.to_rfc3339()),
        "ended_at": session.ended_at.map(|dt| dt.to_rfc3339()),
        "input_needed": session.input_needed,
        "last_total_bytes": session.last_total_bytes,
        "last_output_epoch": session.last_output_epoch.map(|dt| dt.to_rfc3339()),
        "rows": session.rows,
        "cols": session.cols,
        "attach_count": session.attach_count,
        "notifications_enabled": session.notifications_enabled,
    })
}

pub(super) fn format_timestamp_local(timestamp: DateTime<Utc>) -> String {
    timestamp
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn input_required_label(input_needed: bool) -> &'static str {
    if input_needed { "required" } else { "-" }
}

pub(super) fn build_list_query(args: &ListArgs) -> Result<ListQuery> {
    let since = parse_datetime_arg(args.since.as_deref(), "since")?;
    let until = parse_datetime_arg(args.until.as_deref(), "until")?;

    Ok(ListQuery {
        search: args.search.as_ref().map(|text| text.trim().to_string()),
        tags: args
            .tags
            .iter()
            .map(|tag| tag.trim())
            .filter(|tag| !tag.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
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
    use super::{build_list_query, input_required_label, list_targets, session_json};
    use crate::{cli::ListArgs, protocol::SessionSummary};
    use chrono::{TimeZone, Utc};

    #[test]
    fn build_list_query_preserves_json_flag_as_output_only_concern() {
        let args = ListArgs {
            search: Some("demo".to_string()),
            tags: vec!["prod".to_string(), " release ".to_string(), " ".to_string()],
            json: true,
            follow: false,
            status: vec![],
            since: None,
            until: None,
            limit: 25,
            node: vec![],
            node_local: false,
        };

        let query = build_list_query(&args).expect("query should build");

        assert_eq!(query.search.as_deref(), Some("demo"));
        assert_eq!(query.tags, vec!["prod".to_string(), "release".to_string()]);
        assert_eq!(query.limit, 25);
        assert!(query.statuses.is_empty());
    }

    #[test]
    fn list_targets_support_local_and_deduplicate_nodes() {
        let args = ListArgs {
            search: None,
            tags: vec![],
            json: false,
            follow: true,
            status: vec![],
            since: None,
            until: None,
            limit: 100,
            node: vec!["worker-a".into(), "worker-b".into(), "worker-a".into()],
            node_local: true,
        };

        let targets = list_targets(&args);
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].node, None);
        assert_eq!(targets[1].node.as_deref(), Some("worker-a"));
        assert_eq!(targets[2].node.as_deref(), Some("worker-b"));
    }

    #[test]
    fn session_json_includes_iso_time_and_input_required_fields() {
        let created_at = Utc.with_ymd_and_hms(2026, 3, 21, 10, 11, 12).unwrap();
        let session = SessionSummary {
            id: "sess-123".to_string(),
            title: Some("demo".to_string()),
            tags: vec!["prod".to_string()],
            command: "cargo".to_string(),
            args: vec!["test".to_string()],
            pid: Some(42),
            status: "running".to_string(),
            created_at,
            started_at: None,
            ended_at: None,
            cwd: Some("C:/work".to_string()),
            input_needed: true,
            notifications_enabled: false,
            node: None,
            last_total_bytes: 4096,
            last_output_epoch: None,
            rows: Some(24),
            cols: Some(80),
            attach_count: 0,
        };

        let value = session_json(&session);
        assert_eq!(
            value["created_at"],
            serde_json::json!(created_at.to_rfc3339())
        );
        assert_eq!(value["input_needed"], serde_json::json!(true));
        assert_eq!(value["last_total_bytes"], serde_json::json!(4096));
    }

    #[test]
    fn input_required_label_is_explicit() {
        assert_eq!(input_required_label(true), "required");
        assert_eq!(input_required_label(false), "-");
    }
}
