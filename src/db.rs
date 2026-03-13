use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};

use crate::{
    error::Result,
    protocol::{ListQuery, PushSubscriptionInput, PushSubscriptionRecord, SessionSummary},
    session::{SessionMeta, SessionStatus, persist::format_age},
};

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

pub struct Database {
    pool: SqlitePool,
    /// Root directory under which each session stores its files as `<sessions_dir>/<id>/`.
    sessions_dir: PathBuf,
}

impl Database {
    fn push_list_filters(qb: &mut sqlx::QueryBuilder<'_, sqlx::Sqlite>, query: &ListQuery) {
        if !query.statuses.is_empty() {
            qb.push(" AND LOWER(status) IN (");
            let mut sep = qb.separated(", ");
            for s in &query.statuses {
                sep.push_bind(s.to_ascii_lowercase());
            }
            qb.push(")");
        }

        if let Some(since) = query.since {
            qb.push(" AND created_at >= ");
            qb.push_bind(since.to_rfc3339());
        }

        if let Some(until) = query.until {
            qb.push(" AND created_at <= ");
            qb.push_bind(until.to_rfc3339());
        }

        if let Some(search) = query.search.as_deref() {
            let needle = format!("%{}%", search.to_ascii_lowercase());
            qb.push(" AND (LOWER(id) LIKE ");
            qb.push_bind(needle.clone());
            qb.push(" OR LOWER(COALESCE(title,'')) LIKE ");
            qb.push_bind(needle.clone());
            qb.push(" OR LOWER(command) LIKE ");
            qb.push_bind(needle.clone());
            qb.push(" OR LOWER(args) LIKE ");
            qb.push_bind(needle);
            qb.push(")");
        }
    }

    pub async fn count_summaries(&self, query: &ListQuery) -> Result<usize> {
        let mut qb = sqlx::QueryBuilder::new("SELECT COUNT(*) AS total FROM sessions WHERE 1=1");
        Self::push_list_filters(&mut qb, query);
        let row = qb.build().fetch_one(&self.pool).await?;
        let total: i64 = row.get("total");
        Ok(total.max(0) as usize)
    }

