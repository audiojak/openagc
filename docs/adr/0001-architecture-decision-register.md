# ADR 0001: Architecture decision register

- Status: Accepted
- Date: 2026-09-23

## Context

OpenAGC started from a product specification that deliberately left
technical choices open ("evaluate during planning"). Those choices were
researched against primary sources on 2026-09-23 and decided in
[docs/SPECIFICATION.md](../SPECIFICATION.md). This ADR records the register
so later ADRs can supersede individual entries without rewriting the spec's
history.

## Decision

The decision register in spec §0 is adopted as the baseline architecture.
Its load-bearing entries, with the reason each was chosen:

| Area | Decision | Why |
|---|---|---|
| UI | SwiftUI shell, AppKit where speed demands it (thread list is `NSTableView`) | SwiftUI `List` cannot hold 120 fps over 100k rows |
| Core | Rust workspace, in-process static library | One authoritative engine; static linking avoids `disable-library-validation` |
| FFI | UniFFI proc-macro mode | Maps Rust `async fn` to Swift `async throws`; maintained; coarse surface |
| Runtime | Core-owned tokio multi-thread runtime | UniFFI's ambient runtime is current-thread and panics on `block_in_place` |
| Storage | `rusqlite` bundled, one writer thread, WAL | `sqlx` adds no value for a local file DB and fights dynamic FTS queries |
| Search | FTS5 external content: `unicode61` text, `trigram` addresses | Bodies stored once; as-you-type address matching |
| Gmail | Hand-written `reqwest` client, `history.list` polling | Generated crate is in maintenance; push needs a server |
| OAuth | Desktop flow, PKCE, loopback; shipped client + bring-your-own | Installed apps cannot keep secrets; BYO avoids the unverified-app cap |
| Agents | Claude Code via `claude -p` stream-json; Codex via `codex app-server` | Only app-server offers interruption for Codex; subprocess is the supported path for non-SDK languages |
| Enforcement | Permission engine inside the MCP tool call | Neither agent's approval contract is fully documented; one mechanism for both |
| Routines | Local runner, or Claude cloud via the user's CLI (`RemoteTrigger`) | No public routine API; the CLI keeps credentials out of OpenAGC |
| Distribution | Developer ID + notarization + Sparkle, not sandboxed | The app must spawn the user's `claude`/`codex` binaries |

Maintainer decisions (spec §20): repository `audiojak/openagc`, bundle
prefix `ai.actual.openagc`, Actual AI owns the Google Cloud project and the
Apple Developer team, no model picker in MVP, cloud routine publishing on by
default with automatic fallback.

## Consequences

New ADRs are numbered sequentially (`NNNN-short-title.md`) and state which
register entry they amend. The spec is updated in the same change so the
two never disagree.
