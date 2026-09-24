//! Finding and running agent CLIs (spec §9.2). Shared by the adapters;
//! nothing here knows a CLI flag.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::process::Command;

/// Where to look for a CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Locator {
    /// A path the user chose in Settings; used as-is when set.
    pub override_path: Option<PathBuf>,
    /// Directories searched in order.
    pub search_dirs: Vec<PathBuf>,
}

impl Locator {
    /// The app's default: the login shell's `PATH` (a GUI app does not
    /// inherit it), then the usual install locations.
    pub fn standard() -> Self {
        let mut dirs: Vec<PathBuf> = login_shell_path().to_vec();
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(Path::new(&home).join(".local/bin"));
            dirs.push(Path::new(&home).join(".claude/local"));
        }
        dirs.push("/opt/homebrew/bin".into());
        dirs.push("/usr/local/bin".into());
        let mut seen = std::collections::HashSet::new();
        dirs.retain(|d| seen.insert(d.clone()));
        Self { override_path: None, search_dirs: dirs }
    }

    /// Only these directories: for tests, so a real CLI is never found.
    pub fn only(dirs: Vec<PathBuf>) -> Self {
        Self { override_path: None, search_dirs: dirs }
    }

    pub fn find(&self, name: &str) -> Option<PathBuf> {
        if let Some(p) = &self.override_path {
            return is_executable(p).then(|| p.clone());
        }
        self.search_dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p))
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The user's login-shell `PATH`, resolved once per launch
/// (`/bin/zsh -lc 'echo $PATH'`, 5 s timeout).
pub fn login_shell_path() -> &'static [PathBuf] {
    static PATH: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let output = std::process::Command::new("/bin/zsh")
            .args(["-lc", "echo $PATH"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .trim()
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect(),
            _ => std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default(),
        }
    })
}

/// The `PATH` agent subprocesses get: the login shell's, so the CLI finds
/// its own helpers (node, git).
pub fn child_path() -> String {
    let dirs = login_shell_path();
    if dirs.is_empty() {
        return std::env::var("PATH").unwrap_or_default();
    }
    std::env::join_paths(dirs).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `program args` with no stdin, killing it after `timeout`.
pub async fn run(
    program: &Path,
    args: &[&str],
    env: &[(String, String)],
    timeout: Duration,
) -> Result<RunOutput, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env("PATH", child_path())
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("could not run {}: {e}", program.display()))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => Ok(RunOutput {
            success: o.status.success(),
            code: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("{} did not answer within {}s", program.display(), timeout.as_secs())),
    }
}

/// The first `x.y.z` in `text`, e.g. from "2.1.34 (Claude Code)" or
/// "codex-cli 0.145.0".
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.')).find_map(|word| {
        let mut parts = word.split('.').map(|p| p.parse::<u64>().ok());
        let (a, b, c) = (parts.next()??, parts.next()??, parts.next().flatten().unwrap_or(0));
        Some((a, b, c))
    })
}

pub fn version_string(v: (u64, u64, u64)) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("2.1.34 (Claude Code)"), Some((2, 1, 34)));
        assert_eq!(parse_version("codex-cli 0.145.0"), Some((0, 145, 0)));
        assert_eq!(parse_version("v1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("no version"), None);
        assert!((2, 1, 0) <= parse_version("2.1.34").unwrap());
    }

    #[test]
    fn locator_only_finds_executables_where_told() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("oagc-locator-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("mytool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        let loc = Locator::only(vec![dir.clone()]);
        assert_eq!(loc.find("mytool"), None, "not executable yet");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(loc.find("mytool"), Some(tool.clone()));
        assert_eq!(loc.find("other"), None);
        let over = Locator { override_path: Some(tool.clone()), search_dirs: vec![] };
        assert_eq!(over.find("anything"), Some(tool));
    }

    #[tokio::test]
    async fn run_times_out_and_captures_output() {
        let out = run(Path::new("/bin/echo"), &["hi"], &[], Duration::from_secs(5)).await.unwrap();
        assert!(out.success);
        assert_eq!(out.stdout.trim(), "hi");
        let slow = run(Path::new("/bin/sleep"), &["5"], &[], Duration::from_millis(100)).await;
        assert!(slow.unwrap_err().contains("did not answer"));
    }
}
