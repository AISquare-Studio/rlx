# fake-claude

Stand-ins for the `claude` binary, so rlxd's checks run in CI on three OSes with no
model turn (handoff 0.10.3, "How to read the checks").

`RLXD_CLAUDE_BIN=rlxd/tests/fixtures/fake-claude/<scenario>` makes rlxd run one in
place of `claude`. Each scenario replays recorded stream-json, in the shape rlxapp's
architecture recorded (§16, E6).

| Scenario | What it is |
|---|---|
| `auth-status` | Claude Code 2.1.314, signed in with claude.ai as `probe@example.com` on the `pro` plan. It answers `--version` and `auth status --json`, which `status` runs (7.4) |

## The runner's scenarios: one binary

The runner's scenarios (2.1, 2.5, 7.5) are one cross-platform binary,
`rlxd/crates/fake-claude`, so no shell, Python or batch file stands between rlxd and
the stand-in on any OS. It is built as rlxd's `fake-claude` [[bin]], which only the
test-only `fake-claude` feature builds, so `cargo test` gives the tests its path
(`CARGO_BIN_EXE_fake-claude`) and no release build has it.

**The scenario is the name it is started as.** A test links (or copies) the binary to
`<home>/bin/fake-claude/<scenario>` (`.exe` on Windows) and points `RLXD_CLAUDE_BIN`
there, so a run's command line names its scenario (`pgrep -f fake-claude/slow`). The
handoff's `RLXD_CLAUDE_BIN=rlxd/tests/fixtures/fake-claude/<scenario>` can't serve here:
`structured/` holds the recorded output, so that path is a folder. A scenario's recorded
data stays in its folder here, compiled into the binary, and the checks compare against
the same files.

| Scenario | What it does |
|---|---|
| `hello` | One answer, then a successful result |
| `slow` | Says it is working, then takes `FAKE_CLAUDE_SLOW_MS` (default 60,000) before it answers; the prompt `quick` makes it answer at once. It ignores stdin closing, so only rlxd ending its tree ends it |
| `structured` | With `--json-schema`, its result's `structured_output` is `structured/output.json` |
| `rate-limit` | A `rate_limit_event` saying the limit is hit (`rejected`; `FAKE_CLAUDE_RATE_STATUS` gives another, such as `allowed_warning`), five minutes before it resets, then an answer |
| `asks` | Ends with the output `{"status":"needs_you","question":"Which channel?"}`; resumed, it reads the reply and ends `{"status":"done","channel":<reply>}` |
| `crash` | Says part of an answer, then exits 1 with no result |
| `persist` | `hello`, named for the erase check: every scenario writes its session file where Claude Code does unless `--no-session-persistence` |
| `orphan` | Starts a process of its own that would linger a minute, answers, and exits: the run's end must take that process too |
| `abort` | Says part of an answer, waits `FAKE_CLAUDE_ABORT_AFTER_MS` (default 0), then dies of `SIGABRT` (an abort), so rlxd reports the signal |
| `huge` | A successful result of 1.5 MiB of text, more than `run.done` carries |
| `tool-result` | A recorded connector call and its `tool_result` block, whose content is `tool-result/content.json`. It runs no tool and asks no hook |

### The connectors' scenarios (5.5 and 7.7)

One connector, `claude.ai Slack`, with made-up tools (`init-connectors/expected-tools.json`). Its status follows the fixture clock from the run's first start, and doesn't start again with each try. As Claude Code does, a start writes `system/init` after its first message, listing Slack's status then and mounting its tools only when connected. With `RLXD_TEST_CONNECTOR_ROUTE=1` (rlxd's route-1 switch, which rlxd passes on), it plays a Claude Code that also reports the states before the first message: an init as it starts and another at each change. `connected_at` is when the start saw Slack connected. With `FAKE_CLAUDE_TRY_TOOL=<tool>`, a start that sees Slack connected asks its `--settings`' PreToolUse hooks about one call of that tool, and logs the answer in `hook_replies`.

| Scenario | What it does |
|---|---|
| `init-connectors` | Slack connected from the start |
| `init-needs-auth` | Slack `needs-auth`, its tools not mounted |
| `connectors-late` | Slack `pending`, then connected from 3 s |
| `connectors-later` | Slack `pending`, then connected from 8 s |
| `connectors-never` | Slack `pending` throughout |
| `connectors-needs-auth` | Slack `needs-auth` throughout |
| `stdin-eof-empty` | No servers in `system/init` when stdin is at EOF by then, as E9 found; Slack connected while it is open |
| `mcp-login` | `claude mcp login <name>`: prints an `https://` URL whose query holds made-up secrets, waits, says it signed in, and exits 0 |
| `mcp-login-fails` | The same URL, then says the sign-in failed, and exits 1 |
| `mcp-login-http` | Prints an `http://` URL, then waits a minute |
| `mcp-login-silent` | Prints no URL, and exits 0 |

