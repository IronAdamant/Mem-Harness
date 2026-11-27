# Harness vs. nine separate servers

One small comparison, run on 2026-11-26. It is a single task, with two runs per
setup, so read it as an indication, not a benchmark.

## Setup

- Model `claude-sonnet-5-5`, `claude -p` in stream-json mode, with
  `--strict-mcp-config`, so only the servers under test load
  ([run.sh](run.sh)).
- Project: a clean checkout of [Relatum](https://github.com/IronAdamant/Relatum)
  at `ca1cadd`, with no stores in it.
- Task: [task.txt](task.txt). The agent records a rule, finds a definition and
  its references, picks the tests for a file, writes a private note, and makes
  a claim backed by the rule.
- **nine**: the nine tools as nine MCP servers, with their own Libellus and
  Adsertio hooks ([config/nine-tools.*](config/)).
- **harness**: one `mem-harness` server and its hooks
  ([config/harness.*](config/)). `harness-pre-*` ran commit `e7e9b3f`, before
  `harness_init` existed. `harness-fresh` ran `563743e`.

The raw stream-json output is in [runs/](runs/), with local paths replaced.
`python3 analyze.py runs` prints the table below from it.

## Results

| Run | MCP tools offered | First-turn context (tokens) | Cost (USD) | Turns |
|---|---|---|---|---|
| nine-pre-1 | 187 | 26,216 | 0.2024 | 20 |
| nine-pre-2 | 187 | 26,216 | 0.2035 | 20 |
| harness-pre-1 | 130 | 24,024 | 0.1761 | 18 |
| harness-pre-2 | 130 | 24,024 | 0.1802 | 18 |
| harness-fresh | 130 | 23,896 | 0.1747 | 16 |

First-turn context is input, cache-creation and cache-read tokens added
together. Every run finished the task.

Comparing the two `pre` pairs:

| Measure | Change |
|---|---|
| Tools offered | 57 fewer (−30%) |
| First-turn context | 2,192 tokens fewer (−8%) |
| Newly cached tokens on the first turn | 10,623 vs 12,815 (−17%) |
| Cost per run | 0.1782 vs 0.2030 USD on average (−12%) |
| Turns | 18 vs 20 |

The v0.1.0 release notes said "17% less context at session start". That 17%
is the drop in newly cached tokens. The whole first-turn context fell by 8%.

## Re-run on 0.4.1 (2026-11-27)

The same task and checkout, run again with `mem-harness` 0.4.1 and all seven
of its hooks ([config/harness-041.settings.json](config/harness-041.settings.json)),
against the same nine servers with their own hooks. The MCP configs need
absolute paths: `claude` does not expand the `~` in the redacted copies here.
Raw output: `runs/*-041.jsonl`.

| Run | MCP tools offered | First-turn context (tokens) | Cost (USD) | Turns | Task done |
|---|---|---|---|---|---|
| nine-1-041 | 187 | 26,020 | 0.1881 | 18 | no |
| nine-2-041 | 187 | 26,020 | 0.1816 | 20 | no |
| harness-1-041 | 130 | 23,918 | 0.1741 | 18 | yes |
| harness-2-041 | 130 | 23,918 | 0.1831 | 18 | yes |

- First-turn context is 2,102 tokens lower with the harness (−8%), as on 0.1.0.
  Claude Code's own share grew between the two runs (13,399 to 15,319 cached
  tokens), so compare runs from the same day only.
- Cost is 3% lower on average (0.1786 vs 0.1849 USD), but this is not like for
  like. Both nine-server runs stopped short: the Libellus and Adsertio stores
  did not exist, their `init` needs a shell command the run may not use, and
  the private note and the claim were never written. Both harness runs created
  the stores with `harness_init` and did all five steps.
- The 12% saving measured on 0.1.0 did not hold. Read the harness's gain as the
  task being finished in a fresh project at about the same cost, and 30% fewer
  tools in context.
