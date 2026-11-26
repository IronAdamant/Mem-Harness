# Mem-Harness

One MCP server and one set of Claude Code hooks in front of nine std-only Rust
tools: Probatio, CoordinationHub, Wikifier, Stele-context, Adsertio, Libellus,
Chisel, Trammel and Relatum. Each shared feature has one owner, so there are
no duplicate tools. Rules, claims, notes and the task board come back as one
brief at session start, after compaction, and for every sub-agent.
Claude Code still runs the loop, the tools and compaction; the harness sits
in front of the tools.

Zero dependencies (std only). MIT licensed. Design and contracts: [DESIGN.md](DESIGN.md).
Measured against the nine tools wired up separately: [bench/](bench/).

## Install

The nine tools must be on `PATH` or in `~/.cargo/bin`. Adsertio must be
1.5.0 or later (Relatum evidence). Prebuilt binaries for Linux, macOS and
Windows are on the [releases page](https://github.com/IronAdamant/Mem-Harness/releases),
or build from source:

```sh
git clone https://github.com/IronAdamant/Mem-Harness && cd Mem-Harness
cargo install --path crates/mem-harness --locked   # -> ~/.cargo/bin/mem-harness
mem-harness doctor                                 # every backend binary and CLI found?
```

## Wire it into Claude Code (once, for every project)

```sh
mem-harness install
```

prints the commands and settings, with the real path to the binary filled in.
In order, they:

0. back up `~/.claude.json` and `~/.claude/settings.json` as
   `*.bak-mem-harness-<date>` (Roll back below restores these);
1. add the server for every project with
   `claude mcp add --scope user mem-harness -- <path> mcp`, and remove the
   per-tool servers it replaces;
2. give the `hooks` block to merge into `~/.claude/settings.json`, in place
   of the Libellus and Adsertio hooks. It wires SessionStart, SubagentStart,
   PreCompact, PostCompact, Stop and SubagentStop to `mem-harness hook <event>`.

Restart Claude Code, and `/mcp` should show `mem-harness` connected. Old
servers or hooks left in place show every tool and the brief twice.

## What survives a compact

The brief comes back after every compact, and holds what was **saved**:
Relatum rules, Libellus notes and board, Adsertio claims, open Chisel issues.
Two hooks cover what was not saved:

- **Before a compact**, the summary is asked to end with a
  `## Not yet recorded` list. After the compact those lines are saved as
  Libellus notes tagged `from-compact`, so they also reach later sessions.
  A line saved by an earlier compact is not saved again, and one line
  Libellus refuses does not lose the others.
- **When a turn edits files but saves nothing**, the agent is asked once to
  save its decisions, rules or results before it stops. This costs one extra
  round trip on such turns. Turn it off with `"nudge_on_stop": false` under
  `brief` in `harness.json`; `"save_unrecorded_on_compact": false` turns off
  the other.

Hooks run as programs, not as the model, so nothing is saved that the model
neither wrote down nor put in the summary.

## Start in a project

```sh
cd your-project            # a git repository
mem-harness init           # creates .swarmnotes, .libellus, .adsertio if missing
```

Or let the agent call `harness_init`. The brief says when a store is missing.
Then use Claude Code as usual. Tools appear as `mcp__mem-harness__<backend>_<tool>`,
for example `relatum_new`, `stele_find_references`, `chisel_diff_impact`,
`hub_acquire_lock`. Useful first calls in a new repository are `stele_index` and
`chisel_analyze`, which build the indexes.

## Everyday commands

| Command | Does |
|---|---|
| `mem-harness brief [--agent NAME]` | Print the brief the hooks inject |
| `mem-harness ownership` | Which tool owns each feature, and what is hidden where |
| `mem-harness doctor` | Config, project, every backend binary |
| `mem-harness print-config` | The built-in `harness.json` |

In a session, `harness_status` shows each backend's state and errors, and
`harness_brief` re-reads the brief on demand.

## Configure

The config is read in three layers:

1. **Yours:** `$MEM_HARNESS_CONFIG`, else `~/.mem-harness/harness.json`, else
   the built-in one (`mem-harness print-config` prints it). Only this layer
   may name commands: `backends`, `stores` and `brief_sections`.
2. **The project's:** `<project>/.mem-harness/harness.json`, laid over yours.
   Claude Code may open any repository, so this file may only pick one of
   your profiles (`profile`), change `brief` settings, and set `main_agent`.
   Anything else is refused with an error, because it could run commands,
   hide tools, or put text in the instructions the model reads.
3. **`MEM_HARNESS_PROFILE`** picks the profile. `lean` is the default, and
   `full` shows every tool that is not a duplicate.

The ownership table decides which tool owns a feature, and each backend's
`*_DISABLE` variable is computed from it. Each entry's `protect` list names
the owner's groups and tools that serve the feature. A trim that cannot take
effect, or would hide one of those, is an error: for example a trim on a
backend with no `disable_env`, or `locks` on the hub. A trim is one group or
tool name (letters, digits, `_`, `-`). Unknown keys are errors at every
level, so a typo such as `env_passs` cannot silently turn a setting off.

### Adding a tool or a third-party MCP server

Add a backend to your own config. A server that runs over stdio needs only
this:

```json
{ "name": "docs", "command": "npx", "args": ["-y", "some-mcp-server"],
  "handshake_s": 60, "env_pass": ["SOME_API_KEY"], "hide_tools": ["delete_*"] }
```

Its tools appear as `docs_<tool>`. The optional settings:

| Setting | Does |
|---|---|
| `handshake_s` | how long to wait for it to start (default 20) |
| `call_s` | the cap on one call (default 3600) |
| `env_pass` | gives it only the basic system variables plus these, instead of your whole environment. Use it for servers you did not write. |
| `keep_children` | leave processes the server started running when it stops (normally its whole process group is stopped with it) |
| `hide_tools` / `only_tools` | the proxy hides tools itself, so this works for servers that have no `*_DISABLE` variable. A pattern matches the server's own tool name, or that name after `strip_prefix`. `*` is a wildcard. |

A remote server works through a stdio bridge such as `mcp-remote`. The
harness has no dependencies, and Rust's standard library has no TLS, so it
does not speak HTTPS itself.

To add the tool's output to the brief, add an entry to `brief_sections`:

```json
{ "name": "docs_status", "parse": "text", "heading": "## Docs", "command": "docs-cli", "args": ["status", "--agent", "{agent}"], "weight": 10 }
```

If the tool keeps a store that `mem-harness init` should create, add it to
`stores`.

## Roll back

Restore the `.bak-mem-harness-*` copies of `~/.claude.json` and
`~/.claude/settings.json`, which you made by running the step 0 commands that
`mem-harness install` prints, and restart Claude Code. Without a backup, run `claude mcp remove --scope user
mem-harness`, delete the `mem-harness hook` entries from
`~/.claude/settings.json`, and add back the per-tool servers.
