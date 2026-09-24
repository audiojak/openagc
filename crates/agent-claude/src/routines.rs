//! Claude cloud routines through the user's own CLI (spec §11.5).
//!
//! There is no public routines API. The user's `claude` CLI has a built-in
//! `RemoteTrigger` tool that manages them with the CLI's own claude.ai
//! login, so OpenAGC never holds a claude.ai credential. Each call spawns
//! `claude -p` with that one tool, no MCP servers and `dontAsk`, asks it to
//! call RemoteTrigger with an exact body, and parses the JSON it returns
//! strictly. The endpoint is internal and undocumented: any surprise is an
//! error, and the app falls back to the paste hand-off.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use agent_api::process::{Locator, child_path};
use serde_json::{Value, json};
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(120);
pub const ROUTINES_PAGE: &str = "https://claude.ai/code/routines";
pub const CONNECTORS_PAGE: &str = "https://claude.ai/customize/connectors";
pub const GMAIL_CONNECTOR_URL: &str = "https://gmailmcp.googleapis.com/mcp/v1";
/// Used when no existing routine shows which model the user prefers.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CloudError {
    #[error("Claude Code is not installed")]
    NotInstalled,
    #[error("Claude Code could not manage routines: {0}")]
    Cli(String),
    #[error("Claude Code returned something unexpected: {0}")]
    Unexpected(String),
    #[error("Connect Gmail at claude.ai first (claude.ai › Customize › Connectors)")]
    NoGmailConnector,
    #[error("No cloud environment was found for routines; create one routine at claude.ai first")]
    NoEnvironment,
}

