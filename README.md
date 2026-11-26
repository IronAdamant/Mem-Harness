# Mem-Harness

One MCP server and one set of Claude Code hooks in front of nine std-only Rust
tools: Probatio, CoordinationHub, Wikifier, Stele-context, Adsertio, Libellus,
Chisel, Trammel and Relatum. Each shared feature has one owner, so there are
no duplicate tools. Rules, claims, notes and the task board come back as one
brief at session start, after compaction, and for every sub-agent.

Zero dependencies (std only). MIT licensed. Design and contracts: [DESIGN.md](DESIGN.md).

## Install

The nine tools must be on `PATH` or in `~/.cargo/bin`. Adsertio must be
1.5.0 or later (Relatum evidence).

```sh
git clone https://github.com/IronAdamant/Mem-Harness && cd Mem-Harness
cargo install --path crates/mem-harness --locked   # -> ~/.cargo/bin/mem-harness
mem-harness doctor                                 # every backend binary found?
mem-harness install                                # prints the config below
```

## Wire it into Claude Code (once, user scope)

1. In `~/.claude.json` under `mcpServers`, replace the per-tool servers with one entry:
   ```json
   "mem-harness": { "type": "stdio", "command": "/Users/<you>/.cargo/bin/mem-harness", "args": ["mcp"] }
   ```
2. In `~/.claude/settings.json`, replace the Libellus and Adsertio hooks with:
   ```json
   "hooks": {
     "SessionStart": [{ "matcher": "startup|resume|clear|compact",
       "hooks": [{ "type": "command", "command": "/Users/<you>/.cargo/bin/mem-harness", "args": ["hook", "session-start"] }] }],
     "SubagentStart": [{
       "hooks": [{ "type": "command", "command": "/Users/<you>/.cargo/bin/mem-harness", "args": ["hook", "subagent-start"] }] }]
   }
   ```
3. Restart Claude Code. `/mcp` should show `mem-harness` connected.

Leaving the old servers or hooks in place shows every tool and the brief twice.

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
`~/.claude/settings.json`, made when the harness was wired in, and restart
Claude Code.
