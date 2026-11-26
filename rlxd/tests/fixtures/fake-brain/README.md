# fake-brain

Stand-ins for `rlx brain --stdio`, the resident Python brain rlxd supervises (handoff
0.10.3, 1.6), for the checks a real rlx cannot fail on purpose.

`RLXD_RLX_BIN=<absolute path>` makes rlxd run one in place of the `rlx` it would find.
Each scenario is a POSIX shell script (`<scenario>`, executable) beside a batch file
(`<scenario>.cmd`, CRLF); the tests name the one for their OS.

| Scenario | What it is |
|---|---|
| `dies` | Exits 1 as it starts, before any hello: a crash loop, which rlxd restarts with backoff |
| `skewed` | Answers rlxd's hello with brain protocol 99, a release this rlxd does not speak: `brain_incompatible` |
| `mute` | Says a good hello, then never accepts a request: rlxd kills it, starts it again, and the request was never delivered |
| `accepts-then-dies` | Accepts its first request, then dies before it answers: the request failed, and was not "never delivered" |

The checks that need a brain that works run the real `rlx brain --stdio` of the checkout
(`uv run`), as `tests/brain.rs` says.
