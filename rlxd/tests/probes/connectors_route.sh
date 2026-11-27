#!/usr/bin/env bash
# connectors_route.sh: the owner-run probe of rlxd's connectors, phase 1 (task #36;
# handoff 0.10.3, 2.4, 5.3's listing, 5.5 and 7.7). See README.md beside it.
#
# It runs the real Claude Code, signed in, with a claude.ai connector connected, so CI
# never runs it. Each step that makes a model turn says so. It never prints a prompt's
# answer, a connector's content, or a URL's query: what it keeps is in $OUT, and what it
# prints is statuses, counts and yes/no.
#
#   rlxd/tests/probes/connectors_route.sh            P1-P3, P5, P6 and P7
#   rlxd/tests/probes/connectors_route.sh --login    P4 as well: a real `claude mcp login`,
#                                                    which signs the connector in again
#
# Every Claude Code it starts itself (P1, P5) runs in an empty folder with no user or
# local settings (`--setting-sources project`), asks for no permission
# (`--permission-mode dontAsk`), and has each tool call it attempts denied by rlxd's own
# probe hook (`rlxd __probe-deny`), as rlxd's one-turn refresh does. P6 and P7 go through
# rlxd, which starts Claude Code its own way.
#
# CLAUDE      Claude Code (default: `command -v claude`)
# RLXD        a built rlxd (default: rlxd/target/debug/rlxd; `cargo build -p rlxd` first)
# CONNECTOR   the connector to probe (default: "claude.ai Slack")
# RUNS        P6's runs (default: 5)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)
CLAUDE=${CLAUDE:-$(command -v claude || true)}
RLXD=${RLXD:-$repo/rlxd/target/debug/rlxd}
CONNECTOR=${CONNECTOR:-claude.ai Slack}
RUNS=${RUNS:-5}
LOGIN=0
[ "${1:-}" = "--login" ] && LOGIN=1

for need in jq mkfifo uuidgen; do
  command -v "$need" >/dev/null || { echo "needs $need" >&2; exit 2; }
done
[ -x "$CLAUDE" ] || { echo "no Claude Code: set CLAUDE" >&2; exit 2; }
[ -x "$RLXD" ] || { echo "no rlxd at $RLXD: cargo build -p rlxd, or set RLXD" >&2; exit 2; }
RLXD=$(cd "$(dirname "$RLXD")" && pwd)/$(basename "$RLXD")
case "$RLXD" in
  *[\"\$\`\\]*) echo "rlxd's path holds a character a hook's shell would read" >&2; exit 2;;
esac

OUT=$(mktemp -d "${TMPDIR:-/tmp}/rlxd-connectors-probe.XXXXXX")
echo "probe: results in $OUT"
PROMPT='Reply with the single word OK.'
user_line() {
  jq -cn --arg t "$1" '{type:"user",message:{role:"user",content:[{type:"text",text:$t}]},parent_tool_use_id:null}'
}
# The prefix Claude Code mounts a server's tools under, as rlxd expects it: each
# character but a letter, a digit, _ or - as _, and for a claude.ai connector runs of _
# as one and none at the ends.
written=$(printf '%s' "$CONNECTOR" | sed 's/[^A-Za-z0-9_-]/_/g')
case "$CONNECTOR" in "claude.ai "*) written=$(printf '%s' "$written" | sed -E 's/_+/_/g; s/^_//; s/_$//');; esac
prefix="mcp__${written}__"
# The settings of every Claude Code this script starts itself: each tool call denied.
deny="$OUT/deny-all.json"
jq -n --arg c "\"$RLXD\" __probe-deny" \
  '{hooks: {PreToolUse: [{matcher: "*", hooks: [{type: "command", command: $c, timeout: 10}]}]}}' > "$deny"
safe=(--setting-sources project --permission-mode dontAsk --settings "$deny")
mask() { sed -E 's#(https?://[^ ?#]*)[?#][^ ]*#\1?<query masked>#g'; }
verdict() { printf '%-4s %s\n' "$1" "$2"; printf '%s\t%s\n' "$1" "$2" >> "$OUT/summary.tsv"; }