    /// Open (or create) the SQLite database at `db_path` and run pending migrations.
    /// `sessions_dir` is the root under which session files live (`<sessions_dir>/<id>/`);
    /// it is stored so callers never need to pass it again.
    pub async fn open(db_path: &Path, sessions_dir: PathBuf) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool, sessions_dir })
    }

    /// Insert a newly created session into the database.
    /// The session directory is derived at runtime as `<sessions_dir>/<id>/`.
    pub async fn insert_session(&self, meta: &SessionMeta) -> Result<()> {
        let args = serde_json::to_string(&meta.args)?;
        let status = meta.status.as_str();
        let created_at = meta.created_at.to_rfc3339();
        let started_at = meta.started_at.map(|t| t.to_rfc3339());
        let ended_at = meta.ended_at.map(|t| t.to_rfc3339());

        sqlx::query(
            "INSERT INTO sessions \
             (id, title, command, args, cwd, status, pid, exit_code, created_at, started_at, ended_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )
        .bind(&meta.id)
        .bind(&meta.title)
        .bind(&meta.command)
        .bind(&args)
        .bind(&meta.cwd)
        .bind(status)
        .bind(meta.pid.map(|p| p as i64))
        .bind(meta.exit_code.map(|c| c as i64))
        .bind(&created_at)
        .bind(&started_at)
        .bind(&ended_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Update mutable fields of an existing session row.
    pub async fn update_session(&self, meta: &SessionMeta) -> Result<()> {
        let args = serde_json::to_string(&meta.args)?;
        let status = meta.status.as_str();
        let started_at = meta.started_at.map(|t| t.to_rfc3339());
        let ended_at = meta.ended_at.map(|t| t.to_rfc3339());

        sqlx::query(
            "UPDATE sessions \
             SET title=?1, command=?2, args=?3, cwd=?4, status=?5, pid=?6, \
                 exit_code=?7, started_at=?8, ended_at=?9 \
             WHERE id=?10",
        )
        .bind(&meta.title)
        .bind(&meta.command)
        .bind(&args)
        .bind(&meta.cwd)
        .bind(status)
        .bind(meta.pid.map(|p| p as i64))
        .bind(meta.exit_code.map(|c| c as i64))
        .bind(&started_at)
        .bind(&ended_at)
        .bind(&meta.id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn delete_session(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id=?1")
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Returns `<sessions_dir>/<id>` if the session exists in the database, else `None`.
    pub async fn get_session_dir(&self, id: &str) -> Result<Option<PathBuf>> {
        if self.session_exists(id).await {
            Ok(Some(self.sessions_dir.join(id)))
        } else {
            Ok(None)
        }
    }

    /// Fetch a session's metadata. Returns `None` when the id is not found.
    pub async fn get_session(&self, id: &str) -> Result<Option<SessionMeta>> {
        let row = sqlx::query(
            "SELECT id, title, command, args, cwd, status, pid, exit_code, \
                    created_at, started_at, ended_at \
             FROM sessions WHERE id=?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.as_ref().map(row_to_meta))
    }

    /// Returns `true` when a session with the given `id` exists in the database.
    pub async fn session_exists(&self, id: &str) -> bool {
        sqlx::query("SELECT 1 FROM sessions WHERE id=?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .ok()
            .flatten()
            .is_some()
    }

    /// List all sessions as `SessionSummary` DTOs, applying the `ListQuery` filter.
    pub async fn list_summaries(&self, query: &ListQuery) -> Result<Vec<SessionSummary>> {
        let limit = query.limit.max(1) as i64;
        let offset = query.offset.max(0) as i64;

        let mut qb = sqlx::QueryBuilder::new(
            "SELECT id, title, command, args, cwd, status, pid, exit_code, \
                    created_at, started_at, ended_at \
             FROM sessions WHERE 1=1",
        );

        Self::push_list_filters(&mut qb, query);

        // Sorting
        let sort_field = query.sort.sqlite_order_by();
        let order = query.order.sql();
        qb.push(" ORDER BY ");
        qb.push(sort_field);
        qb.push(" ");
        qb.push(order);
        qb.push(", id ");
        qb.push(order);
        qb.push(" LIMIT ");
        qb.push_bind(limit);
        qb.push(" OFFSET ");
        qb.push_bind(offset);

        let rows = qb.build().fetch_all(&self.pool).await?;

        let summaries: Vec<SessionSummary> = rows
            .iter()
            .map(|r| meta_to_summary(&row_to_meta(r), false))
            .collect();

        Ok(summaries)
    }

    /// Load sessions whose status is included in `statuses`.
    pub async fn load_sessions_with_status(
        &self,
        statuses: &[SessionStatus],
    ) -> Result<Vec<(String, SessionMeta)>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }

        let mut qb = sqlx::QueryBuilder::new(
            "SELECT id, title, command, args, cwd, status, pid, exit_code, \
                    created_at, started_at, ended_at \
             FROM sessions
             WHERE status IN (",
        );

        {
            let mut separated = qb.separated(", ");
            for status in statuses {
                separated.push_bind(status.as_str());
            }
        }

        qb.push(")");

        let rows = qb.build().fetch_all(&self.pool).await?;

        Ok(rows
            .iter()
            .map(|r| {
                let meta = row_to_meta(r);
                (meta.id.clone(), meta)
            })
            .collect())
    }

    #[allow(dead_code)]
    pub async fn list_push_subscriptions(&self) -> Result<Vec<PushSubscriptionRecord>> {
        let rows = sqlx::query(
            "SELECT endpoint, p256dh, auth FROM push_subscriptions ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| PushSubscriptionRecord {
                endpoint: row.get::<String, _>(0),
                p256dh: row.get::<String, _>(1),
                auth: row.get::<String, _>(2),
            })
            .collect())
    }

    pub async fn upsert_push_subscription(&self, sub: &PushSubscriptionInput) -> Result<()> {
        sqlx::query(
            "INSERT INTO push_subscriptions (endpoint, p256dh, auth, created_at, updated_at) \
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
             ON CONFLICT(endpoint) DO UPDATE SET \
                p256dh = excluded.p256dh, \
                auth = excluded.auth, \
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(sub.endpoint.trim())
        .bind(sub.keys.p256dh.trim())
        .bind(sub.keys.auth.trim())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn delete_push_subscription(&self, endpoint: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM push_subscriptions WHERE endpoint = ?1")
            .bind(endpoint)
            .execute(&self.pool)
            .await?;

        Ok(res.rows_affected() > 0)
    }
}

// ---------------------------------------------------------------------------
// Row mapping helpers (columns 0-10: id…ended_at)
// ---------------------------------------------------------------------------

fn row_to_meta(r: &sqlx::sqlite::SqliteRow) -> SessionMeta {
    let id: String = r.get(0);
    let title: Option<String> = r.get(1);
    let command: String = r.get(2);
    let args_json: String = r.get(3);
    let cwd: Option<String> = r.get(4);
    let status_str: String = r.get(5);
    let pid: Option<i64> = r.get(6);
    let exit_code: Option<i64> = r.get(7);
    let created_at_str: String = r.get(8);
    let started_at_str: Option<String> = r.get(9);
    let ended_at_str: Option<String> = r.get(10);

    build_meta(
        id,
        title,
        command,
        args_json,
        cwd,
        status_str,
        pid,
        exit_code,
        created_at_str,
        started_at_str,
        ended_at_str,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_meta(
    id: String,
    title: Option<String>,
    command: String,
    args_json: String,
    cwd: Option<String>,
    status_str: String,
    pid: Option<i64>,
    exit_code: Option<i64>,
    created_at_str: String,
    started_at_str: Option<String>,
    ended_at_str: Option<String>,
) -> SessionMeta {
    let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
    let created_at = parse_dt(&created_at_str).unwrap_or_else(Utc::now);
    let started_at = started_at_str.as_deref().and_then(parse_dt);
    let ended_at = ended_at_str.as_deref().and_then(parse_dt);

    SessionMeta {
        id,
        title,
        command,
        args,
        cwd,
        created_at,
        started_at,
        ended_at,
        status: parse_status(&status_str),
        pid: pid.map(|p| p as u32),
        exit_code: exit_code.map(|c| c as i32),
    }
}

fn parse_status(s: &str) -> SessionStatus {
    match s {
        "created" => SessionStatus::Created,
        "running" => SessionStatus::Running,
        "stopping" => SessionStatus::Stopping,
        "stopped" => SessionStatus::Stopped,
        "killed" => SessionStatus::Killed,
        _ => SessionStatus::Failed,
    }
}

fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

pub fn meta_to_summary(meta: &SessionMeta, input_needed: bool) -> SessionSummary {
    SessionSummary {
        id: meta.id.clone(),
        title: meta.title.clone(),
        command: meta.command.clone(),
        args: meta.args.clone(),
        pid: meta.pid,
        status: meta.status.as_str().to_string(),
        age: format_age(meta.created_at, meta.started_at, meta.ended_at),
        created_at: meta.created_at,
        cwd: meta.cwd.clone(),
        input_needed,
    }
}

// ---------------------------------------------------------------------------
// API key helpers
// ---------------------------------------------------------------------------

/// A named API key record from the database.
#[derive(Debug, Clone)]
pub struct ApiKeyRecord {
    pub name: String,
    pub created_at: Option<DateTime<Utc>>,
}

impl Database {
    /// Insert a new API key with the given Argon2id-hashed value.
    /// Returns an error if a key with that name already exists.
    pub async fn add_api_key(&self, name: &str, api_key_hash: &str) -> Result<()> {
        sqlx::query("INSERT INTO api_keys (name, api_key_hash) VALUES (?1, ?2)")
            .bind(name)
            .bind(api_key_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Return all stored Argon2id hashes — used to validate an incoming key
    /// against any registered key (keys are independent of node names).
    pub async fn list_api_key_hashes(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT api_key_hash FROM api_keys")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>(0)).collect())
    }

    /// Delete an API key by name. Returns `true` if a row was deleted.
    pub async fn delete_api_key(&self, name: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM api_keys WHERE name = ?1")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// List all registered API keys (names + creation timestamps).
    pub async fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>> {
        let rows = sqlx::query("SELECT name, created_at FROM api_keys ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| ApiKeyRecord {
                name: r.get::<String, _>(0),
                created_at: r.get::<Option<String>, _>(1).as_deref().and_then(parse_dt),
            })
            .collect())
    }
}
