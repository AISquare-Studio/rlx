# fake-brain

Stand-ins for `rlx brain --stdio`, the resident Python brain rlxd supervises (handoff
0.10.3, 1.6), for the checks a real rlx cannot fail on purpose.

`RLXD_RLX_BIN=<absolute path>` makes rlxd run one in place of the `rlx` it would find.
Each scenario is a POSIX shell script (`<scenario>`, executable) beside a batch file
(`<scenario>.cmd`, CRLF); the tests name the one for their OS. A scenario that reads
rlxd's frames runs a PowerShell script (`<scenario>.ps1`) from its batch file, since cmd's
`set /p` reads at most 1023 characters of a line, and rlxd's hello, naming the hook
variables, is longer.

| Scenario | What it is |
|---|---|
| `dies` | Exits 1 as it starts, before any hello: a crash loop, which rlxd restarts with backoff |
| `skewed` | Answers rlxd's hello with brain protocol 99, a release this rlxd does not speak: `brain_incompatible` |
| `mute` | Says a good hello, then never accepts a request: rlxd kills it, starts it again, and the request was never delivered |
| `accepts-then-dies` | Accepts its first request, then dies before it answers: the request failed, and was not "never delivered" |
| `accepts-unterminated` | Writes its first request's `accepted` without the line's end, then stalls: rlxd's accept wait ends with it unread, and after the kill it is read, so the request timed out and was not "never delivered" |
| `spawns-child` | Starts a child that would run five minutes (its pid in `$AISQUARE_HOME/fake-brain-child.pid`), then says a good hello: rlxd's stop must end the child with it |
| `print-env` | Says a good hello, then answers each request with its pid and the `CLAUDE_CONFIG_DIR` it was started with (4.3). Once it has accepted a request it touches `$AISQUARE_HOME/fake-brain-busy`, and holds the reply while `$AISQUARE_HOME/fake-brain-hold` exists: a check changes the app's environment while a request runs |

The checks that need a brain that works run the real `rlx brain --stdio` of the checkout
(`uv run`), as `tests/brain.rs` says.
