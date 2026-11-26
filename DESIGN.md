# Mem-Harness design

A harness around Claude Code that fronts nine std-only Rust tools as one MCP
server and one set of hooks. Goal: less drift and hallucination, and
memories that survive compaction.

## Rules for all code here

- **Zero dependencies.** Every crate's `[dependencies]` holds only path deps on
  sibling crates in this workspace. No crates.io. std only, except the few
  OS calls std lacks (signal a process group, Linux subreaper), declared by
  hand in `mh-sys` against the C library std already links. No other crate
  uses `unsafe`.
- Rust 1.83+, edition 2021. `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` must pass.
- JSON goes through `mh-json` (`Value`, `parse`, `to_json`). Numbers keep their
  source text: pass request ids and arguments through unchanged.
- Fail loudly: a tool that is missing or broken is reported with a fix, never
  read as empty.

## Crates

| Crate | Job |
|---|---|
| `mh-json` | strict JSON value, parser, writer |
| `mh-core` | `Config` (harness.json: backends, ownership table, routes), `identity`, `project` (root, `find_command`) |
| `mh-backend` | `Backend` trait + `stdio::StdioBackend` (spawns one suite tool's MCP server) |
| `mh-adapters` | native readers of tool stores; `relatum` notes and the `relatum:` evidence pointer |
| `mh-hooks` | Claude Code hook handling and the merged compaction brief |
| `mh-proxy` | the MCP server Claude Code talks to; fronts every backend plus native tools |
| `mh-sys` | the only OS calls: `signal_group`, `signal`, `become_subreaper` (Linux), `descendants` |
| `mem-harness` | the binary: `mcp`, `hook`, `brief`, `init`, `ownership`, `doctor`, `install`, `print-config` |

## Ownership (decided 2026-11-26)

`crates/mh-core/default-harness.json` is the single source. Each backend's
`*_DISABLE` env var is computed from it (`Config::backend_env`).

- locks, messages: CoordinationHub (`hub`)
- task board: Libellus board (hub `tasks` off)
- sub-agent registry: the host (hub `spawn` off)
- rules (constraints, conventions): Relatum notes only (Trammel constraint tools off)
- private notes: Libellus (Stele `notes`, `kv` off); shared notes: Relatum
- per-file change journal: Wikifier; per-plan handoff notes: Trammel
- claims: Adsertio, plus a `relatum:` adapter

## Naming

Exposed tool name = `<backend>_<tool>`, after stripping the backend's own
prefix (`relatum_get` stays `relatum_get`; hub `acquire_lock` becomes
`hub_acquire_lock`). See `Config::exposed_name`. Native harness tools are
`harness_*`.

Agent identity: `main` for the main session; sub-agents
`identity::subagent_name(agent_type, agent_id)` (the Libellus 1.4 scheme), used
for every tool.

## Config layers and extension points

`Config::load` reads three layers. The user's own file
(`$MEM_HARNESS_CONFIG`, which must be absolute, else
`~/.mem-harness/harness.json`, else the built-in one) is the only layer that
may name commands, hide tools or write text the model reads. The project's
`.mem-harness/harness.json` is an overlay limited to `PROJECT_KEYS`
(`profile`, `brief`, `main_agent`), because Claude Code may open any
repository. `$MEM_HARNESS_PROFILE` is applied last.
Every problem is an `Err`, never a panic.

Adding things without code:
- a stdio MCP server is a `backends` entry. It may set `handshake_s`,
  `call_s`, `env_pass` (a restricted environment), and `hide_tools` /
  `only_tools`, which the proxy enforces whatever the server honours;
- a brief section is a `brief_sections` entry (`parse: text` for any CLI);
- a store for `init` is a `stores` entry.

A new backend kind (for example local HTTP) is a name in `KNOWN_KINDS` plus
an arm in `mh_backend::build`. Its own settings are read from
`BackendSpec::raw`, so `BackendSpec` does not change. HTTPS would need a TLS
dependency, which the zero-dependency rule excludes; use a stdio bridge.

## Contracts between crates

```rust
// mh-backend (written)
pub trait Backend: Send + Sync {
    fn name(&self) -> &str;
    fn start(&self) -> Result<(), CallError>;
    fn status(&self) -> BackendStatus;
    fn list_tools(&self) -> Result<Vec<Value>, CallError>;
    fn call_tool(&self, tool: &str, arguments: Value, meta: Option<Value>, cancel: &CancelToken) -> Result<Value, CallError>;
    fn shutdown(&self);
}
pub fn build(spec: &BackendSpec, env: Vec<(String, String)>, project: &Path, sink: EventSink) -> Result<Arc<dyn Backend>, String>;

// mh-proxy
pub struct NativeTool { pub def: Value, pub run: Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync> }
pub fn serve<R: BufRead, W: Write + Send + 'static>(cfg: Config, project: PathBuf, natives: Vec<NativeTool>, input: R, output: W) -> Result<(), String>;
pub fn serve_with<R, W>(cfg: Config, natives: Vec<NativeTool>, make_backends: impl FnOnce(EventSink) -> Vec<Arc<dyn Backend>>, input: R, output: W) -> Result<(), String>;

// mh-adapters::relatum
pub struct Note { pub id: String, pub kind: String, pub summary: String, pub status: String, pub superseded_by: Option<String>, pub tags: Vec<String>, pub author: Option<String>, pub updated: Option<String>, pub body: String }
pub fn store_dir(project: &Path) -> Option<PathBuf>;           // $SWARMNOTES_DIR, else nearest .swarmnotes at/above project
pub fn read_note(store: &Path, id: &str) -> Result<Option<Note>, String>;
pub fn list(store: &Path) -> Result<Vec<Note>, String>;
pub fn active_rules(store: &Path) -> Result<Vec<Note>, String>; // kind constraint|convention|decision, status active, newest first

// mh-hooks
pub fn compose_brief(cfg: &Config, project: &Path, agent: &str) -> String;
pub fn run_hook(cfg: &Config, project: &Path, event: &str, stdin: &str) -> Result<String, String>; // the hook's stdout
```

## Hooks

A hook is a program, not the model, so it cannot write down what only the
model knows. Each event does what a hook can do (`mh_hooks::memory`):

| Event | `mem-harness hook` | Does |
|---|---|---|
| SessionStart (startup, resume, clear, compact) | `session-start` | injects the brief |
| UserPromptSubmit | `prompt-submit` | records the working tree's snapshot as the main agent's baseline; prints `{}` and never fails |
| SubagentStart | `subagent-start` | injects the sub-agent's name and its brief; records the sub-agent's baseline snapshot |
| PreCompact | `pre-compact` | prints instructions for the summary: end with `## Not yet recorded`, one `- <kind>: ...` line per unsaved decision, dead end, question or result |
| PostCompact | `post-compact` | saves those lines as Libellus notes for `main`, tagged `from-compact` (`brief.save_unrecorded_on_compact`) |
| Stop, SubagentStop | `stop`, `subagent-stop` | when the turn edited files and called no save tool, blocks once and asks the agent to save, naming only the save tools whose store exists; `stop_hook_active` lets it stop the second time (`brief.nudge_on_stop`) |

The brief's sections are `brief_sections` in the config, run concurrently
and kept in order; `parse` picks the reader (`text` puts the command's
stdout under `heading`; the others are the built-in Libellus, Relatum,
Adsertio and Chisel readers). A section's trim marker names its `tool`.
Before the budget is split by weight, every non-empty section gets room
for its first line and its trim marker (when all of them fit), so none
vanishes unannounced. Output of a `text` section and Chisel's issue lines
come from a third party: control characters are dropped and a line that
would start with `#` is shown as `[#] ...`, so it cannot pose as the
brief's own headings. Libellus output is kept as is (its headings are
part of the brief), and the Relatum and Adsertio readers build their own
lines.

