//! Is Codex installed, new enough and signed in? (spec §9.2) `codex login
//! status` answers the last question; `~/.codex/auth.json` is never read.

use std::time::Duration;

use agent_api::AgentStatus;
use agent_api::process::{Locator, parse_version, run, version_string};

pub const MINIMUM_VERSION: (u64, u64, u64) = (0, 145, 0);
const TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn detect(locator: &Locator) -> AgentStatus {
    let Some(path) = locator.find("codex") else { return AgentStatus::NotInstalled };
    let version = match run(&path, &["--version"], &[], TIMEOUT).await {
        Ok(out) if out.success => match parse_version(&out.stdout) {
            Some(v) => v,
            None => return AgentStatus::Error { message: format!("unrecognized version: {}", out.stdout.trim()) },
        },
        Ok(out) => return AgentStatus::Error { message: out.stderr.trim().to_owned() },
        Err(message) => return AgentStatus::Error { message },
    };
    let v = version_string(version);
    if version < MINIMUM_VERSION {
        return AgentStatus::UpdateRequired { version: v, minimum: version_string(MINIMUM_VERSION), path };
    }
    match run(&path, &["login", "status"], &[], TIMEOUT).await {
        Ok(out) if out.success => AgentStatus::Ready { version: v, path },
        Ok(out) if out.code == Some(1) => AgentStatus::NotAuthenticated { version: v, path },
        Ok(out) => AgentStatus::Error { message: format!("codex login status failed: {}", out.stderr.trim()) },
        Err(message) => AgentStatus::Error { message },
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::*;

    fn fake_codex(dir: &Path, version: &str, login_exit: i32) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("codex");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'codex-cli {version}'; exit 0; fi\n\
             if [ \"$1\" = \"login\" ]; then echo 'Logged in using ChatGPT'; exit {login_exit}; fi\nexit 2\n"
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oagc-codex-detect-{name}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn detection_states() {
        assert_eq!(detect(&Locator::only(vec![dir("none")])).await, AgentStatus::NotInstalled);
        let d = dir("ready");
        fake_codex(&d, "0.150.2", 0);
        assert!(
            matches!(detect(&Locator::only(vec![d])).await, AgentStatus::Ready { version, .. } if version == "0.150.2")
        );
        let d = dir("out");
        fake_codex(&d, "0.150.2", 1);
        assert!(matches!(detect(&Locator::only(vec![d])).await, AgentStatus::NotAuthenticated { .. }));
        let d = dir("old");
        fake_codex(&d, "0.99.0", 0);
        assert!(matches!(detect(&Locator::only(vec![d])).await, AgentStatus::UpdateRequired { .. }));
    }
}
