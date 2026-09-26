# Project Instructions for AI Agents

This file provides instructions and context for AI coding agents working on this project.

## OpenAGC project notes

- Spec: `docs/SPECIFICATION.md` is the source of truth; beads issue descriptions cite its sections (§N).
- Toolchain PATH (not in the default shell snapshot): `export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"`.
- The app's self-snapshot (`-OpenAGCSnapshot`) cannot capture some pure-SwiftUI surfaces on macOS 26 (Form/List content, the onboarding scroll view): they come out blank although the view tree (`-OpenAGCSnapshotDumpViews YES`) shows them. AppKit views and the agent column capture fine. Real window capture needs Screen Recording permission.
- Checks before closing any issue: `scripts/gate.sh` (wraps `scripts/check.sh` without hiding its exit status) (fmt, clippy -D warnings, tests, check-deps, deny; exits non-zero) and `scripts/test-macos.sh` for the app. Never pipe the gate into `grep`/`tail` before `&& git commit`: the pipe's status wins and a failed gate commits anyway. Use `if scripts/gate.sh >log 2>&1; then …; fi`.
- Only `openagc-core` may depend on UniFFI; dependency direction is enforced by `cargo xtask check-deps`.
- Never touch real Gmail, Google/Apple accounts, or create Claude cloud routines from automation; test against fakes.
- Never launch the app against the real account or start sync outside the fakes: the dev-signed build can read the real Gmail token from the Keychain. Snapshots use the demo account and `-OpenAGCFakeAgents YES`. Never delete anything under `~/Library/Application Support/OpenAGC`.
- Overnight work happens on an `overnight-*` branch (plan in `docs/plans/`); push after each closed issue; never push to `main` overnight.
- Debug builds are signed with the "OpenAGC Dev" identity when `macos/Local.xcconfig` (from `scripts/dev-signing.sh`, gitignored) exists; ad-hoc rebuilds lose Keychain access to the stored Gmail sign-in.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:1105d646 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/core-concepts/sync-concepts.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->


## Build & Test

_Add your build and test commands here_

```bash
# Example:
# npm install
# npm test
```

## Architecture Overview

_Add a brief overview of your project architecture_

## Conventions & Patterns

_Add your project-specific conventions here_