The stores the Setup section, `harness_init` and the hooks check are
`stores`. Each tool finds its store its own way, and the harness must
agree with it (`mh_hooks::stores::locate`). `StoreSpec` has no field for
this, so the rule is kept in `stores.rs` by tool name: `libellus` stops
at the first `.libellus` walking up and refuses a symlink or file there;
`adsertio` walks past a symlink or file to the first real folder; any
other tool, Relatum included, follows a symlink to a folder and walks
past a file, with the same lookup (`mh_adapters::relatum::store_dir_in`)
the brief's rules section reads from. A refused store is named with the
reason in the brief, and `harness_init` does not run over it.

The "Not yet recorded" list is found under that label as a heading of any
level, a numbered (`8. Not yet recorded:`) or a bold line, any case; fenced
code is skipped. Inside it, a line that starts with a note kind and a
colon (bulleted, numbered or bare) is always an item, even when it ends in
a colon; other bulleted or numbered lines are `context` items. A list
under a heading ends at a heading of the same or a higher level or at a
numbered label; one under a numbered label ends at any heading or numbered
label (bold lines inside either are sub-headings); one under a bold label
ends at any label. Each line has control characters and runs of spaces
collapsed before it is clipped to 200 characters. Lines already saved as a
`from-compact` note of `main` (the next summary repeats the last list),
and repeats within one summary, are skipped: `libellus ls` is read page by
page with its `next_before_id` cursor (2000 notes a page, halved when a
page is too large to read whole), stopping once every line was found, and
at most 40 notes are read with `libellus get` for their tags. If that
check cannot finish, the lines are saved anyway and the report says so.
If Libellus refuses the batch (non-zero exit), each line is retried
alone; a batch that timed out or could not start is not retried. The
whole PostCompact stops after 30 seconds and reports what it did not
save.

