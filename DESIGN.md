# Mem-Harness design

A harness around Claude Code that fronts nine std-only Rust tools as one MCP
server and one set of hooks. Goal: less drift and hallucination, and
memories that survive compaction.

## Rules for all code here

- **Zero dependencies.** Every crate's `[dependencies]` holds only path deps on
  sibling crates in this workspace. No crates.io. std only.
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
| `mem-harness` | the binary: `mcp`, `hook`, `brief`, `ownership`, `doctor`, `print-config` |

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
pub fn run_hook(cfg: &Config, project: &Path, event: &str, stdin: &str) -> Result<String, String>; // stdout JSON line
```

## Claims about rules

Decided 2026-11-26: the Relatum evidence adapter lives in **Adsertio itself**
(1.5.0, pointer `relatum:.swarmnotes#<note-id>`), so every view computes the same
verdict. The harness has no verdict tool of its own; `mh-adapters::relatum`
only reads notes to list active rules in the brief. Verdicts on `relatum:`
pointers come only from Adsertio (`adsertio_view`).
