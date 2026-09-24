# Performance

Spec §1.3 sets the targets; §13 lists the rules that meet them. This page
records how they are measured and the latest numbers.

## How to measure

```bash
cargo xtask fixture   # once: synthetic mailbox, ~130k messages, cached in build/fixtures
cargo xtask perf      # store operations, release build, fails if a budget is missed
scripts/test-macos.sh test -only-testing:OpenAGCTests/PerformanceTests   # through the FFI
```

The Swift tests skip themselves when the fixture is absent.

## Latest results

2026-09-24, MacBook (Apple Silicon), fixture of 131,731 messages in 57,143
threads (39,503 archived). p95 over 40–60 runs after a warm-up.

**Store (Rust, release):**

| Operation | p95 | Budget |
|---|---|---|
| Open store + first inbox page (cold launch share) | 2.35 ms | 60 ms |
| Sidebar mailboxes with counts | 0.01 ms | 3 ms |
| Inbox first page, 150 rows | 0.14 ms | 8 ms |
| Archive page at any depth, 150 rows | 0.12 ms | 8 ms |
| Open thread: detail + all bodies | 0.03 ms | 5 ms |

**Through the FFI (Swift, debug build):**

| Operation | p95 | Budget |
|---|---|---|
| Inbox page via FFI, 150 rows | 1.94 ms | 20 ms |
| Select thread → reader data ready | 0.45 ms | 30 ms |
| Build reader HTML | 0.02 ms | 3 ms |
| Configure + lay out 1,000 list rows | 30 ms | 120 ms |

## Not yet measured

- WebKit paint time after `loadHTMLString` (the rest of "body visible in
  < 50 ms").
- Scroll frame rate in the thread list; needs an interactive session with
  Instruments (Animation Hitches).
- Cold launch end to end (process start → first frame).
- A self-hosted Apple Silicon CI runner to track these over time (spec §13).
