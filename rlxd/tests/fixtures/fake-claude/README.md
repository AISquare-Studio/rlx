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
| `auth-status`, `signed-out`, `probe-hangs`, `auth-env` | What `claude --version` and `claude auth status --json` answer, for `status` (7.4): signed in as `probe@example.com` on pro (every other scenario answers so too); signed out (exit 1); neither answers for a minute; signed in as `probe+<CLAUDE_CONFIG_DIR>@example.com`. A run started as one plays `hello` |
| `print-env` | Answers with the app's environment it got (4.3): `CLAUDE_CONFIG_DIR`, `CLAUDE_SECURESTORAGE_CONFIG_DIR` and `PATH`, those set, as JSON. Started by a name that is no scenario, such as `bin/claude`, fake-claude follows the name's links to its scenario, else takes `FAKE_CLAUDE_SCENARIO` (Windows, where the tests copy it) |

Each writes `$AISQUARE_HOME/fake-claude.log.json` in the handoff's shape: see the crate's
documentation for each field. Like Claude Code, every scenario exits 1 for
`-p --output-format stream-json` without `--verbose`, waits for its first stream-json user
message before `system/init`, and also writes a message type and fields rlxd must ignore.

To add a scenario (the gate's, 5.1, the connectors', 5.5): a module in
`crates/fake-claude/src/scenarios/` and a line in its table.

Windows has no exec bit and `which`-style lookups need PATHEXT: give each scenario a
form Windows can start, not a bare script. Each scenario here is a POSIX shell script
(`<scenario>`, executable) beside a batch file (`<scenario>.cmd`, CRLF). On Windows,
rlxd tries a name without an extension with each of `PATHEXT`'s, so the one
`RLXD_CLAUDE_BIN=…/<scenario>` serves every OS.