/// What an existing routine tells us about the account's setup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloudSetup {
    pub environment_id: Option<String>,
    /// The Gmail connector as an existing routine attaches it.
    pub gmail_connection: Option<Value>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedRoutine {
    pub trigger_id: String,
    pub url: String,
    pub next_run_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudRun {
    pub session_id: String,
    pub status: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

pub fn routine_url(trigger_id: &str) -> String {
    format!("{ROUTINES_PAGE}/{trigger_id}")
}

/// A trigger id pasted back by the user, from a URL or on its own.
pub fn trigger_id_from(text: &str) -> Option<String> {
    let t = text.trim().trim_end_matches('/');
    let id = t.rsplit('/').next()?;
    (id.starts_with("trig_") && id.len() > 5 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| id.to_owned())
}

/// The CLI arguments for one RemoteTrigger call. The only place these
/// flags live.
pub fn cli_args(instruction: &str) -> Vec<String> {
    [
        "-p",
        instruction,
        "--output-format",
        "json",
        "--max-turns",
        "4",
        // Only the built-in RemoteTrigger tool is available, and allowed.
        "--tools",
        "RemoteTrigger",
        "--allowedTools",
        "RemoteTrigger",
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
        "--permission-mode",
        "dontAsk",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn instruction(action: &str, trigger_id: Option<&str>, body: Option<&Value>) -> String {
    let mut call = json!({ "action": action });
    if let Some(id) = trigger_id {
        call["trigger_id"] = json!(id);
    }
    if let Some(body) = body {
        call["body"] = body.clone();
    }
    format!(
        "Call the RemoteTrigger tool exactly once with these arguments, then reply with only the raw JSON \
         result of that call and nothing else: {call}"
    )
}

/// The first JSON value in the CLI's final text (it may wrap it in a code
/// fence).
fn extract_json(text: &str) -> Option<Value> {
    let starts = text.char_indices().filter(|(_, c)| *c == '{' || *c == '[').map(|(i, _)| i);
    for start in starts {
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        if let Some(Ok(v)) = stream.next() {
            return Some(v);
        }
    }
    None
}

pub struct CloudRoutines {
    binary: Option<PathBuf>,
}

impl CloudRoutines {
    pub fn new(locator: &Locator) -> Self {
        Self { binary: locator.find("claude") }
    }

    pub fn standard() -> Self {
        Self::new(&Locator::standard())
    }

    pub fn is_available(&self) -> bool {
        self.binary.is_some()
    }

    /// One RemoteTrigger call; the tool's JSON result.
    pub async fn call(
        &self,
        action: &str,
        trigger_id: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Value, CloudError> {
        let binary = self.binary.as_ref().ok_or(CloudError::NotInstalled)?;
        let mut cmd = Command::new(binary);
        cmd.args(cli_args(&instruction(action, trigger_id, body)))
            .env("PATH", child_path())
            // Routines need the claude.ai login, never an API key.
            .env_remove("ANTHROPIC_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Inherited from a parent Claude Code session, these make the CLI
        // wait for that parent (observed while verifying this path).
        for (key, _) in std::env::vars_os() {
            let k = key.to_string_lossy();
            if k.starts_with("CLAUDECODE") || k.starts_with("CLAUDE_CODE_") {
                cmd.env_remove(&key);
            }
        }
        let child = cmd.spawn().map_err(|e| CloudError::Cli(e.to_string()))?;
        let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| CloudError::Cli("no answer within two minutes".into()))?
            .map_err(|e| CloudError::Cli(e.to_string()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let envelope: Value = serde_json::from_str(stdout.trim()).map_err(|_| {
            let err = String::from_utf8_lossy(&out.stderr);
            CloudError::Cli(err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim().to_owned())
        })?;
        if envelope["is_error"].as_bool().unwrap_or(false) {
            return Err(CloudError::Cli(envelope["result"].as_str().unwrap_or("the call failed").to_owned()));
        }
        let text = envelope["result"].as_str().ok_or_else(|| CloudError::Unexpected("no result text".into()))?;
        let value = extract_json(text).ok_or_else(|| CloudError::Unexpected(text.chars().take(200).collect()))?;
        if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
            return Err(CloudError::Cli(err.as_str().map(str::to_owned).unwrap_or_else(|| err.to_string())));
        }
        Ok(value)
    }

    /// The environment, Gmail connector and model the user's existing
    /// routines use.
    pub async fn discover(&self) -> Result<CloudSetup, CloudError> {
        let listed = self.call("list", None, None).await?;
        let items = listed.get("data").or_else(|| listed.get("triggers")).unwrap_or(&listed);
        let mut setup = CloudSetup::default();
        for t in items.as_array().into_iter().flatten() {
            let ccr = &t["job_config"]["ccr"];
            if setup.environment_id.is_none() {
                setup.environment_id = ccr["environment_id"].as_str().map(str::to_owned);
            }
            if setup.model.is_none() {
                setup.model = ccr["session_context"]["model"].as_str().map(str::to_owned);
            }
            if setup.gmail_connection.is_none() {
                setup.gmail_connection = t["mcp_connections"].as_array().into_iter().flatten().find_map(|c| {
                    let name = c["name"].as_str().unwrap_or_default().to_ascii_lowercase();
                    let url = c["url"].as_str().unwrap_or_default();
                    (name.contains("gmail") || url == GMAIL_CONNECTOR_URL).then(|| c.clone())
                });
            }
        }
        Ok(setup)
    }

    pub async fn create(&self, body: &Value) -> Result<CreatedRoutine, CloudError> {
        let v = self.call("create", None, Some(body)).await?;
        let trigger = v.get("trigger").unwrap_or(&v);
        let id = trigger["id"].as_str().filter(|id| id.starts_with("trig_")).ok_or_else(|| {
            CloudError::Unexpected(format!("no routine id in {}", v.to_string().chars().take(200).collect::<String>()))
        })?;
        Ok(CreatedRoutine {
            trigger_id: id.to_owned(),
            url: routine_url(id),
            next_run_at: trigger["next_run_at"].as_str().map(str::to_owned),
        })
    }

    pub async fn update(&self, trigger_id: &str, partial: &Value) -> Result<(), CloudError> {
        self.call("update", Some(trigger_id), Some(partial)).await.map(|_| ())
    }

    /// Fire now; the run's session id when the API gives one.
    pub async fn run(&self, trigger_id: &str) -> Result<Option<String>, CloudError> {
        let v = self.call("run", Some(trigger_id), None).await?;
        Ok(v["session_id"].as_str().or_else(|| v["run"]["session_id"].as_str()).map(str::to_owned))
    }

    pub async fn list_runs(&self, trigger_id: &str) -> Result<Vec<CloudRun>, CloudError> {
        let v = self.call("list_runs", Some(trigger_id), None).await?;
        let items = v.get("data").or_else(|| v.get("runs")).unwrap_or(&v);
        Ok(items
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some(CloudRun {
                    session_id: r["session_id"].as_str().or_else(|| r["id"].as_str())?.to_owned(),
                    status: r["status"].as_str().unwrap_or("unknown").to_owned(),
                    started_at: r["started_at"].as_str().or_else(|| r["created_at"].as_str()).map(str::to_owned),
                    ended_at: r["ended_at"].as_str().or_else(|| r["finished_at"].as_str()).map(str::to_owned),
                })
            })
            .collect())
    }

    /// A run's condensed log, as plain text. It can quote mail the run
    /// read: show it, never feed it to an agent.
    pub async fn run_log(&self, session_id: &str) -> Result<String, CloudError> {
        let body = json!({ "session_id": session_id });
        let v = self.call("get_run_log", None, Some(&body)).await?;
        Ok(match v {
            Value::String(s) => s,
            other => other.get("log").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| other.to_string()),
        })
    }
}

/// The RemoteTrigger `create` body for a routine (spec §11.5). No
/// repository and no built-in tools: the routine needs only Gmail.
pub fn create_body(
    name: &str,
    cron_utc: &str,
    enabled: bool,
    prompt: &str,
    setup: &CloudSetup,
    event_uuid: &str,
) -> Result<Value, CloudError> {
    let environment = setup.environment_id.as_deref().ok_or(CloudError::NoEnvironment)?;
    let gmail = setup.gmail_connection.clone().ok_or(CloudError::NoGmailConnector)?;
    Ok(json!({
        "name": name,
        "cron_expression": cron_utc,
        "enabled": enabled,
        "job_config": {
            "ccr": {
                "environment_id": environment,
                "session_context": {
                    "model": setup.model.as_deref().unwrap_or(DEFAULT_MODEL),
                    "sources": [],
                    "allowed_tools": [],
                },
                "events": [{
                    "data": {
                        "uuid": event_uuid,
                        "session_id": "",
                        "type": "user",
                        "parent_tool_use_id": null,
                        "message": { "role": "user", "content": prompt },
                    }
                }],
            }
        },
        "mcp_connections": [gmail],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_ids_from_pasted_text() {
        assert_eq!(trigger_id_from("https://claude.ai/code/routines/trig_01Abc/"), Some("trig_01Abc".into()));
        assert_eq!(trigger_id_from(" trig_9 "), Some("trig_9".into()));
        assert_eq!(trigger_id_from("https://evil.test/trig_x;rm"), None);
        assert_eq!(trigger_id_from("hello"), None);
    }

    #[test]
    fn json_is_found_inside_prose_and_fences() {
        assert_eq!(extract_json("```json\n{\"id\": \"trig_1\"}\n```").unwrap()["id"], "trig_1");
        assert_eq!(extract_json("Done: [1, 2]").unwrap(), json!([1, 2]));
        assert!(extract_json("nothing").is_none());
    }

    #[test]
    fn the_body_needs_an_environment_and_gmail() {
        let mut setup = CloudSetup::default();
        assert_eq!(create_body("n", "0 * * * *", true, "p", &setup, "u").unwrap_err(), CloudError::NoEnvironment);
        setup.environment_id = Some("env_1".into());
        assert_eq!(create_body("n", "0 * * * *", true, "p", &setup, "u").unwrap_err(), CloudError::NoGmailConnector);
        setup.gmail_connection = Some(json!({ "connector_uuid": "c", "name": "Gmail", "url": GMAIL_CONNECTOR_URL }));
        let body = create_body("Sort", "44 * * * *", true, "the prompt", &setup, "uuid-1").unwrap();
        assert_eq!(body["job_config"]["ccr"]["session_context"]["allowed_tools"], json!([]));
        assert_eq!(body["job_config"]["ccr"]["session_context"]["model"], DEFAULT_MODEL);
        assert_eq!(body["job_config"]["ccr"]["events"][0]["data"]["message"]["role"], "user");
        assert_eq!(body["mcp_connections"][0]["name"], "Gmail");
    }
}
