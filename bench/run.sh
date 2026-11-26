#!/bin/sh
# usage: run.sh <name> <mcp.json> <settings.json> <allowed tools>
S=$(dirname "$0"); n=$1
cd "$S/proj-$n" || exit 1
claude -p "$(cat "$S/task.txt")" --model claude-sonnet-5-5 \
  --mcp-config "$2" --strict-mcp-config --settings "$3" --setting-sources project \
  --allowedTools $4 Read Grep Glob \
  --output-format stream-json --verbose --no-session-persistence --max-budget-usd 3 \
  > "$S/out-$n.jsonl" 2> "$S/err-$n.txt"
echo "$n exit $?" >> "$S/done.txt"
