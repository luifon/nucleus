//! Nucleus attribution: which agent, venue or reminder a session belongs to
//! (ADR-034). Every source is read-only; labels are copied into
//! `usage.db` and kept after the source forgets them (runs.jsonl keeps 50
//! rows per agent, venue tables keep only the current session per chat).
//!
//! Sources, lowest priority first (a later source overwrites):
//!
//! 1. venue DBs — `discord.db channel_sessions`, `whatsapp.db chat_sessions`,
//!    `chat.db obsidian_chats`, `jobs.db jobs`;
//! 2. `session_index.db indexed_sessions.agent` (ADR-023, long retention);
//! 3. `memory/logs/<agent>/runs.jsonl` (ADR-016);
//! 4. `reminders.db reminder_fires.msg_id = skill-fire:<session>[|…]` →
//!    the reminder id (agent `reminders-fire`).
//!
//! Reminder title, cron and status are copied into `reminders_meta` so
//! a cancelled or deleted reminder keeps its name on the usage surface.

use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Label {
    pub agent: String,
    pub source: &'static str,
    pub reminder_id: Option<i64>,
}

async fn read_pairs(db: &Path, sql: &str) -> Vec<(String, String)> {
    if !db.exists() {
        return vec![];
    }
    let Ok(pool) = crate::db::open_read_only(db).await else {
        return vec![];
    };
    let rows: Vec<(Option<String>, Option<String>)> =
        sqlx::query_as(sql).fetch_all(&pool).await.unwrap_or_default();
    pool.close().await;
    rows.into_iter()
        .filter_map(|(a, b)| Some((a?, b?)))
        .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        .collect()
}

/// `skill-fire:<session>` or `skill-fire:<session>|silent` → session id.
pub fn skill_fire_session(msg_id: &str) -> Option<&str> {
    let rest = msg_id.strip_prefix("skill-fire:")?;
    let sid = rest.split('|').next()?.trim();
    (!sid.is_empty()).then_some(sid)
}

/// Collect session → label from every source, priority applied.
pub async fn collect(workspace_root: &Path) -> HashMap<String, Label> {
    let mem = workspace_root.join("memory");
    let mut out: HashMap<String, Label> = HashMap::new();
    let mut put = |sid: String, agent: String, source: &'static str, reminder_id: Option<i64>| {
        out.insert(sid, Label { agent, source, reminder_id });
    };

    for (db, sql, agent) in [
        ("discord.db", "SELECT session_id, 'x' FROM channel_sessions", "discord"),
        ("whatsapp.db", "SELECT session_id, 'x' FROM chat_sessions", "whatsapp"),
        ("chat.db", "SELECT claude_session_id, 'x' FROM obsidian_chats", "chat"),
        ("jobs.db", "SELECT session_id, 'x' FROM jobs", "jobs"),
    ] {
        for (sid, _) in read_pairs(&mem.join(db), sql).await {
            put(sid, agent.to_string(), "venue-db", None);
        }
    }
    for (sid, agent) in read_pairs(
        &mem.join("session_index.db"),
        "SELECT session_id, agent FROM indexed_sessions",
    )
    .await
    {
        put(sid, agent, "session-index", None);
    }
    for (sid, agent) in crate::session_index::load_agent_map(workspace_root) {
        put(sid, agent, "run-log", None);
    }
    for (msg_id, rid) in read_pairs(
        &mem.join("reminders.db"),
        "SELECT msg_id, CAST(reminder_id AS TEXT) FROM reminder_fires WHERE msg_id LIKE 'skill-fire:%'",
    )
    .await
    {
        if let (Some(sid), Ok(rid)) = (skill_fire_session(&msg_id), rid.parse::<i64>()) {
            put(sid.to_string(), "reminders-fire".to_string(), "reminder-fire", Some(rid));
        }
    }
    out
}

/// Write the labels onto known sessions and refresh `reminders_meta`.
/// Labels are sticky: a session absent from every source keeps its label.
pub async fn apply(pool: &SqlitePool, workspace_root: &Path, labels: &HashMap<String, Label>) -> Result<usize> {
    let mut tx = pool.begin().await?;
    let mut n = 0;
    for (sid, l) in labels {
        let res = sqlx::query(
            "UPDATE sessions SET agent = ?1, label_source = ?2,
                                 reminder_id = COALESCE(?3, reminder_id)
              WHERE session_id = ?4",
        )
        .bind(&l.agent)
        .bind(l.source)
        .bind(l.reminder_id)
        .bind(sid)
        .execute(&mut *tx)
        .await?;
        n += res.rows_affected() as usize;
    }
    tx.commit().await?;

    let db = workspace_root.join("memory/reminders.db");
    if db.exists() {
        if let Ok(rpool) = crate::db::open_read_only(&db).await {
            let rows: Vec<(i64, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> =
                sqlx::query_as(
                    "SELECT id, title, cron, status, created_by, system_prompt, body FROM reminders",
                )
                .fetch_all(&rpool)
                .await
                .unwrap_or_default();
            rpool.close().await;
            let mut tx = pool.begin().await?;
            for (id, title, cron, status, created_by, prompt, body) in rows {
                let title = title
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| prompt.or(body).map(|t| super::records::truncate(&t, 60)));
                sqlx::query(
                    "INSERT INTO reminders_meta (reminder_id, title, cron, status, created_by)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(reminder_id) DO UPDATE SET title = excluded.title,
                       cron = excluded.cron, status = excluded.status, created_by = excluded.created_by",
                )
                .bind(id)
                .bind(title)
                .bind(cron)
                .bind(status)
                .bind(created_by)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_fire_msg_ids() {
        assert_eq!(skill_fire_session("skill-fire:abc-123"), Some("abc-123"));
        assert_eq!(skill_fire_session("skill-fire:abc-123|silent"), Some("abc-123"));
        assert_eq!(skill_fire_session("discord:999"), None);
        assert_eq!(skill_fire_session("skill-fire:"), None);
    }
}
