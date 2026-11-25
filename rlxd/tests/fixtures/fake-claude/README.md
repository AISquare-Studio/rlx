# fake-claude

Stand-ins for the `claude` binary, so rlxd's checks run in CI on three OSes with no
model turn (handoff 0.10.3, "How to read the checks").

`RLXD_CLAUDE_BIN=rlxd/tests/fixtures/fake-claude/<scenario>` makes rlxd run one in
place of `claude`. Each scenario replays recorded stream-json, in the shape rlxapp's
architecture recorded (§16, E6).

| Scenario | What it is |
|---|---|
| `auth-status` | Claude Code 2.1.314, signed in with claude.ai as `probe@example.com` on the `pro` plan. It answers `--version` and `auth status --json`, which `status` runs (7.4) |

The runner (2.1, 7.5) adds the scenarios its checks name: `hello`, `asks`, `crash`,
`slow`, `print-env`, `init-connectors`, `mcp-login`.

Windows has no exec bit and `which`-style lookups need PATHEXT: give each scenario a
form Windows can start, not a bare script. Each scenario here is a POSIX shell script
(`<scenario>`, executable) beside a batch file (`<scenario>.cmd`, CRLF). On Windows,
rlxd tries a name without an extension with each of `PATHEXT`'s, so the one
`RLXD_CLAUDE_BIN=…/<scenario>` serves every OS.
