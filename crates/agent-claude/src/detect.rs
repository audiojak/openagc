//! Is Claude Code installed, new enough and signed in? (spec §9.2)
//!
//! There is no auth status command, so a minimal headless turn probes it:
//! no tools, no MCP servers, one turn. The credentials themselves are never
//! read.

use std::time::Duration;

use agent_api::AgentStatus;
use agent_api::process::{Locator, parse_version, run, version_string};

pub const MINIMUM_VERSION: (u64, u64, u64) = (2, 1, 0);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

pub fn auth_probe_args() -> Vec<&'static str> {
    vec![
        "-p",
        "ping",
        "--output-format",
        "json",
        "--max-turns",
        "1",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
    ]
}

pub(crate) async fn detect(locator: &Locator) -> AgentStatus {
    let Some(path) = locator.find("claude") else { return AgentStatus::NotInstalled };
    let version = match run(&path, &["--version"], &[], VERSION_TIMEOUT).await {
        Ok(out) if out.success => match parse_version(&out.stdout) {
            Some(v) => v,
            None => return AgentStatus::Error { message: format!("unrecognized version: {}", out.stdout.trim()) },
        },
        Ok(out) => return AgentStatus::Error { message: first_line(&out.stderr, "claude --version failed") },
        Err(message) => return AgentStatus::Error { message },
    };
    let v = version_string(version);
    if version < MINIMUM_VERSION {
        return AgentStatus::UpdateRequired { version: v, minimum: version_string(MINIMUM_VERSION), path };
    }
    match run(&path, &auth_probe_args(), &[], PROBE_TIMEOUT).await {
        Ok(out) => classify_probe(&out.stdout, &out.stderr, out.success, v, path),
        Err(message) => AgentStatus::Error { message },
    }
}

fn first_line(text: &str, fallback: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or(fallback).trim().to_owned()
}

fn looks_unauthenticated(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    ["not_logged_in", "not logged in", "/login", "invalid api key", "authentication", "oauth token"]
        .iter()
        .any(|needle| t.contains(needle))
}

/// Read the probe's JSON result (or, failing that, its exit and stderr).
pub(crate) fn classify_probe(
    stdout: &str,
    stderr: &str,
    success: bool,
    version: String,
    path: std::path::PathBuf,
) -> AgentStatus {
    let json: Option<serde_json::Value> = stdout.lines().rev().find_map(|l| serde_json::from_str(l).ok());
    if let Some(v) = json {
        let is_error =
            v["is_error"].as_bool().unwrap_or(false) || v["subtype"].as_str().is_some_and(|s| s.starts_with("error"));
        if !is_error {
            return AgentStatus::Ready { version, path };
        }
        let text = format!("{} {} {}", v["subtype"], v["result"], v["error"]);
        if looks_unauthenticated(&text) {
            return AgentStatus::NotAuthenticated { version, path };
        }
        return AgentStatus::Error { message: first_line(v["result"].as_str().unwrap_or(&text), "the probe failed") };
    }
    if looks_unauthenticated(stderr) || looks_unauthenticated(stdout) {
        return AgentStatus::NotAuthenticated { version, path };
    }
    if success {
        return AgentStatus::Error { message: "unexpected probe output".into() };
    }
    AgentStatus::Error { message: first_line(stderr, "the probe failed") }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::*;

    /// A fake `claude` that answers `--version` and the probe.
    pub(crate) fn fake_claude(dir: &Path, version: &str, probe_stdout: &str, probe_exit: i32) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("claude");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version} (Claude Code)'; exit 0; fi\n\
             cat <<'JSON'\n{probe_stdout}\nJSON\nexit {probe_exit}\n"
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oagc-claude-detect-{name}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn detection_states() {
        let none = Locator::only(vec![dir("none")]);
        assert_eq!(detect(&none).await, AgentStatus::NotInstalled);

        let d = dir("ready");
        fake_claude(&d, "2.1.34", r#"{"type":"result","subtype":"success","is_error":false,"result":"pong"}"#, 0);
        assert!(
            matches!(detect(&Locator::only(vec![d])).await, AgentStatus::Ready { version, .. } if version == "2.1.34")
        );

        let d = dir("old");
        fake_claude(&d, "1.0.90", "", 0);
        assert!(
            matches!(detect(&Locator::only(vec![d])).await, AgentStatus::UpdateRequired { minimum, .. } if minimum == "2.1.0")
        );

        let d = dir("logged-out");
        fake_claude(
            &d,
            "2.1.34",
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Please run /login"}"#,
            1,
        );
        assert!(matches!(detect(&Locator::only(vec![d])).await, AgentStatus::NotAuthenticated { .. }));
    }

    #[test]
    fn probe_classification() {
        let p = PathBuf::from("/x/claude");
        let v = || "2.1.0".to_owned();
        assert!(matches!(
            classify_probe(
                r#"{"subtype":"error_during_execution","is_error":true,"error":"not_logged_in"}"#,
                "",
                false,
                v(),
                p.clone()
            ),
            AgentStatus::NotAuthenticated { .. }
        ));
        assert!(matches!(
            classify_probe("", "Error: Not logged in", false, v(), p.clone()),
            AgentStatus::NotAuthenticated { .. }
        ));
        assert!(matches!(
            classify_probe(r#"{"is_error":true,"result":"Overloaded"}"#, "", false, v(), p.clone()),
            AgentStatus::Error { message } if message == "Overloaded"
        ));
    }
}