`FAKE_CLAUDE_SCENARIO_FILE=<file>` names a file holding the scenario's name, read at each start, over the binary's name: so one rlxd plays one scenario, then another.
**The gate's scenarios** (5.1) make a tool call as Claude Code does: the `tool_use`, then
the PreToolUse hook the run's `--settings` names, run with the call as JSON on its stdin
(split into words, no shell) and started detached as Claude Code starts it (a session and
process group of its own; on Windows, detached from the console in a group of its own),
then the tool only on an explicit `allow`, else the hook's reason as the call's error
result. Each answer is added to `hook_replies` (never the
key), and each tool run to `tool_runs`. `system/init` lists the send tool
(`readOnlyHint: false`), the read tool (`true`) and one with no annotations (`null`).
Each is a scenario of its own name, and `gate` plays the one its prompt names, so one
home runs them all. With `FAKE_CLAUDE_KEY_OUT` set to a file outside the home, each start
adds its `RLXD_HOOK_KEY` there, for the check that rlxd writes it nowhere.

| Scenario | What it does |
|---|---|
| `slack-send` | Says text claiming approval (which decides nothing), then calls the send tool with `gate/input.json`; resumed, it calls again under another id |
| `slack-read` | Calls the read tool |
| `write-call` | Calls the tool with no hint: a write |
| `send-twice` | Calls the send tool twice, the same input, two ids |
| `send-parallel` | Calls the send tool twice in one message, the same input, two ids, asking the hook about both at once |
| `send-fails` | Calls the send tool; allowed, the tool fails |
| `crash-in-call` | Calls the send tool; allowed, exits 1 mid-call |
| `tool-hang` | Calls the send tool; allowed, the tool hangs two minutes |
| `forge` | First asks the hook itself about a call of its own with the real call's id, then makes the real call |
| `bad-hooks` | Asks the hook with a wrong key, no key, a wrong attempt, and the run named in `./other-run`, then makes a real call |
| `lookalike-nested`, `lookalike-evil` | Init lists the Slack connector and a local server `claude_ai_Slack__x` (or `__evil`); the call is to a tool of the local one |
| `lookalike-same`, `lookalike-local` | Init lists a local server `claude_ai_Slack` beside the connector, or alone; the call is to `mcp__claude_ai_Slack__post` |
| `beta-send` | Init lists `claude.ai Slack (Beta)`; the call is to its tool, `mcp__claude_ai_Slack_Beta__send` |
| `impostor` | `slack-send`, but init's one server named `claude.ai Slack` is a local one (`source: "local"`), not the claude.ai connector |
| `evil-unlisted` | Init lists only the Slack connector and its tools; the call is to `mcp__claude_ai_Slack__evil__delete_everything`, which the init doesn't list |

Each look-alike scenario's init lists its own tool, as Claude Code would, and every
server carries its `source` as Claude Code 2.1.332 writes it (`claudeai` for a connector).

Resumed, a scenario that makes one call makes it under another id.

**`watch-call`** plays a watch job's run (7.6) as a model would. It reads the connector,
the tool and the input from the JSON rlxd quotes in its message, lists that tool
(read-only, of that connector, from claude.ai) in `system/init`, and makes the call through
the hook. Allowed, the tool gives `watch-call/content.json`, which `job.watch_result`
carries; for the input `{"ref": "missing"}` an error, and for `{"ref": "huge"}` 2 MiB of
text, as a tool would. Then it tries the identical call again, which rlxd denies `pinned_call_used`.

Each writes `$AISQUARE_HOME/fake-claude.log.json` in the handoff's shape: see the crate's
documentation for each field. Like Claude Code, every scenario exits 1 for
`-p --output-format stream-json` without `--verbose`, waits for its first stream-json user
message before `system/init`, and also writes a message type and fields rlxd must ignore.

To add a scenario (the connectors', 5.5): a module in
`crates/fake-claude/src/scenarios/` and a line in its table.

Windows has no exec bit and `which`-style lookups need PATHEXT: give each scenario a
form Windows can start, not a bare script. Each scenario here is a POSIX shell script
(`<scenario>`, executable) beside a batch file (`<scenario>.cmd`, CRLF). On Windows,
rlxd tries a name without an extension with each of `PATHEXT`'s, so the one
`RLXD_CLAUDE_BIN=…/<scenario>` serves every OS.
