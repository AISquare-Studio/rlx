# Owner-run probes

Probes run the real Claude Code, signed in, so CI never runs them. Each says which of its
steps make a model turn, and none prints a prompt's answer, a connector's content or a
URL's query.

## `connectors_route.sh`: rlxd's connectors, phase 1

For task #36 (handoff 0.10.3, 2.4, 5.3's listing, 5.5 and 7.7). It needs Claude Code
signed in with a claude.ai connector connected (`CONNECTOR`, default `claude.ai Slack`),
a built rlxd (`cargo build -p rlxd`), `jq`, and `uvx` for P7's schema check.

```sh
rlxd/tests/probes/connectors_route.sh            # P1-P3, P5-P7
rlxd/tests/probes/connectors_route.sh --login    # and P4, a real `claude mcp login`
```

Every Claude Code it starts itself (P1, P5) runs in an empty folder with no user or local
settings (`--setting-sources project`), asks for no permission (`--permission-mode
dontAsk`), and has each tool call it attempts denied by rlxd's own probe hook (`rlxd
__probe-deny`). P6 and P7 go through rlxd. `--login` runs a real `claude mcp login`, which
signs the connector in again: finishing it in the browser replaces its sign-in.

It keeps what it saw in a temp folder it names, and writes `summary.tsv` there. The steps:

| Step | What it finds | What rlxd does with it |
|---|---|---|
| P1 | Whether `system/init` comes before the first message under `--input-format stream-json` (one turn) | Route 1 (`connectors.gate_before_turn`) only if yes; route 2, the default, stays otherwise |
| P2 | Whether `system/init`'s tools carry MCP annotations, and that a connector's tools are under `mcp__<name>__`, the name as rlxd writes it (non-`[A-Za-z0-9_-]` as `_`; for a `claude.ai ` connector, runs of `_` as one and none at the ends) | `read_only_hint` is `null` unless they do (5.3); the cache's tools are those under the prefix |
| P3 | That `--max-turns 1` is taken | The one-turn probe's command line |
| P4 | `claude mcp login <name>`'s output: a URL, `https://`, its masked form (`--login` only; it signs the connector in again) | `connectors.login` reads the first URL on the PTY's screen |
| P5 | Whether a second start with the same `--session-id` is refused while the first's session file is there, and taken once it is removed (three turns) | After a first attempt's missed try, rlxd removes its session file; a resumed attempt's file is only cut back to its length before the attempt |
| P6 | 5.5's `[real, connectors]`: `RUNS` runs of a one-word prompt with `needs_connectors` (`RUNS` turns at most) | Each must end `run.done` with the connector connected, or `run.failed {reason: "connectors"}` naming it; none `run.done` without it |
| P7 | 7.7's `[real, connectors]` list: `connectors.list` valid against `connectors.json`, whether any hint is not `null`, and a one-turn refresh (one turn) | `connectors.list` |

P6's "otherwise" count must be 0. P1's and P2's answers decide the route rlxd announces
and whether hints are read; P5's whether the removal is needed.
