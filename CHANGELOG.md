# Changelog

## Unreleased

Fixes from a second adversarial review (30 confirmed, 15 plausible findings), and the known limits, except Windows-specific work.

**Security**
- A project's `.mem-harness/harness.json` may now set only `profile` (one of yours), `brief` and `main_agent`. `profiles`, `ownership` and `routes` moved to your own config, because a repository could otherwise:
  - hide a feature's owner by replacing the ownership table or defining its own trims;
  - put its own text into the instructions the model reads.
- A trim is one name (`[A-Za-z0-9_-]`). A comma or a NUL character used to get past the owner and `core` checks.
- Each ownership entry lists the owner's protected groups and tools (`protect`). Before, the check only worked when a feature's name equalled a group name.
- Unknown keys are errors inside every object, not only at the top level, so a typo cannot silently turn off `env_pass`.
- `MEM_HARNESS_CONFIG` must be an absolute path. A relative one would have been read from the repository.
- Command lookup skips files without an execute bit, as the shell does.
- Command lookup ignores relative and empty `PATH` entries (since 0.3.0), so a program is never found through the folder the harness was started in.
- `Value::obj` with a repeated key keeps one entry, as parsing does.
- `print-config` and the usage text point at `~/.mem-harness/harness.json` and list `MEM_HARNESS_PROFILE`.

## 0.3.0

Fixes from a full adversarial review (32 findings, 28 reproduced with failing tests), and room for new tools.

**Security**
- A project's `.mem-harness/harness.json` can no longer name commands. It is now an overlay limited to `profile`, `profiles`, `brief`, `main_agent`, `routes` and `ownership`. `backends`, `stores` and `brief_sections` come only from `$MEM_HARNESS_CONFIG` or `~/.mem-harness/harness.json`. **If you kept a full config in a project, move it to `~/.mem-harness/harness.json`.**
- `env_pass` gives a backend only the basic system variables plus the ones listed, not your whole environment.
- `install` quotes the binary path for sh and PowerShell.
- The release workflow checks the tag against the version, runs the tests, and only the publishing job may write.

**Reliability**
- **Backends:**
  - one writer thread per server, so a stalled pipe can no longer block calls past their time limit or hang shutdown;
  - an exit is noticed even while a child the server started holds its output;
  - no "broken pipe" errors in the moment after a server exits;
  - output lines are capped;
  - a server that hangs at startup now counts toward the restart guard.
- **Proxy:**
  - a backend that failed to start is retried (on a call, on `tools/list` and on a backoff), and `list_changed` is sent when its tools appear;
  - a backend panic answers the call with an error;
  - a backend still starting when the harness exits is stopped once its start finishes;
  - a request with an id but no method gets -32600, and a bad cursor gets -32602;
  - an unknown protocol version gets the newest one; 2025-03-26 is dropped;
  - stdin lines are capped.
- **Hooks:**
  - command timeouts hold even when a command leaves a process behind (on Unix the process group gets TERM, then KILL);
  - `MEM_HARNESS_PROFILE` typos are reported instead of crashing;
  - a hook never exits non-zero.
- **Memory hooks:**
  - unsaved items are cleaned of control characters, and retried one by one if the batch is refused;
  - items saved by an earlier compact are skipped;
  - the "Not yet recorded" list is found under any heading level, as a numbered or bold label, and outside code blocks;
  - task notifications, system reminders and other injected entries no longer count as a new prompt in the stop check;
  - the nudge names only the save tools whose store exists;
  - a symlinked store is reported, not used.

**Correctness**
- Config validation: a trim must reach a backend that has a `disable_env` and must never hide a feature from its owner. `main_agent`, env keys and `disable_env` are checked. Numbers out of range are errors, not silently replaced. Unknown keys are errors.
- JSON: a repeated key keeps one entry with the last value, and `\u` takes exactly four hex digits.
- The project root is canonical, and `MEM_HARNESS_PROJECT` is checked. Relative commands are resolved against the project. Windows tries `PATHEXT`.

**Adding tools without code**
- Per backend: `handshake_s`, `call_s`, `env_pass`, and `hide_tools` / `only_tools`. The proxy enforces hiding itself, so it works for third-party servers.
- `brief_sections` (with a generic `text` reader), `stores` (with `save_hint`, used by the stop nudge), and `save_tools` / `edit_tools` are config. A new backend kind needs a `KNOWN_KINDS` entry and one match arm, and reads its own settings from `BackendSpec::raw`.

## 0.2.1

- **Fix:** when a backend crashes, its status now records the exit before the waiting calls are failed. Previously a caller could see the crash error and then read a status that showed no error yet. This also made one backend test fail about once in 600 runs, and once on main CI.
- README: "Roll back" now says the backups come from the commands `mem-harness install` prints.
- The `mem-harness` crate now uses the workspace `description`.

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