The stop check reads the last 8 MB of the transcript and looks at the turn
after the last prompt, or after the last stop-nudge feedback (`Stop hook
feedback: ...`; those edits were already asked about). A prompt is a user
entry someone typed: meta entries, the compact summary, entries whose
`origin.kind` is not `human` (task notifications, peer and coordinator
messages), text that is only `<system-reminder>` blocks, local-command and
shell-escape output, and interrupt markers do not start a turn. When no
turn start is in the part read, the agent may stop. Edit and save tools
are `brief.edit_tools` and `brief.save_tools` (default edit tools `Edit`,
`Write`, `MultiEdit`, `NotebookEdit`); shell tools are `Bash` and
`PowerShell`.

Whether the turn changed files comes from git when it can
(`mh_hooks::snapshot`). A snapshot is `HEAD`, the entries of `git
--no-optional-locks status --porcelain=v1 -z --untracked-files=normal`
and each listed path's size and modification time, hashed. The baseline
is taken at UserPromptSubmit (main agent) and SubagentStart (that
sub-agent), and saved in `<temp>/mem-harness/<session_id>[-<agent_id>].snap`
(ids cleaned to `[A-Za-z0-9_-]`; written to a fresh file and renamed, so a
planted symlink is replaced, not followed; files older than 7 days are
removed). At Stop and SubagentStop a new snapshot is taken: the turn
edited files when it differs from the baseline and the turn ran an edit or
shell tool (edits the user makes in an editor while the agent only reads
are not the turn's). The new snapshot then becomes the baseline, whether
the agent was nudged or allowed to stop, so one set of changes is asked
about once. Outside git, without a baseline, or when git fails, the edit
tools in the transcript decide. Git calls share the brief's command
timeout. A project with none of the stores is never nudged and runs no
git. SessionEnd is not used: it cannot reach the model and has 1.5
seconds.

Hook commands run with the brief's timeout over the whole run, output
included; on Unix each runs in its own process group. On timeout (or when
something it started still holds its output at the deadline) the group
and every descendant listed by `ps` before the kill get TERM, then KILL
after 200 ms, through `mh-sys` (no `kill` binary). On Linux the hook
process makes itself a subreaper (`mh_hooks::adopt_orphans`, called by
`run_hook` only, as orphans stay zombies until the process exits), so a
process that double-forked out of a command's tree is re-parented to it
and stopped with the timed-out command.

## Claims about rules

Decided 2026-11-26: the Relatum evidence adapter lives in **Adsertio itself**
(1.5.0, pointer `relatum:.swarmnotes#<note-id>`), so every view computes the same
verdict. The harness has no verdict tool of its own; `mh-adapters::relatum`
only reads notes to list active rules in the brief. Verdicts on `relatum:`
pointers come only from Adsertio (`adsertio_view`).

## Known limits

- A process that a backend server starts and leaves holding the server's
  stdin, without reading it, can keep that connection's writer thread
  blocked in `write` until the process exits. Nothing waits on that thread
  (calls time out, and shutdown kills the server), so it costs one idle
  thread, not a hang. std cannot close a pipe under a blocked writer.
- Hook timeouts reach the command's process group and its descendants
  listed just before the kill. On macOS (no subreaper) a process that
  double-forks and whose middle process exits before that listing is
  re-parented to launchd and not reached; its output is no longer waited
  on. On Linux such orphans are found, but with several hook commands
  running at once an orphan cannot be told apart by command, so a timeout
  stops every orphan the hook process adopted.
- The stop check's git snapshot does not see ignored files, files outside
  the repository, or an edit inside an untracked folder that git lists
  only as `dir/` (only the folder's own time is read). Edits the user makes
  while the turn runs a shell or edit tool count as the turn's, and so do a
  concurrent sub-agent's. Outside git, edits made with shell commands are
  not seen.

