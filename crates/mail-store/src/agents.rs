//! Agent sessions and their transcripts (spec §9, §10.5), so a
//! conversation can be read again and continued after a relaunch.

use mail_domain::Millis;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::StoreResult;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentSessionRow {
    /// OpenAGC's session id.
    pub uuid: String,
    pub provider: String,
    /// The provider's id for resuming.
    pub external_id: Option<String>,
    pub started_at: Millis,
    pub ended_at: Option<Millis>,
    pub prompt_count: u32,
    pub cost_usd: Option<f64>,
    /// The first prompt, as a title.
    pub title: String,
}

/// One transcript line: `role` is `user` (a prompt) or `agent` (an event),
/// `content_json` the prompt text or the serialized event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRow {
    pub role: String,
    pub content_json: String,
}

fn rowid(tx: &Transaction<'_>, uuid: &str) -> StoreResult<Option<i64>> {
    Ok(tx.query_row("SELECT id FROM agent_sessions WHERE uuid = ?1", [uuid], |r| r.get(0)).optional()?)
}

/// Record a session (idempotent: resuming reuses the row).
pub fn start_session(tx: &Transaction<'_>, uuid: &str, provider: &str, now: Millis) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO agent_sessions (uuid, provider, state, started_at) VALUES (?1, ?2, 'active', ?3)
         ON CONFLICT (uuid) DO UPDATE SET state = 'active', ended_at = NULL",
        params![uuid, provider, now],
    )?;
    Ok(())
}

pub fn set_external_id(tx: &Transaction<'_>, uuid: &str, external_id: &str) -> StoreResult<()> {
    tx.execute("UPDATE agent_sessions SET external_id = ?2 WHERE uuid = ?1", params![uuid, external_id])?;
    Ok(())
}

pub fn end_session(tx: &Transaction<'_>, uuid: &str, now: Millis) -> StoreResult<()> {
    tx.execute("UPDATE agent_sessions SET state = 'ended', ended_at = ?2 WHERE uuid = ?1", params![uuid, now])?;
    Ok(())
}

pub fn add_cost(tx: &Transaction<'_>, uuid: &str, cost_usd: f64) -> StoreResult<()> {
    tx.execute(
        "UPDATE agent_sessions SET cost_usd = COALESCE(cost_usd, 0) + ?2 WHERE uuid = ?1",
        params![uuid, cost_usd],
    )?;
    Ok(())
}

/// Append to a session's transcript. A user line also counts a prompt.
pub fn append(tx: &Transaction<'_>, uuid: &str, role: &str, content_json: &str) -> StoreResult<()> {
    let Some(session) = rowid(tx, uuid)? else { return Ok(()) };
    tx.execute(
        "INSERT INTO agent_transcript (session_id, seq, role, content_json)
         VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM agent_transcript WHERE session_id = ?1), ?2, ?3)",
        params![session, role, content_json],
    )?;
    if role == "user" {
        tx.execute("UPDATE agent_sessions SET prompt_count = prompt_count + 1 WHERE id = ?1", [session])?;
    }
    Ok(())
}

