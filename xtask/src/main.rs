//! Repository automation. Run with `cargo xtask <command>`.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

mod perf;

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let messages = std::env::args()
        .skip_while(|a| a != "--messages")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(perf::DEFAULT_MESSAGES);
    match cmd.as_str() {
        "check-deps" => check_deps(),
        "fixture" => perf::fixture(&root, messages, std::env::args().any(|a| a == "--force")).map(|_| ()),
        "perf" => perf::perf(&root, messages),
        _ => {
            eprintln!(
                "usage: cargo xtask <command>\n\ncommands:\n  check-deps              enforce the crate dependency direction (spec §3)\n  fixture [--messages N]  build the synthetic performance mailbox (default 100k)\n  perf [--messages N]     measure store operations against §1.3 budgets"
            );
            std::process::exit(2);
        }
    }
}

/// Which internal crates each crate may depend on. Dependency direction
/// follows `domain ← store ← sync ← core`; providers and agent adapters
/// depend only on their `*-api` crate and `mail-domain` (providers may also
/// use `mail-mime` to decode what they fetch).
fn allowed_internal_deps() -> BTreeMap<&'static str, &'static [&'static str]> {
    BTreeMap::from([
        ("mail-domain", &[][..]),
        ("mail-store", &["mail-domain"][..]),
        ("mail-mime", &["mail-domain"][..]),
        ("provider-api", &["mail-domain"][..]),
        ("provider-gmail", &["mail-domain", "mail-mime", "provider-api"][..]),
        ("mail-sync", &["mail-domain", "mail-store", "mail-mime", "provider-api"][..]),
        ("agent-api", &["mail-domain"][..]),
        ("permissions", &["mail-domain"][..]),
        ("agent-claude", &["mail-domain", "agent-api"][..]),
        ("agent-codex", &["mail-domain", "agent-api"][..]),
        ("agent-mcp", &["mail-domain", "agent-api", "permissions"][..]),
        (
            "openagc-core",
            &[
                "mail-domain",
                "mail-store",
                "mail-mime",
                "mail-sync",
                "provider-api",
                "provider-gmail",
                "agent-api",
                "agent-claude",
                "agent-codex",
                "agent-mcp",
                "permissions",
            ][..],
        ),
        ("openagc-mcp", &["mail-domain", "agent-api", "permissions", "agent-mcp"][..]),
        ("uniffi-bindgen-swift", &[][..]),
        ("xtask", &["mail-domain", "mail-store"][..]),
    ])
}

/// Crates allowed to depend on UniFFI directly (spec §3: only the core
/// knows about UniFFI; the bindgen binary is tooling).
const UNIFFI_ALLOWED: &[&str] = &["openagc-core", "uniffi-bindgen-swift"];

fn check_deps() -> Result<()> {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .context("running cargo metadata")?;
    if !out.status.success() {
        bail!("cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let meta: Value = serde_json::from_slice(&out.stdout)?;
    let packages = meta["packages"].as_array().context("no packages")?;
    let members: BTreeSet<String> = packages.iter().filter_map(|p| p["name"].as_str().map(str::to_owned)).collect();
    let allowed = allowed_internal_deps();

    let mut errors = Vec::new();
    for pkg in packages {
        let name = pkg["name"].as_str().unwrap_or_default();
        let Some(permitted) = allowed.get(name) else {
            errors.push(format!(
                "{name}: not listed in xtask allowed_internal_deps; add it with its permitted dependencies"
            ));
            continue;
        };
        for dep in pkg["dependencies"].as_array().into_iter().flatten() {
            let dep_name = dep["name"].as_str().unwrap_or_default();
            let is_dev = dep["kind"].as_str() == Some("dev");
            if members.contains(dep_name) && !is_dev && !permitted.contains(&dep_name) {
                errors.push(format!("{name} must not depend on {dep_name}"));
            }
            if dep_name == "uniffi" && !UNIFFI_ALLOWED.contains(&name) {
                errors.push(format!("{name} must not depend on uniffi; only openagc-core exports to Swift"));
            }
        }
    }

    if errors.is_empty() {
        println!("check-deps: {} crates, dependency direction OK", members.len());
        Ok(())
    } else {
        for e in &errors {
            eprintln!("check-deps: {e}");
        }
        bail!("{} dependency rule violation(s)", errors.len());
    }
}