# --- P1-P3: what Claude Code says before and after the first message (one turn) ---------
# rlxd's route 1 needs states before the first message; route 2 reads the system/init
# that follows it. The init after it also says whether tools carry annotations.
# In an empty folder, so no project's settings or hooks apply.
space=$(mktemp -d "$OUT/space.XXXXXX")
mkfifo "$OUT/p1.in"
(cd "$space" && exec "$CLAUDE" -p --verbose --output-format stream-json --input-format stream-json \
  --no-session-persistence --tools "" --max-turns 1 "${safe[@]}" \
  < "$OUT/p1.in" > "$OUT/p1.jsonl" 2> "$OUT/p1.err") &
p1=$!
exec 3> "$OUT/p1.in"
sleep 12
before=$(jq -s '[.[] | select(.type == "system" and .subtype == "init")] | length' "$OUT/p1.jsonl")
before_status=$(jq -rs --arg c "$CONNECTOR" '[.[] | select(.type == "system" and .subtype == "init")
  | .mcp_servers[]? | select(.name == $c) | .status] | join(",")' "$OUT/p1.jsonl")
user_line "$PROMPT" >&3
for _ in $(seq 120); do
  grep -q '"type":"result"' "$OUT/p1.jsonl" 2>/dev/null && break
  sleep 0.5
done
exec 3>&-
wait "$p1" 2>/dev/null || true
init=$(jq -cs '[.[] | select(.type == "system" and .subtype == "init")] | last // {}' "$OUT/p1.jsonl")
verdict P1 "route 1 (states before the first message): $([ "$before" -gt 0 ] && echo "YES, $before init(s), $CONNECTOR: ${before_status:-absent}" || echo "no: route 2 stays")"
verdict P1 "after the message: $CONNECTOR is $(jq -r --arg c "$CONNECTOR" '[.mcp_servers[]? | select(.name == $c) | .status] | first // "absent"' <<<"$init")"
verdict P2 "init's tools are $(jq -r '[.tools[]? | type] | unique | join(",")' <<<"$init"); with readOnlyHint: $(jq -r '[.tools[]? | objects | select(.annotations.readOnlyHint != null)] | length' <<<"$init")"
verdict P2 "tools under $prefix: $(jq -r --arg p "$prefix" '[.tools[]? | (if type == "object" then .name else . end) | select(startswith($p))] | length' <<<"$init")"
verdict P3 "--max-turns 1 taken: $(grep -qi 'unknown option\|error: ' "$OUT/p1.err" && echo "NO (see p1.err)" || echo yes)"

# --- P4: claude mcp login, in a terminal (--login only) ----------------------------------
# It signs the connector in again: finishing it in the browser replaces its sign-in.
if [ "$LOGIN" = 1 ]; then
  echo "P4: a real 'claude mcp login $CONNECTOR', which signs it in again: finish or close the browser when it opens"
  if [ "$(uname)" = Darwin ]; then
    script -q "$OUT/p4.raw" "$CLAUDE" mcp login "$CONNECTOR" </dev/tty || true
  else
    script -q -c "$(printf '%q mcp login %q' "$CLAUDE" "$CONNECTOR")" "$OUT/p4.raw" </dev/tty || true
  fi
  mask < "$OUT/p4.raw" > "$OUT/p4.txt"
  rm -f "$OUT/p4.raw"
  url=$(grep -Eo 'https?://[^ ]+' "$OUT/p4.txt" | head -1 || true)
  verdict P4 "claude mcp login printed a URL: ${url:+yes, ${url%%\?*}}${url:-NO}"
  verdict P4 "its URL is https: $(case "$url" in https://*) echo yes;; '') echo -;; *) echo NO;; esac)"
fi

