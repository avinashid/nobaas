# CLAUDE.md

All project guidance lives in AGENTS.md, so every agent tool reads the same rules:

@AGENTS.md

## Claude Code specifics

- For browser checks use the `playwright` MCP tools (headless Chromium; no Chrome on this arm64 host). Close the
  browser when finished.
- The Bash working directory resets between calls: use absolute paths and `git -C <worktree>`.
- Run builds and tests in the foreground, or poll them to completion in the same turn.
