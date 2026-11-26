# Changelog

## 0.2.0

**Memory that was never saved**
- **PreCompact:** the compact summary is asked to end with a `## Not yet recorded` list, one line for each decision, dead end, question or result that was not saved.
- **PostCompact:** those lines are saved as Libellus notes for `main`, tagged `from-compact`, so they reach later sessions too. Turn it off with `brief.save_unrecorded_on_compact`.
- **Stop and SubagentStop:** a turn that edited files but called no save tool is asked, once, to save before it stops. Turn it off with `brief.nudge_on_stop`. A project with no stores is never asked.

**Install and docs**
- `mem-harness install` now prints setup for every project: a backup step, `claude mcp add --scope user`, and all six hooks. Previously it printed project setup that did not match the README.
- The README no longer hardcodes `/Users/...` paths. Roll back now restores the backups that `install` tells you to make.
- `bench/` holds the comparison runs, scripts and results behind the release-note numbers. Corrected figure: first-turn context fell by 8%, not 17%. The 17% was newly cached tokens.

**Fixes**
- The brief's claims heading is now "Claims unverified or refuted", because it lists both.
- `doctor` checks the `relatum` CLI, which `init` needs.
- Removed unused crate dependencies. A test now fails on any dependency that no code uses.

**CI and releases**
- CI runs fmt, clippy, tests and a release build on Linux, macOS and Windows.
- Release binaries for 4 targets, with `SHA256SUMS`.

## 0.1.0

First release: one MCP server and one set of hooks in front of the nine tools, an ownership table, a merged brief, and the `harness_status`, `harness_brief` and `harness_init` tools.
