use chrono::{DateTime, Local, Utc};

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
    const TITLE_WIDTH: usize = 12;
    const ARGS_WIDTH: usize = 12;

    let query = build_list_query(&list_args)?;

    let mut sessions: Vec<SessionSummary> = if let Some(node_name) = node {
        // Remote list via IPC NodeProxy.
        let inner = RpcRequest::List { query };
        let req = RpcRequest::NodeProxy {
            node: node_name,
            inner: Box::new(inner),
        };
        match ipc::send_request(config, req).await? {
            RpcResponse::List { sessions, .. } => sessions,
            RpcResponse::Error { message } => return Err(AppError::DaemonUnavailable(message)),
            _ => return Err(AppError::Protocol("unexpected response type".to_string())),
        }
    } else {
        // Daemon handles DB + in-memory overlay; fall back to DB-only when unavailable.
        match ipc::send_request(
            config,
            RpcRequest::List {
                query: query.clone(),
            },
        )
        .await
        {
            Ok(RpcResponse::List { sessions, .. }) => sessions,
            Ok(RpcResponse::Error { message }) => return Err(AppError::DaemonUnavailable(message)),
            Ok(_) => return Err(AppError::Protocol("unexpected response type".to_string())),
            Err(AppError::DaemonUnavailable(_)) | Err(AppError::Protocol(_)) => {
                println!(
                    "⚠️ Daemon unavailable; falling back to direct DB access (data may be stale)"
                );
                let db = Database::open(&config.db_file, config.sessions_dir.clone()).await?;
                db.list_summaries(&query).await?
            }
            Err(e) => return Err(e),
        }
    };

    if sessions.is_empty() {
        println!("No sessions. Start one with: oly start --detach <cmd>");
        return Ok(());
    }

    println!(
        "ID      STATUS    CMD          AGE    PID    CREATE_AT↓            TITLE        ARGS"
    );

    sessions.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    for session in sessions {
        print_session_row(&session, CMD_WIDTH, AGE_WIDTH, TITLE_WIDTH, ARGS_WIDTH);
    }

    Ok(())
}

fn print_session_row(
    session: &SessionSummary,
    cmd_width: usize,
    age_width: usize,
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
    let created = session
        .created_at
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let status = if session.input_needed {
        format!("{}!", session.status)
    } else {
        session.status.clone()
    };
    println!(
        "{:<7} {:<9} {:<12} {:<6} {:<6} {:<21} {:<12} {}",
        session.id, status, command, age, pid, created, title, args
    );
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
