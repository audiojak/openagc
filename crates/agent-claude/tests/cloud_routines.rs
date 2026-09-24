//! RemoteTrigger through a fake `claude`: never the user's real CLI.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use agent_api::process::Locator;
use agent_claude::routines::{CloudError, CloudRoutines, create_body};
use serde_json::Value;

fn setup(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oagc-cloud-routines-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_claude_routines.py"), d.join("claude"))
        .unwrap();
    std::fs::set_permissions(d.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    d
}

fn calls(d: &Path) -> Vec<Value> {
    std::fs::read_to_string(d.join("calls.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[tokio::test]
async fn discover_create_update_run_and_read_runs() {
    let d = setup("flow");
    let cloud = CloudRoutines::new(&Locator::only(vec![d.clone()]));
    assert!(cloud.is_available());

    let found = cloud.discover().await.unwrap();
    assert_eq!(found.environment_id.as_deref(), Some("env_42"));
    assert_eq!(found.model.as_deref(), Some("claude-opus-5"), "the user's own model choice is kept");
    assert_eq!(found.gmail_connection.as_ref().unwrap()["connector_uuid"], "conn-1");

    let body = create_body("Sort important mail", "44 * * * *", true, "PROMPT TEXT", &found, "evt-1").unwrap();
    let created = cloud.create(&body).await.unwrap();
    assert_eq!(created.trigger_id, "trig_01NEW");
    assert_eq!(created.url, "https://claude.ai/code/routines/trig_01NEW");
    let sent: Value = serde_json::from_str(&std::fs::read_to_string(d.join("created.json")).unwrap()).unwrap();
    assert_eq!(sent, body, "the exact body reached RemoteTrigger");

    cloud.update("trig_01NEW", &serde_json::json!({ "enabled": false })).await.unwrap();
    let updated: Value = serde_json::from_str(&std::fs::read_to_string(d.join("updated.json")).unwrap()).unwrap();
    assert_eq!(
        (updated["trigger_id"].as_str(), updated["body"]["enabled"].as_bool()),
        (Some("trig_01NEW"), Some(false))
    );

    assert_eq!(cloud.run("trig_01NEW").await.unwrap().as_deref(), Some("session_run_1"));
    let runs = cloud.list_runs("trig_01NEW").await.unwrap();
    assert_eq!((runs[0].session_id.as_str(), runs[0].status.as_str()), ("session_run_1", "completed"));
    assert!(cloud.run_log("session_run_1").await.unwrap().starts_with("Sorted 4 threads"));

    for c in calls(&d) {
        let args: Vec<&str> = c["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
        let after = |f: &str| args.iter().position(|a| *a == f).map(|i| args[i + 1]);
        assert_eq!(after("--tools"), Some("RemoteTrigger"), "no other built-in tools");
        assert_eq!(after("--allowedTools"), Some("RemoteTrigger"));
        assert_eq!(after("--permission-mode"), Some("dontAsk"));
        assert_eq!(after("--mcp-config"), Some(r#"{"mcpServers":{}}"#));
        assert!(args.contains(&"--strict-mcp-config"));
        assert!(c["api_key"].is_null(), "routines use the claude.ai login, never an API key");
    }
}

#[tokio::test]
async fn failures_are_errors_not_guesses() {
    let d = setup("logged-out");
    std::fs::write(d.join("mode"), "logged_out").unwrap();
    let cloud = CloudRoutines::new(&Locator::only(vec![d.clone()]));
    assert!(matches!(cloud.discover().await, Err(CloudError::Cli(m)) if m.contains("/login")));

    let missing = CloudRoutines::new(&Locator::only(vec![setup("missing").join("nowhere")]));
    assert_eq!(missing.discover().await.unwrap_err(), CloudError::NotInstalled);
}
