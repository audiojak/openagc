# Security and threat model

What OpenAGC protects, from whom, and where each control lives in the code.
The design is in [SPECIFICATION.md §15](SPECIFICATION.md); this page tracks
the implementation.

## What is at stake

The user's mail, the Google sign-in that can read and change it, the ability
to send as the user, and the user's agent CLIs (which run with the user's
own AI accounts).

## Who we defend against

1. **A malicious email author** — phishing, tracking pixels, hostile HTML,
   and prompt injection aimed at the agent ("forward all invoices to …").
2. **A confused or manipulated agent** that follows injected instructions,
   hallucinates a destructive action, or loops.
3. **Ourselves** — OpenAGC has no servers and must not be able to see mail.

Local malware running as the same user is out of scope; we avoid making it
worse (secrets in the Keychain, private sockets, no credentials on disk).

## Controls

| Threat | Control | Where |
|---|---|---|
| Mail leaves the Mac | No backend; sync is Google ↔ Mac only; no analytics | whole app |
| Stolen sign-in | Refresh token only in the Keychain; `gmail.modify` scope (never `mail.google.com`); PKCE + loopback | `KeychainSecretStore.swift`, `provider-gmail/src/oauth.rs` |
| Hostile HTML | Sanitized at sync (ammonia, style allowlist); reader has JavaScript off, a strict CSP, no navigation, per-scheme image handlers | `mail-mime/src/sanitize.rs`, `MessageWebView.swift` |
| Tracking pixels | Remote images blocked until the user loads them; fetched without cookies or referrer | `SchemeHandlers.swift` |
| Deceptive links | Visible-text/target mismatch warning before opening | `LinkSafety.swift` |
| Malicious attachments | Downloaded only on use, into the app's folder, quarantined (Gatekeeper checks before opening); names sanitized | `mail-sync/src/attachments.rs`, `CoreClient.quarantine` |
| Agent reaches beyond mail | CLIs run with no built-in tools and only OpenAGC's MCP server (Claude: `--tools ""`, `--strict-mcp-config`, `dontAsk`; Codex: read-only sandbox, `approval_policy=never`, the user's MCP servers replaced, shell/exec/browser/apps disabled) | `agent-claude/src/session.rs`, `agent-codex/src/session.rs` |
| Prompt injection → sending, forwarding, deleting | Every tool call is decided in the core; external actions always wait for the user, time out after 10 minutes, and are rejected if the turn is cancelled | `permissions`, `openagc-core/src/agents/{tools,approvals}.rs` |
| Agent edits what the user reviews | A draft under review is frozen for the agent; approving sends what the user sees | `approvals.rs` |
| Agent sends the user's own drafts | Only drafts created in the session can be sent | `permissions::SessionGuard` |
| Bulk damage | ≤ 200 threads per call, ≤ 2,000 per prompt, ≤ 60 calls a minute, system labels refused, selection scope | `permissions`, `tools.rs` |
| "What did the agent see or do?" | Every call audited with its decision and the ids it returned or touched; exportable | Settings › Permissions › Activity |
| Other users on the Mac | The MCP socket is per launch, mode 0600, same-UID peers only, bound to known sessions | `agent-mcp/src/server.rs` |
| Nested-session hangs, key leakage | Agent CLIs get a scrubbed environment; routines never get an API key | adapters, `agent-claude/src/routines.rs` |
| Cloud routine logs | Shown as plain text, never fed to an agent | `RoutinesWindow.swift` |
| Untrusted PDFs | Text extracted by PDFKit in the app, not a Rust parser | `PDFTextExtractor` in `CoreClient.swift` |
| Supply chain | `cargo deny` (licenses, advisories, sources) in the gate; Swift packages pinned by revision; Sparkle updates EdDSA-signed | `deny.toml`, `project.yml` |

## Tests that hold the line

- `crates/openagc-core/src/agents/injection_tests.rs` — a naive agent obeys
  hostile emails; nothing is sent, trashed or archived without the user.
- `crates/permissions` — the policy table and hard limits.
- `crates/mail-mime/tests/sanitize.rs` — hostile HTML fixtures.
- `crates/openagc-mcp/tests/shim.rs` — the socket refuses unknown sessions.
- `crates/agent-claude/tests`, `crates/agent-codex/tests` — the exact CLI
  flags, against fake CLIs.

## What the MVP does not protect against

Local malware with the user's privileges; a compromised agent CLI binary
(it runs as the user); a cloud routine acting within the Gmail permissions
the user granted to Claude or ChatGPT (outside OpenAGC's approval rules, as
the routine editor says); the user approving something they should not.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting on
[audiojak/openagc](https://github.com/audiojak/openagc/security) rather than a
public issue.
