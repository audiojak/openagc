# OpenAGC

**Open Agent Gmail Client** — an open-source, local-first, native macOS email
client built for personal AI agents.

OpenAGC is a Gmail client first: a fast SwiftUI/AppKit app over a Rust core
that keeps your mailbox in a local SQLite database. Its defining feature is
that it lets the AI agents you already use — [Claude Code](https://claude.com/claude-code)
and [Codex](https://openai.com/codex) — work on your mail through a small,
explicit set of tools, with sending and deleting always gated on your
approval. You bring your own AI subscription; OpenAGC never sees your AI
credentials and has no server of its own.

> **Status: pre-alpha.** The architecture is specified and implementation is
> under way. Nothing here is usable yet.

## How it works

```text
Gmail ──HTTPS/OAuth──▶ OpenAGC.app on your Mac
                         ├── local mail database + search
                         ├── permission engine (the only enforcement point)
                         └── mail tools (MCP) ──▶ your claude / codex CLI
```

- **No backend.** Mail goes from Google to your Mac and nowhere else. The
  project operates no servers, has no accounts, and collects no telemetry.
- **Agents get tools, not your mailbox.** An agent searches locally, reads
  only what it asks for, and every access is logged. Email content is
  treated as untrusted input.
- **You approve anything that leaves.** Sending, forwarding and deleting are
  proposals you review; archive, labels and drafts are reversible.
- **Routines.** Scheduled sorting of automated mail into review labels, run
  locally or as a Claude cloud routine created through your own Claude Code
  login.

The full technical specification is in [docs/SPECIFICATION.md](docs/SPECIFICATION.md).

## Requirements

- macOS 26 or later on Apple Silicon
- A Gmail account
- Optional: [Claude Code](https://claude.com/claude-code) 2.1+ and/or
  [Codex CLI](https://github.com/openai/codex) 0.145+, logged in

## Building from source

```bash
./scripts/bootstrap.sh     # Rust (rustup), XcodeGen; checks for Xcode 27
cargo build --workspace
cargo test --workspace
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the app build, tests and workflow.

## Who is behind this

OpenAGC is an independent open-source project sponsored by
[Actual AI](https://actual.ai). Actual AI is the developer named on the
signed app and on Google's OAuth consent screen. Mail never passes through
Actual AI's infrastructure.

OpenAGC is **not related** to [github.com/OpenAGC](https://github.com/OpenAGC),
an unrelated PlayStation 5 graphics project that shares the acronym.

## License

[MIT](LICENSE)
