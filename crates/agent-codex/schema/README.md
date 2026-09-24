# Codex app-server protocol schema

The subset of `codex app-server`'s JSON-RPC schema that `agent-codex` speaks
(spec §9.4), from **codex-cli 0.145.0**:

```bash
codex app-server generate-json-schema --experimental --out /tmp/codex-schema
```

Only the files the adapter uses are kept (the full bundle is 4 MB):
`initialize`, `thread/start`, `thread/resume`, `turn/start`,
`turn/interrupt`, the notifications it maps to agent events, and the
server-request union (all of which OpenAGC refuses). The adapter's request
shapes are checked against these files by `tests/schema.rs`.

When upgrading Codex, regenerate, diff against this directory, and add a new
version directory if anything the adapter uses changed.

The `-c` overrides and `--disable` flags in `server_args()` were checked
against the same version with `codex app-server --strict-config …` (which
rejects unknown configuration fields at startup) and stdin closed, so no
session was started. `--ignore-user-config` from an earlier draft of the spec
does not exist in 0.145; replacing the whole `mcp_servers` table has the same
effect for MCP servers.