/// Recent sessions with at least one prompt, newest first.
pub fn list_sessions(conn: &Connection, limit: u32) -> StoreResult<Vec<AgentSessionRow>> {
    let rows = conn
        .prepare_cached(
            "SELECT s.uuid, s.provider, s.external_id, s.started_at, s.ended_at, s.prompt_count, s.cost_usd,
                    (SELECT content_json FROM agent_transcript t
                      WHERE t.session_id = s.id AND t.role = 'user' ORDER BY seq LIMIT 1)
             FROM agent_sessions s WHERE s.prompt_count > 0
             ORDER BY s.started_at DESC, s.id DESC LIMIT ?1",
        )?
        .query_map([limit], |r| {
            Ok(AgentSessionRow {
                uuid: r.get(0)?,
                provider: r.get(1)?,
                external_id: r.get(2)?,
                started_at: r.get(3)?,
                ended_at: r.get(4)?,
                prompt_count: r.get::<_, i64>(5)?.max(0) as u32,
                cost_usd: r.get(6)?,
                title: r
                    .get::<_, Option<String>>(7)?
                    .and_then(|j| serde_json::from_str::<String>(&j).ok())
                    .unwrap_or_default(),
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

pub fn get_session(conn: &Connection, uuid: &str) -> StoreResult<Option<AgentSessionRow>> {
    Ok(conn
        .query_row(
            "SELECT uuid, provider, external_id, started_at, ended_at, prompt_count, cost_usd FROM agent_sessions
             WHERE uuid = ?1",
            [uuid],
            |r| {
                Ok(AgentSessionRow {
                    uuid: r.get(0)?,
                    provider: r.get(1)?,
                    external_id: r.get(2)?,
                    started_at: r.get(3)?,
                    ended_at: r.get(4)?,
                    prompt_count: r.get::<_, i64>(5)?.max(0) as u32,
                    cost_usd: r.get(6)?,
                    title: String::new(),
                })
            },
        )
        .optional()?)
}

pub fn transcript(conn: &Connection, uuid: &str) -> StoreResult<Vec<TranscriptRow>> {
    Ok(conn
        .prepare_cached(
            "SELECT t.role, t.content_json FROM agent_transcript t JOIN agent_sessions s ON s.id = t.session_id
             WHERE s.uuid = ?1 ORDER BY t.seq",
        )?
        .query_map([uuid], |r| Ok(TranscriptRow { role: r.get(0)?, content_json: r.get(1)? }))?
        .collect::<Result<_, _>>()?)
}

/// One tool call in the audit log (spec §10.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRow {
    pub id: i64,
    pub session_uuid: String,
    pub tool: String,
    pub args_json: String,
    /// `read_only`, `reversible` or `external`.
    pub risk: String,
    /// `allowed`, `denied`, `pending`, `approved`, `rejected`, `expired`,
    /// `done` or `failed`.
    pub state: String,
    pub result_summary: Option<String>,
    pub created_at: Millis,
    pub resolved_at: Option<Millis>,
}

/// Record a tool call; returns its action id.
pub fn record_action(
    tx: &Transaction<'_>,
    uuid: &str,
    tool: &str,
    args_json: &str,
    risk: &str,
    state: &str,
    now: Millis,
) -> StoreResult<Option<i64>> {
    let Some(session) = rowid(tx, uuid)? else { return Ok(None) };
    tx.execute(
        "INSERT INTO agent_actions (session_id, tool, args_json, risk, state, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![session, tool, args_json, risk, state, now],
    )?;
    Ok(Some(tx.last_insert_rowid()))
}

pub fn update_action(
    tx: &Transaction<'_>,
    id: i64,
    state: &str,
    summary: Option<&str>,
    now: Millis,
) -> StoreResult<()> {
    tx.execute(
        "UPDATE agent_actions SET state = ?2, result_summary = COALESCE(?3, result_summary), resolved_at = ?4 WHERE id = ?1",
        params![id, state, summary, now],
    )?;
    Ok(())
}

/// The newest actions first, across sessions.
pub fn list_actions(conn: &Connection, limit: u32) -> StoreResult<Vec<ActionRow>> {
    Ok(conn
        .prepare_cached(
            "SELECT a.id, s.uuid, a.tool, a.args_json, a.risk, a.state, a.result_summary, a.created_at, a.resolved_at
             FROM agent_actions a JOIN agent_sessions s ON s.id = a.session_id ORDER BY a.id DESC LIMIT ?1",
        )?
        .query_map([limit], |r| {
            Ok(ActionRow {
                id: r.get(0)?,
                session_uuid: r.get(1)?,
                tool: r.get(2)?,
                args_json: r.get(3)?,
                risk: r.get(4)?,
                state: r.get(5)?,
                result_summary: r.get(6)?,
                created_at: r.get(7)?,
                resolved_at: r.get(8)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    #[test]
    fn sessions_and_transcripts() {
        let dir = std::env::temp_dir().join(format!("openagc-agents-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&dir.join("mail.sqlite")).unwrap();
        db.write_blocking(|tx| {
            start_session(tx, "a-1", "claude-code", 10)?;
            start_session(tx, "a-2", "codex", 20)?;
            append(tx, "a-1", "user", "\"What needs a reply?\"")?;
            append(tx, "a-1", "agent", r#"{"type":"text_delta","text":"Two threads."}"#)?;
            set_external_id(tx, "a-1", "sess-9")?;
            add_cost(tx, "a-1", 0.01)?;
            add_cost(tx, "a-1", 0.02)?;
            end_session(tx, "a-1", 30)?;
            append(tx, "missing", "user", "\"ignored\"")
        })
        .unwrap();
        let sessions = db.read_blocking(|c| list_sessions(c, 10)).unwrap();
        assert_eq!(sessions.len(), 1, "sessions without prompts are not listed");
        let s = &sessions[0];
        assert_eq!((s.uuid.as_str(), s.title.as_str(), s.prompt_count), ("a-1", "What needs a reply?", 1));
        assert_eq!(s.external_id.as_deref(), Some("sess-9"));
        assert!((s.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(s.ended_at, Some(30));
        let t = db.read_blocking(|c| transcript(c, "a-1")).unwrap();
        assert_eq!(t.iter().map(|r| r.role.as_str()).collect::<Vec<_>>(), vec!["user", "agent"]);
        // Resuming reopens the same row.
        db.write_blocking(|tx| start_session(tx, "a-1", "claude-code", 40)).unwrap();
        let again = db.read_blocking(|c| get_session(c, "a-1")).unwrap().unwrap();
        assert_eq!((again.ended_at, again.started_at), (None, 10));

        let id = db
            .write_blocking(|tx| record_action(tx, "a-1", "mail_send", r#"{"draft_id":3}"#, "external", "pending", 50))
            .unwrap()
            .unwrap();
        db.write_blocking(move |tx| update_action(tx, id, "done", Some("Sent"), 60)).unwrap();
        let actions = db.read_blocking(|c| list_actions(c, 10)).unwrap();
        assert_eq!(actions[0].state, "done");
        assert_eq!(actions[0].result_summary.as_deref(), Some("Sent"));
        assert_eq!((actions[0].session_uuid.as_str(), actions[0].resolved_at), ("a-1", Some(60)));
        assert!(
            db.write_blocking(|tx| record_action(tx, "nope", "x", "{}", "read_only", "allowed", 1)).unwrap().is_none()
        );
    }
}
