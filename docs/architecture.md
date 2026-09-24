# Architecture

OpenAGC is a native macOS app (SwiftUI with AppKit where speed matters) on a
Rust core. The core owns everything that is not drawing pixels: the mail
store, Gmail sync, MIME, search, the agent stack and routines. Swift talks to
it through one UniFFI object. The full design, with the reasons behind it, is
in [SPECIFICATION.md](SPECIFICATION.md); this page is the map.

```text
┌──────────────────────────── OpenAGC.app ────────────────────────────┐
│ SwiftUI / AppKit                                                     │
│  MainWindow ─ Sidebar · ThreadList (NSTableView) · Reader (WKWebView)│
│  Composer windows · Agent column · Routines window · Settings        │
│  Stores (@Observable, main actor) ──▶ CoreClient (only UniFFI user)  │
└──────────────────────────────┬───────────────────────────────────────┘
                               │ UniFFI (async calls, one event stream)
┌──────────────────────────────▼──────────── openagc-core (Rust) ─────┐
│  mail-store (SQLite, FTS5) ◀─ mail-sync (bootstrap, history, outbox) │
│        ▲                         │                                   │
│        │                    provider-gmail (REST) ─▶ Google          │
│  agents: AgentManager ─▶ agent-claude / agent-codex ─▶ user's CLI    │
│          MCP socket ◀─ openagc-mcp (spawned by the CLI)              │
│          permissions (decide + session guard) · approvals · audit    │
│  routines: model + prompt (agent-api) · scheduler · cloud (CLI)      │
└──────────────────────────────────────────────────────────────────────┘
```

## Crates

| Crate | What it is | May depend on |
|---|---|---|
| `mail-domain` | Plain types: ids, messages, labels, time helpers | — |
| `mail-store` | SQLite store: schema and migrations, writer, reads, search, drafts, outbox, agent sessions and audit, routines | domain |
| `mail-mime` | Parse, sanitize (ammonia), build (mail-builder), text extraction, Markdown | domain |
| `provider-api` | `MailProvider` trait, HTTP/retry/rate limit, the fake provider | domain |
| `provider-gmail` | Gmail REST client and OAuth (PKCE, loopback) | domain, mime, provider-api |
| `mail-sync` | Sync engine, outbox drain, compose/send, attachments on demand | domain, store, mime, provider-api |
| `agent-api` | Agent traits, events, the session manager, process helpers, routines (model, prompt, schedule) | domain |
| `agent-claude` | Claude Code adapter and Claude cloud routines through the CLI | domain, agent-api |
| `agent-codex` | Codex app-server adapter | domain, agent-api |
| `permissions` | Risk classes, policy, `decide`, per-session hard limits | domain |
| `agent-mcp` | Tool catalog, the shim ↔ core socket protocol and server | domain, agent-api, permissions |
| `openagc-mcp` | The stdio MCP shim binary agents spawn | agent-mcp |
| `openagc-core` | The UniFFI surface; ties everything together | all of the above |

`cargo xtask check-deps` enforces the right-hand column.

## How mail moves

- **Sync** (`mail-sync::engine`): a bootstrap lists message ids by priority
  (inbox first) into a queue that a backfill loop fetches in batches; then
  `history.list` from a cursor keeps the store current. Outbox changes are
  drained before each history pass so history never appears to undo them.
- **Local-first changes**: archive, read, star, label, trash, drafts and
  sends are written to the store and queued in the outbox in one
  transaction; the UI updates at once and the outbox retries with backoff,
  rolling back on permanent failure.
- **Events**: the core emits `ThreadsChanged` hints coalesced in 50 ms
  windows, sync status, new mail, and agent events batched every 16 ms.
  Swift stores re-query only what changed.
- **Reading**: bodies are sanitized once at sync time; the reader renders a
  whole thread as one document in a locked-down `WKWebView` (no JavaScript,
  CSP, remote images blocked, links checked).

## How agents work

A prompt starts (or resumes) an agent session with the user's own Claude Code
or Codex CLI. The CLI is given no built-in tools and exactly one MCP server,
`openagc-mcp`, which forwards each tool call over a per-launch Unix socket to
the core. There every call passes the session's hard limits and the
permission decision; reads are answered from the store, reversible changes go
through the same outbox as the UI, and sends, forwards and deletes wait for
the user in the agent column. Every call is recorded. See
[security.md](security.md) and [mcp.md](mcp.md).

## Routines

A routine is data (buckets, rules, schedule); its prompt is generated per
runner. Local routines run as agent sessions on a scheduler in the core.
Claude cloud routines are created and updated through the user's `claude`
CLI (`RemoteTrigger`), so OpenAGC never holds a claude.ai credential; their
work is also inferred from label changes seen in Gmail history.

## Where to start reading

- Swift entry: `macos/OpenAGC/App/OpenAGCApp.swift`, `AppModel.swift`,
  `Core/CoreClient.swift`.
- Core entry: `crates/openagc-core/src/lib.rs`, then `mail.rs`, `sync.rs`,
  `agents/`.
- Tests: `cargo test` (Rust, fakes only) and `scripts/test-macos.sh test`.
