#!/usr/bin/env bash
# connector_map.sh: the owner-run probe of rlxd's connector map (task #14, acceptance 6).
# See README.md beside it.
#
# It reads the real accounts' connectors with no model turn: it makes no turn and calls no
# connector's tool. It prints slots, connector names, kinds and statuses only: never an
# email, an account id, a login mark or a home's path. $OUT keeps only that (summary.tsv
# and map.txt); what it reads to get there (the brain's reply, rlxd's temp home, `claude
# mcp list`'s output) is in a private temp folder it deletes when it ends.
#
#   rlxd/tests/probes/connector_map.sh
#
# M1  the brain's `accounts` (this checkout's), read-only: which slots, and whether each
#     has a login mark
# M2  a test build of rlxd, in a temp home, with the no-turn route on: connectors.list
#     {refresh: true}, valid against connectors.json, model_turns 0, and each account's
#     connectors (the table to hold against the brief's)
# M3  whether user-scope local MCP servers (kind `mcp`) show up under rlxd's
#     `--setting-sources project`, against `claude mcp list` in each account's home
#
# CLAUDE  Claude Code (default: `command -v claude`)
# RLXD    rlxd built with its test methods (default: rlxd/target/debug/rlxd, after
#         `cargo build -p rlxd --features debug-methods`)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)
CLAUDE=${CLAUDE:-$(command -v claude || true)}
RLXD=${RLXD:-$repo/rlxd/target/debug/rlxd}

for need in jq uv; do
  command -v "$need" >/dev/null || { echo "needs $need" >&2; exit 2; }
done
[ -x "$CLAUDE" ] || { echo "no Claude Code: set CLAUDE" >&2; exit 2; }
[ -x "$RLXD" ] || {
  echo "no rlxd at $RLXD: cargo build -p rlxd --features debug-methods, or set RLXD" >&2
  exit 2
}

OUT=$(mktemp -d "${TMPDIR:-/tmp}/rlxd-connector-map.XXXXXX")
RAW=$(mktemp -d "${TMPDIR:-/tmp}/rlxd-connector-map-raw.XXXXXX")
chmod 700 "$RAW"
RLXD_STARTED=0
cleanup() {
  [ "$RLXD_STARTED" = 1 ] && "$RLXD" stop >/dev/null 2>&1 || true
  rm -rf "$RAW"
}
trap cleanup EXIT
echo "probe: results in $OUT"
verdict() { printf '%-4s %s\n' "$1" "$2"; printf '%s\t%s\n' "$1" "$2" >> "$OUT/summary.tsv"; }

# --- M1: the brain's accounts, read-only ---------------------------------------------------
uv run --quiet --project "$repo" python -c '
import json
from aisquare.services import brain
print(json.dumps(brain.answer("accounts", {}, brain.METHODS)))
' > "$RAW/brain.json"
jq -e '.ok == true' "$RAW/brain.json" >/dev/null || { echo "the brain refused accounts" >&2; exit 1; }
jq '.result.accounts' "$RAW/brain.json" > "$RAW/accounts.json"
verdict M1 "the brain lists $(jq length "$RAW/accounts.json") Claude Code accounts: slots $(jq -c 'map(.slot)' "$RAW/accounts.json"); signed in (a mark): $(jq -c 'map(.login != null)' "$RAW/accounts.json")"

# --- M2: rlxd's no-turn read of each account ------------------------------------------------
home=$(mktemp -d "$RAW/home.XXXXXX")
export RLX_HOME=$home RLXD_CLAUDE_BIN=$CLAUDE
export RLXD_TEST_CONNECTORS_NO_TURN=1 RLXD_TEST_ACCOUNTS=$RAW/accounts.json
export RLXD_TEST_CONNECTOR_MAP_MS=off
"$RLXD" serve --detach >/dev/null
RLXD_STARTED=1
verdict M2 "rlxd announces connectors.no_turn: $("$RLXD" call status | jq '.data.capabilities | index("connectors.no_turn") != null')"
started=$(date +%s)
"$RLXD" call connectors.list '{"refresh":true}' > "$RAW/map.json" || true
verdict M2 "the refresh: $(jq -c '{ok, model_turns: .data.model_turns, code: .error.code}' "$RAW/map.json"), $(($(date +%s) - started)) s"
if command -v uvx >/dev/null; then
  verdict M2 "valid against connectors.json: $(uvx --quiet check-jsonschema --schemafile "$repo/rlxd/schemas/v1/connectors.json" <(jq .data "$RAW/map.json") >/dev/null 2>&1 && echo yes || echo NO)"
fi
jq -r '.data.accounts[]? | "slot \(.account): from \(.from), error \(.error.code // "none")",
  (.connectors[] | "  \(.id)\t\(.source)\t\(.status)\t\(.tools | length) tools")' "$RAW/map.json" \
  | tee "$OUT/map.txt"
verdict M2 "the table above is in $OUT/map.txt: hold it against the brief's"

# --- M3: local MCP servers under --setting-sources project ----------------------------------
for slot in $(jq -r '.[].slot' "$RAW/accounts.json"); do
  dir=$(jq -r --argjson s "$slot" '.[] | select(.slot == $s) | .config_dir // ""' "$RAW/accounts.json")
  # `claude mcp list` checks each server's health: it starts each local one.
  if [ -n "$dir" ]; then
    CLAUDE_CONFIG_DIR=$dir "$CLAUDE" mcp list > "$RAW/mcp-list-$slot.txt" 2>/dev/null || true
  else
    env -u CLAUDE_CONFIG_DIR -u CLAUDE_CODE_TMPDIR "$CLAUDE" mcp list > "$RAW/mcp-list-$slot.txt" 2>/dev/null || true
  fi
  listed=$(grep -c ': ' "$RAW/mcp-list-$slot.txt" | tr -d ' ' || true)
  local_listed=$(grep ': ' "$RAW/mcp-list-$slot.txt" | grep -vc '^claude\.ai ' | tr -d ' ' || true)
  read_local=$(jq --argjson s "$slot" '[.data.accounts[]? | select(.account == $s) | .connectors[] | select(.source == "mcp")] | length' "$RAW/map.json")
  verdict M3 "slot $slot: claude mcp list shows $listed servers, $local_listed local; rlxd's read shows $read_local local (kind mcp)"
done
echo "probe: done; the summary is $OUT/summary.tsv"
