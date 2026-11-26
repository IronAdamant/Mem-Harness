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

Copy `mem-harness print-config` to `<project>/.mem-harness/harness.json` (or
point `MEM_HARNESS_CONFIG` at a file) and edit it there. The ownership table
decides which tool owns a feature, and each backend's `*_DISABLE` variable is
computed from it. The `lean` profile is the default. Set
`MEM_HARNESS_PROFILE=full` (or `"profile": "full"`) to show every non-duplicate
tool.

## Roll back

Restore the `.bak-mem-harness-*` copies of `~/.claude.json` and
`~/.claude/settings.json`, which you made by running the step 0 commands that
`mem-harness install` prints, and restart Claude Code. Without a backup, run `claude mcp remove --scope user
mem-harness`, delete the `mem-harness hook` entries from
`~/.claude/settings.json`, and add back the per-tool servers.
