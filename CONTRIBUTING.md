# Contributing to OpenAGC

Thanks for helping. This file covers how to build, test and land changes.
The architecture and the reasoning behind it are in
[docs/SPECIFICATION.md](docs/SPECIFICATION.md); significant decisions are
recorded as ADRs in [docs/adr/](docs/adr/).

## Setup

```bash
./scripts/bootstrap.sh
```

This installs Rust (via Homebrew `rustup`, version pinned by
`rust-toolchain.toml`) and XcodeGen, and checks that Xcode 27 is installed
and selected. If `cargo` is not found afterwards, add this to your shell
profile:

```bash
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
```

If you connect a real Gmail account to a development build, also run

```bash
./scripts/dev-signing.sh
```

once. It creates a self-signed "OpenAGC Dev" identity in your login
Keychain and points Debug builds at it (`macos/Local.xcconfig`, gitignored).
Without it every rebuild is a new ad-hoc identity, and the Keychain stops
handing the stored sign-in to the new binary, so the app asks you to sign in
again after each build.

## Layout

| Path | What |
|---|---|
| `crates/` | Rust core, one crate per concern (spec §3) |
| `macos/` | The SwiftUI/AppKit app, generated with XcodeGen from `project.yml` |
| `scripts/` | Build and release scripts |
| `xtask/` | Repository automation (`cargo xtask …`) |
| `docs/` | Specification, ADRs, security and MCP reference |

Crates depend on each other in one direction only: `mail-domain ← store ←
sync ← core`, and providers/agent adapters depend only on their `*-api`
crate. `cargo xtask check-deps` enforces this, and only `openagc-core` may
depend on UniFFI.

## Checks

Everything CI runs, locally:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo xtask check-deps
cargo deny check
```

## Security-sensitive code

Changes under `crates/mail-mime/` (HTML sanitization), `crates/permissions/`,
`crates/agent-mcp/` and `crates/openagc-core/src/agents/` must keep the
golden-file and injection test suites passing, and should add a test for the
case being changed. Email content is untrusted input everywhere, including in
agent prompts and logs. [docs/security.md](docs/security.md) lists each
control and where it lives.

## Test fixtures

Every MIME parser bug fix adds a message to `crates/mail-mime/fixtures/`
that reproduces it. Fixtures must be synthetic or scrubbed: no real
addresses, names or content.

Tests never touch real accounts. Gmail is `provider_api::fake::FakeProvider`
(or `wiremock` for the REST client); agents are `agent_api::fake::FakeAgent`
or fake `claude`/`codex` scripts under each adapter's `tests/`; cloud routines
use `crates/agent-claude/tests/fake_claude_routines.py`. The Swift tests run
with scripted agents automatically, and in that mode the core cannot reach a
real agent CLI. Performance numbers come from the synthetic fixture
(`cargo xtask fixture`, `cargo xtask perf`).

## Generated files

`docs/mcp.md` is rendered from the tool catalog by `cargo xtask mcp-docs`
(the gate checks it), `docs/keyboard.md` from
`macos/OpenAGC/App/KeyboardShortcuts.swift`, and the prompt snapshots under
`crates/agent-api/src/routines/snapshots/` by insta (`INSTA_UPDATE=always
cargo test -p agent-api` after reviewing the change).

## Issue tracking

Work is tracked with [beads](https://github.com/gastownhall/beads) (`bd`).
The dependency graph mirrors the milestones in spec §19.

```bash
bd ready                 # unblocked work
bd show <id>             # details; descriptions cite spec sections
bd update <id> --claim   # take it
bd close <id> --reason "what was done and how it was verified"
```

## Commits

Small, focused commits with a message that says what changed and why. Reference
the beads issue (`Closes oagc-xxx`) when a commit completes one.