# --- P5: a gated try ended, then the same --session-id again (three turns at most) -------
# After a first attempt's missed try, rlxd removes the session file it left, so the next
# try's --session-id starts anew. (A resumed attempt's tries are --resume, and rlxd only
# cuts its session file back to its length before the attempt.)
sid=$(uuidgen | tr 'A-Z' 'a-z')
try() {
  (cd "$space" && printf '%s\n' "$(user_line "$PROMPT")" | "$CLAUDE" -p --verbose \
    --output-format stream-json --input-format stream-json --session-id "$sid" --tools "" \
    --max-turns 1 "${safe[@]}" > "$OUT/p5-$1.jsonl" 2> "$OUT/p5-$1.err") || true
}
try 1
try 2
verdict P5 "the same --session-id again: $(grep -qi 'already in use' "$OUT/p5-2.err" && echo "REFUSED: rlxd's removal of the session file is needed" || echo taken)"
config=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
encoded=$(printf '%s' "$(cd "$space" && pwd -P)" | sed 's/[^A-Za-z0-9]/-/g')
rm -f "$config/projects/$encoded/$sid.jsonl"
try 3
verdict P5 "after removing the session file: $(grep -qi 'already in use' "$OUT/p5-3.err" && echo REFUSED || echo taken)"
rm -f "$config/projects/$encoded/$sid.jsonl"
rmdir "$config/projects/$encoded" 2>/dev/null || true

# --- P6 and P7: through rlxd, the handoff's [real] lines (RUNS + 1 turns) -----------------
home=$(mktemp -d "$OUT/home.XXXXXX")
export AISQUARE_HOME=$home RLXD_CLAUDE_BIN=$CLAUDE
unset RLX_HOME
"$RLXD" serve --detach >/dev/null
trap '"$RLXD" stop >/dev/null 2>&1 || true' EXIT
verdict P6 "rlxd's capabilities: $("$RLXD" call status | jq -c '.data.capabilities')"
done_with=0 failed_naming=0 other=0
for _ in $(seq "$RUNS"); do
  r=$(uuidgen | tr 'A-Z' 'a-z')
  s=$("$RLXD" call status | jq .data.seq)
  "$RLXD" events --topics run --since "$s" --follow --until run.done,run.failed --timeout 120 > "$OUT/p6-$r.jsonl" &
  cap=$!
  sleep 0.5
  params=$(jq -cn --arg r "$r" --arg c "$CONNECTOR" '{run_id:$r, kind:"widget_refresh", prompt:"OK",
    mount:{kind:"connectors", connectors:[$c]}, needs_connectors:[$c], space:"default"}')
  "$RLXD" call run.start "$params" >/dev/null
  wait "$cap" || true
  end=$(jq -s --arg r "$r" --arg c "$CONNECTOR" '
    (map(select(.data.run_id == $r)) | last) as $end
    | (map(select(.type == "run.started" and .data.run_id == $r)) | first) as $started
    | if $end.type == "run.done" and ([$started.data.mcp_servers[]? | select(.name == $c and .status == "connected")] | length) == 1 then "done_with"
      elif $end.type == "run.failed" and $end.data.reason == "connectors" and ([$end.data.connectors[]? | select(.id == $c)] | length) == 1 then "failed_naming"
      else "other" end' "$OUT/p6-$r.jsonl" | tr -d '"')
  case "$end" in done_with) done_with=$((done_with + 1));; failed_naming) failed_naming=$((failed_naming + 1));; *) other=$((other + 1));; esac
done
verdict P6 "$RUNS runs: $done_with done with $CONNECTOR connected, $failed_naming failed naming it, $other otherwise (must be 0)"
"$RLXD" call connectors.list > "$OUT/c.json"
if command -v uvx >/dev/null; then
  verdict P7 "connectors.list valid against connectors.json: $(uvx --quiet check-jsonschema --schemafile "$repo/rlxd/schemas/v1/connectors.json" <(jq .data "$OUT/c.json") >/dev/null 2>&1 && echo yes || echo NO)"
fi
verdict P7 "listed: $(jq -r '[.data.connectors[] | "\(.source) \(.status) \(.tools | length) tools"] | join("; ")' "$OUT/c.json")"
verdict P7 "hints not null: $(jq '[.data.connectors[].tools[] | select(.read_only_hint != null)] | length' "$OUT/c.json")"
probed=$("$RLXD" call connectors.list '{"refresh":true,"allow_turn":true}' | jq -c '{ok, model_turns: .data.model_turns, code: .error.code}')
verdict P7 "a one-turn refresh: $probed"
echo "probe: done; the summary is $OUT/summary.tsv"
