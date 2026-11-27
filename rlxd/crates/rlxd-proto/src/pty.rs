//! The PTY host (handoff 0.10.3, 1.4): processes run on pseudo-terminals that rlxd holds,
//! as tmux held them, so they outlive every client.
//!
//! - `pty.spawn {argv, cwd?, env?, cols, rows, label?}` → [`PtyRef`]
//! - `pty.write {pty_id, text | keys | paste}` → [`PtyRef`]: `text` is typed as it is,
//!   `keys` are tmux's key names, and `paste` is a bracketed paste, as `paste-buffer -p`
//!   makes one
//! - `pty.resize {pty_id, cols, rows}` → [`PtyRef`]
//! - `pty.screen {pty_id, scrollback?}` → [`PtyScreen`]: the screen as `tmux
//!   capture-pane -e -p` prints it, byte for byte
//! - `pty.subscribe {pty_id, read_only}`: a stream of the screen ([`PtyScreen`] as
//!   `pty.snapshot`), then the PTY's output and its end
//! - `pty.kill {pty_id}` → [`PtyRef`]: ends the process and every process in its group or
//!   job, and frees the PTY
//! - `pty.list` → [`PtyList`]
//! - the `pty` topic carries `pty.output` ([`PtyOutput`]) and `pty.exited`
//!   ([`PtyExited`])
//!
//! A `pty_id` is 128 random bits, as 32 lowercase hex digits: a client can't guess one it
//! was not given. A process that ended keeps its last screen and its exit status until
//! `pty.kill`, as tmux's `remain-on-exit on` keeps a pane.
//!
//! **Who may do what.** `cli` and `tui` connections may use every method. An `app`
//! connection may only list, read the screen and subscribe with `read_only: true`, and a
//! `remote` one may also write. Any other call gets `role_not_allowed`. A connection
//! subscribed to a PTY read-only may not write to it (`read_only`).

use std::collections::BTreeMap;

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Value, json};

use crate::check::{self, Invalid, drop_null, present};
use crate::envelope::Data;

/// The widest and tallest a PTY may be, in cells.
pub const MAX_SIZE: u16 = 1000;

/// The most bytes one `pty.write` carries: its `text`, its `paste`, or its keys' names:
/// 128 KiB, an eighth of the 1 MiB frame rlxd reads (#2574 minor 5). JSON takes at most
/// six bytes for one (`\u001b`), so the longest write there is, its envelope with it,
/// fits a frame: one past it gets `write_too_large`, and a frame past 1 MiB
/// `frame_too_large`, both on a connection that stays open.
pub const MAX_WRITE: usize = 128 << 10;

/// The most lines of history a PTY keeps above its screen, as the fleet's tmux keeps
/// (`history-limit 50000`).
pub const MAX_SCROLLBACK: u32 = 50_000;

/// The longest `label`, in characters.
pub const MAX_LABEL: usize = 128;

/// The longest key name, in characters, such as `C-M-S-PageDown`.
pub const MAX_KEY_NAME: usize = 32;

/// The largest signal number a `signal` may carry.
pub const MAX_SIGNAL: u32 = 127;

/// A `pty_id`'s length: 128 bits as lowercase hex.
pub const ID_LENGTH: usize = 32;

/// The most characters an `error` name has.
pub const MAX_ERROR: usize = 64;

/// Why rlxd ends a PTY's process itself, as a screen's, a list's or `pty.exited`'s
/// `error` names it.
pub mod errors {
    /// The screen model failed on the program's output: rlxd ends that PTY's process,
    /// and no other.
    pub const SCREEN_FAILED: &str = "screen_failed";
}

/// `pty.spawn`'s params: what to run, where, and on how big a terminal.
///
/// ```json
/// {"argv": ["sh", "-c", "echo ready; sleep 30"], "cols": 80, "rows": 24,
///  "cwd": "/Users/sam/code", "env": {"FOO": "1", "BAR": null}, "label": "coder"}
/// ```
///
/// The process gets rlxd's environment, as the fleet's tmux server gives a window its
/// own, with what ties a process to one session, board or agent taken out: the tracing
/// identity and run marker (`ANTHROPIC_BASE_URL`, `ANTHROPIC_CUSTOM_HEADERS`,
/// `AISQUARE_PIPELINE_ID`, `AISQUARE_TRACE_AGENT_NAME`, `AISQUARE_RUN_TRACE_ID`,
/// `AISQUARE_LAUNCH_ID`, `AISQUARE_LAUNCH_AGENT`, `AISQUARE_FLEET_AGENT`, and every
/// `AISQUARE_BROKERED*`); the board's (`AISQUARE_TEAM_HUB`, `AISQUARE_ROLE`,
/// `AISQUARE_PERSONA`, `AISQUARE_BOARD_CONTEXT`); each agent's process identity
/// (`CLAUDE_PID`, `CLAUDECODE`, and Codex's `CODEX_THREAD_ID`, `CODEX_SESSION_ID`,
/// `CODEX_TOOL_CALL_ID`, `CODEX_CI`, `CODEX_SANDBOX_NETWORK_DISABLED`,
/// `CODEX_PERMISSION_PROFILE`, `CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS`,
/// `CODEX_VERSION`, `CODEX_MANAGED_BY_NPM`, `CODEX_MANAGED_PACKAGE_ROOT`);
/// `CODEX_HOME` when rlxd runs in a Codex tool shell; and tmux's own (`TMUX`,
/// `TMUX_PANE`, `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`). These are the fleet's
/// (`core/spawn.py`'s `tmux_client_env`) plus tmux's. Then `TERM=xterm-256color` and
/// `COLORTERM=truecolor`, then `env` over it all, which can put any of them back.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtySpawnParams")]
#[non_exhaustive]
pub struct PtySpawnParams {
    /// The program and its arguments. The program is looked up on `PATH` when it has no
    /// path of its own. No item holds a NUL.
    #[schemars(length(min = 1), transform = argv_items)]
    pub argv: Vec<String>,
    /// The directory it starts in: an absolute path. Absent is rlxd's home directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = no_nul_schema, length(min = 1))]
    pub cwd: Option<String>,
    /// Variables to set, over rlxd's environment and the defaults above, or to remove,
    /// with `null`. A name is not empty and holds no `=` or NUL; a value holds no NUL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = env_schema)]
    pub env: Option<BTreeMap<String, Option<String>>>,
    /// The terminal's width, in cells.
    #[schemars(range(min = 1, max = 1000))]
    pub cols: u16,
    /// The terminal's height, in cells.
    #[schemars(range(min = 1, max = 1000))]
    pub rows: u16,
    /// A name for people, shown by `pty.list`, such as an agent's: 1 to 128 characters,
    /// none of them a control character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = label_schema)]
    pub label: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtySpawnParams")]
struct UncheckedSpawn {
    argv: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    cwd: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    env: Option<BTreeMap<String, Option<String>>>,
    cols: u16,
    rows: u16,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    label: Option<String>,
}

/// One PTY, named: `pty.spawn`'s reply, and `pty.write`'s, `pty.resize`'s and
/// `pty.kill`'s.
///
/// ```json
/// {"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtyRef")]
#[non_exhaustive]
pub struct PtyRef {
    /// The PTY: 128 random bits, as 32 lowercase hex digits.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtyRef")]
struct UncheckedRef {
    pty_id: String,
}

/// `pty.write`'s params: one of `text`, `keys` or `paste`.
///
/// ```json
/// {"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b", "keys": ["C-c", "Enter"]}
/// ```
///
/// - `text` goes to the process as its UTF-8, as `send-keys -l` types it.
/// - `keys` are tmux's key names, such as `Enter`, `S-Enter`, `Escape`, `BTab`, `Up`,
///   `C-c`, `M-x`, `F5` or `PageDown`, or single characters. Each goes as the bytes tmux's
///   `send-keys` sends for it, honouring the modes the program set, such as application
///   cursor keys. A name tmux doesn't know is typed as its characters, as `send-keys`
///   types it.
/// - `paste` goes as `paste-buffer -p` sends it: each newline as a carriage return, and
///   bracketed (`ESC [200~` … `ESC [201~`) when the program turned bracketed paste on.
///
/// At most 128 KiB of text, of paste, or of key names.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtyWriteParams", transform = one_input)]
#[non_exhaustive]
pub struct PtyWriteParams {
    /// The PTY.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
    /// Text to type, as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub text: Option<String>,
    /// Keys to press, in order: tmux's names, each 1 to 32 characters, with no space or
    /// control character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = key_names, length(min = 1))]
    pub keys: Option<Vec<String>>,
    /// Text to paste, as one bracketed paste.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub paste: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtyWriteParams")]
struct UncheckedWrite {
    pty_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    text: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    keys: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    paste: Option<String>,
}

/// What one `pty.write` sends: [`PtyWriteParams`]' one input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PtyInput<'a> {
    /// Text to type, as it is.
    Text(&'a str),
    /// tmux key names to press.
    Keys(&'a [String]),
    /// Text to paste.
    Paste(&'a str),
}

/// `pty.resize`'s params: the terminal's new size.
///
/// ```json
/// {"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b", "cols": 120, "rows": 40}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtyResizeParams")]
#[non_exhaustive]
pub struct PtyResizeParams {
    /// The PTY.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
    /// The new width, in cells.
    #[schemars(range(min = 1, max = 1000))]
    pub cols: u16,
    /// The new height, in cells.
    #[schemars(range(min = 1, max = 1000))]
    pub rows: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtyResizeParams")]
struct UncheckedResize {
    pty_id: String,
    cols: u16,
    rows: u16,
}

/// `pty.screen`'s params, and `pty.kill`'s and `pty.subscribe`'s are alike.
///
/// ```json
/// {"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b", "scrollback": 100}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtyScreenParams")]
#[non_exhaustive]
pub struct PtyScreenParams {
    /// The PTY.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
    /// Lines of history to put above the screen, at most 50,000, as `capture-pane -S -n`
    /// does: fewer when the PTY holds fewer. Absent is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(max = 50_000))]
    pub scrollback: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtyScreenParams")]
struct UncheckedScreenParams {
    pty_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    scrollback: Option<u32>,
}

/// `pty.kill`'s params.
///
/// ```json
/// {"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtyKillParams")]
#[non_exhaustive]
pub struct PtyKillParams {
    /// The PTY. Its process and every process in its group (on Windows, its job) end, and
    /// the PTY is gone.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtyKillParams")]
struct UncheckedKill {
    pty_id: String,
}

/// `pty.subscribe`'s params: which PTY to stream, and whether this connection may still
/// write to it.
///
/// ```json
/// {"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b", "read_only": true}
/// ```
///
/// The stream that follows carries, in order:
///
/// 1. `pty.snapshot`, the screen as [`PtyScreen`], whose `seq` the stream goes on from;
/// 2. the PTY's `pty.output` events, then its `pty.exited` when its process ends.
///
/// A client that reads too slowly to keep up never holds the PTY back: the output it
/// missed is dropped, and it gets a new `pty.snapshot` instead. The reply, `{seq}`, comes
/// last, once the PTY is killed or rlxd stops.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtySubscribeParams")]
#[non_exhaustive]
pub struct PtySubscribeParams {
    /// The PTY.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
    /// Watch only: this connection's `pty.write` to the PTY gets `read_only`. An `app`
    /// or `remote` connection must say `true`.
    pub read_only: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtySubscribeParams")]
struct UncheckedSubscribe {
    pty_id: String,
    read_only: bool,
}

/// Where the cursor is, from the screen's top left, and whether the program shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct PtyCursor {
    /// The column, from 0. It is `cols` while a line's last cell is written and the next
    /// character would wrap, and can be past `cols` after a resize that didn't rewrap the
    /// screen (the alternate screen), until the next character, as in tmux.
    pub x: u16,
    /// The row, from 0.
    pub y: u16,
    /// Whether the program shows the cursor.
    pub visible: bool,
}

impl PtyCursor {
    /// A cursor at column `x` and row `y`.
    pub fn new(x: u16, y: u16, visible: bool) -> Self {
        Self { x, y, visible }
    }
}

/// `pty.screen`'s reply, and a `pty.snapshot`'s data: the screen, as tmux would print it.
///
/// ```json
/// {"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b", "text": "\u001b[31mred\u001b[39m plain\n",
///  "cursor": {"x": 9, "y": 0, "visible": true}, "cols": 80, "rows": 24,
///  "alternate": false, "history": 0, "exited": false, "seq": 41}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtyScreen", transform = exit_rules)]
#[non_exhaustive]
pub struct PtyScreen {
    /// The PTY.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
    /// The screen, byte for byte what `tmux capture-pane -e -p` prints for it (with
    /// `-S -n` for `scrollback`): each line, its colours and attributes as SGR sequences,
    /// trailing spaces dropped, and a newline after every line, the blank ones too.
    pub text: String,
    /// The cursor.
    pub cursor: PtyCursor,
    /// The width, in cells.
    #[schemars(range(min = 1, max = 1000))]
    pub cols: u16,
    /// The height, in cells.
    #[schemars(range(min = 1, max = 1000))]
    pub rows: u16,
    /// Whether the program is on the alternate screen, as a full-screen program is.
    pub alternate: bool,
    /// The lines of history the PTY holds above the screen.
    #[schemars(range(max = 50_000))]
    pub history: u32,
    /// Whether the process has ended.
    pub exited: bool,
    /// Its exit status, once it has ended: its exit code, or 128 plus the signal that
    /// ended it, as a shell reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null)]
    pub status: Option<u32>,
    /// The signal that ended it, on macOS and Linux, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(min = 1, max = 127))]
    pub signal: Option<u32>,
    /// Why rlxd is ending, or has ended, the process itself, when it is: `screen_failed`
    /// when the screen model failed on the program's output. That PTY alone ends, and its
    /// screen is blank from then on. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, regex(pattern = "^[a-z_]{1,64}$"))]
    pub error: Option<String>,
    /// The last event `seq` this screen takes in: a stream goes on from the event after
    /// it.
    pub seq: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtyScreen")]
struct UncheckedScreen {
    pty_id: String,
    text: String,
    cursor: PtyCursor,
    cols: u16,
    rows: u16,
    alternate: bool,
    history: u32,
    exited: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    status: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    signal: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    error: Option<String>,
    seq: u64,
}

/// One PTY, as `pty.list` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(transform = exit_rules)]
#[non_exhaustive]
pub struct PtyInfo {
    /// The PTY.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
    /// Its label, when `pty.spawn` gave one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = label_schema)]
    pub label: Option<String>,
    /// What it runs, as `pty.spawn` gave it.
    #[schemars(length(min = 1))]
    pub argv: Vec<String>,
    /// The directory it started in.
    pub cwd: String,
    /// Its process's id.
    #[schemars(range(min = 1, max = 2_147_483_647))]
    pub pid: u32,
    /// The width, in cells.
    #[schemars(range(min = 1, max = 1000))]
    pub cols: u16,
    /// The height, in cells.
    #[schemars(range(min = 1, max = 1000))]
    pub rows: u16,
    /// When it started, in RFC 3339 UTC.
    #[schemars(regex(pattern = r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$"))]
    pub started_at: String,
    /// Whether the process has ended.
    pub exited: bool,
    /// Its exit status, once it has ended, as [`PtyScreen`]'s.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null)]
    pub status: Option<u32>,
    /// The signal that ended it, when one did.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null, range(min = 1, max = 127))]
    pub signal: Option<u32>,
    /// Why rlxd is ending, or has ended, the process itself, as [`PtyScreen`]'s says.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    #[schemars(transform = drop_null, regex(pattern = "^[a-z_]{1,64}$"))]
    pub error: Option<String>,
}

/// `pty.list`'s reply: every PTY of the home, whichever connection spawned it, in the
/// order they started.
///
/// ```json
/// {"ptys": [{"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b", "label": "coder",
///   "argv": ["sh"], "cwd": "/Users/sam", "pid": 4242, "cols": 80, "rows": 24,
///   "started_at": "2026-11-26T12:00:00.000Z", "exited": false}]}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtyList")]
#[non_exhaustive]
pub struct PtyList {
    /// The PTYs.
    pub ptys: Vec<PtyInfo>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtyList")]
struct UncheckedPtyList {
    ptys: Vec<PtyInfo>,
}

/// `pty.output`'s data, on the `pty` topic: bytes the process wrote, as they came.
///
/// ```json
/// {"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b", "bytes_b64": "cmVhZHkNCg=="}
/// ```
///
/// rlxd keeps less of this output than of other events, apart from them, so a busy PTY
/// never pushes another topic's events out. A stream that asked for `pty` gets a
/// `daemon.gap` when output it had not read was dropped.
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtyOutput")]
#[non_exhaustive]
pub struct PtyOutput {
    /// The PTY.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
    /// The bytes, in standard base64 with padding: at least one byte.
    #[schemars(
        length(min = 4),
        regex(pattern = "^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$")
    )]
    pub bytes_b64: String,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtyOutput")]
struct UncheckedOutput {
    pty_id: String,
    bytes_b64: String,
}

/// `pty.exited`'s data, on the `pty` topic: the process ended. The PTY keeps its last
/// screen until `pty.kill`.
///
/// ```json
/// {"pty_id": "6f1c2e0a9b8d4f3e2a1b0c9d8e7f6a5b", "status": 3}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, JsonSchema)]
#[schemars(title = "PtyExited")]
#[non_exhaustive]
pub struct PtyExited {
    /// The PTY.
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub pty_id: String,
    /// Its exit status, as [`PtyScreen`]'s.
    pub status: u32,
    /// The signal that ended it, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, range(min = 1, max = 127))]
    pub signal: Option<u32>,
    /// Why rlxd ended the process itself, when it did, as [`PtyScreen`]'s says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(transform = drop_null, regex(pattern = "^[a-z_]{1,64}$"))]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PtyExited")]
struct UncheckedExited {
    pty_id: String,
    status: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    signal: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    error: Option<String>,
}

/// Whether `id` is a `pty_id`'s shape: 32 lowercase hex digits.
pub fn check_id(field: &str, id: &str) -> Result<(), Invalid> {
    if id.len() == ID_LENGTH && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Ok(());
    }
    Err(Invalid::new(field, "is not 32 lowercase hex digits"))
}

fn check_size(cols: u16, rows: u16) -> Result<(), Invalid> {
    for (field, value) in [("cols", cols), ("rows", rows)] {
        check::at_least(field, u64::from(value), 1)?;
        check::at_most(field, u64::from(value), u64::from(MAX_SIZE))?;
    }
    Ok(())
}

fn no_nul(field: &str, value: &str) -> Result<(), Invalid> {
    if value.contains('\0') {
        return Err(Invalid::new(field, "holds a NUL"));
    }
    Ok(())
}

fn check_label(field: &str, label: &str) -> Result<(), Invalid> {
    let count = label.chars().count();
    if count == 0 || count > MAX_LABEL {
        return Err(Invalid::new(field, "is not 1 to 128 characters"));
    }
    if label.chars().any(char::is_control) {
        return Err(Invalid::new(field, "holds a control character"));
    }
    Ok(())
}

/// An `error` is a name: 1 to 64 lowercase letters and underscores.
fn check_error(field: &str, error: Option<&str>) -> Result<(), Invalid> {
    match error {
        Some(error)
            if error.is_empty()
                || error.len() > MAX_ERROR
                || !error.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            Err(Invalid::new(
                field,
                "is not 1 to 64 lowercase letters and underscores",
            ))
        }
        _ => Ok(()),
    }
}

fn check_signal(field: &str, signal: Option<u32>) -> Result<(), Invalid> {
    if let Some(signal) = signal {
        check::at_least(field, u64::from(signal), 1)?;
        check::at_most(field, u64::from(signal), u64::from(MAX_SIGNAL))?;
    }
    Ok(())
}

/// Whether `key` is a key name's shape: 1 to 32 characters, with no whitespace or control
/// character. Whether tmux knows it is rlxd's to say.
pub fn check_key_name(field: &str, key: &str) -> Result<(), Invalid> {
    let count = key.chars().count();
    if count == 0 || count > MAX_KEY_NAME {
        return Err(Invalid::new(
            field,
            "is not a key name of 1 to 32 characters",
        ));
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Invalid::new(
            field,
            "holds a space or a control character, which no key name does",
        ));
    }
    Ok(())
}

fn check_base64(field: &str, text: &str) -> Result<(), Invalid> {
    let bytes = text.as_bytes();
    let well_formed = !bytes.is_empty()
        && bytes.len().is_multiple_of(4)
        && bytes.iter().enumerate().all(|(i, &b)| {
            b.is_ascii_alphanumeric()
                || b == b'+'
                || b == b'/'
                || (b == b'='
                    && i >= bytes.len() - 2
                    && (i == bytes.len() - 1 || bytes[i + 1] == b'='))
        });
    if well_formed {
        return Ok(());
    }
    Err(Invalid::new(
        field,
        "is not standard base64 of at least one byte",
    ))
}

impl PtySpawnParams {
    /// `pty.spawn` of `argv` on a terminal of `cols` by `rows`.
    pub fn new(argv: Vec<String>, cols: u16, rows: u16) -> Result<Self, Invalid> {
        let params = Self {
            argv,
            cwd: None,
            env: None,
            cols,
            rows,
            label: None,
        };
        params.validate()?;
        Ok(params)
    }

    /// These params, starting in `cwd`.
    #[must_use]
    pub fn in_dir(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// These params, setting (or, with `None`, removing) `name`.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: Option<String>) -> Self {
        self.env
            .get_or_insert_with(BTreeMap::new)
            .insert(name.into(), value);
        self
    }

    /// These params, labelled `label`.
    #[must_use]
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Whether these params keep every rule of `pty-spawn.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.argv.is_empty() {
            return Err(Invalid::new("argv", "is empty"));
        }
        check::non_empty("argv[0]", &self.argv[0])?;
        for (i, item) in self.argv.iter().enumerate() {
            no_nul(&format!("argv[{i}]"), item)?;
        }
        if let Some(cwd) = &self.cwd {
            check::non_empty("cwd", cwd)?;
            no_nul("cwd", cwd)?;
        }
        if let Some(env) = &self.env {
            for (name, value) in env {
                let field = format!("env.{name}");
                if name.is_empty() || name.contains('=') || name.contains('\0') {
                    return Err(Invalid::new(
                        "env",
                        format!("{name:?} is not a variable's name"),
                    ));
                }
                if let Some(value) = value {
                    no_nul(&field, value)?;
                }
            }
        }
        check_size(self.cols, self.rows)?;
        if let Some(label) = &self.label {
            check_label("label", label)?;
        }
        Ok(())
    }
}

impl PtyRef {
    /// The PTY `pty_id`.
    pub fn new(pty_id: impl Into<String>) -> Result<Self, Invalid> {
        let reply = Self {
            pty_id: pty_id.into(),
        };
        reply.validate()?;
        Ok(reply)
    }

    /// Whether this keeps every rule of `pty-ref.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)
    }
}

impl PtyWriteParams {
    /// `pty.write` of `text`, typed as it is.
    pub fn text(pty_id: impl Into<String>, text: impl Into<String>) -> Result<Self, Invalid> {
        Self::of(pty_id.into(), Some(text.into()), None, None)
    }

    /// `pty.write` of `keys`, tmux's key names, pressed in order.
    pub fn keys(pty_id: impl Into<String>, keys: Vec<String>) -> Result<Self, Invalid> {
        Self::of(pty_id.into(), None, Some(keys), None)
    }

    /// `pty.write` of `text`, as one bracketed paste.
    pub fn paste(pty_id: impl Into<String>, text: impl Into<String>) -> Result<Self, Invalid> {
        Self::of(pty_id.into(), None, None, Some(text.into()))
    }

    fn of(
        pty_id: String,
        text: Option<String>,
        keys: Option<Vec<String>>,
        paste: Option<String>,
    ) -> Result<Self, Invalid> {
        let params = Self {
            pty_id,
            text,
            keys,
            paste,
        };
        params.validate()?;
        Ok(params)
    }

    /// What these params send.
    pub fn input(&self) -> PtyInput<'_> {
        match (&self.text, &self.keys, &self.paste) {
            (Some(text), _, _) => PtyInput::Text(text),
            (_, Some(keys), _) => PtyInput::Keys(keys),
            (_, _, Some(paste)) => PtyInput::Paste(paste),
            // validate() holds one of the three, and every constructor runs it.
            (None, None, None) => PtyInput::Text(""),
        }
    }

    /// Whether these params keep every rule of `pty-write.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)?;
        let given = [
            self.text.is_some(),
            self.keys.is_some(),
            self.paste.is_some(),
        ];
        if given.iter().filter(|&&g| g).count() != 1 {
            return Err(Invalid::new(
                "text",
                "exactly one of text, keys and paste is given",
            ));
        }
        let too_long = |field: &str, len: usize| {
            if len > MAX_WRITE {
                return Err(Invalid::new(
                    field,
                    format!("is {len} bytes, more than {MAX_WRITE}"),
                ));
            }
            Ok(())
        };
        if let Some(text) = &self.text {
            too_long("text", text.len())?;
        }
        if let Some(paste) = &self.paste {
            too_long("paste", paste.len())?;
        }
        if let Some(keys) = &self.keys {
            if keys.is_empty() {
                return Err(Invalid::new("keys", "is empty"));
            }
            for (i, key) in keys.iter().enumerate() {
                check_key_name(&format!("keys[{i}]"), key)?;
            }
            too_long("keys", keys.iter().map(String::len).sum())?;
        }
        Ok(())
    }
}

impl PtyResizeParams {
    /// `pty.resize` of `pty_id` to `cols` by `rows`.
    pub fn new(pty_id: impl Into<String>, cols: u16, rows: u16) -> Result<Self, Invalid> {
        let params = Self {
            pty_id: pty_id.into(),
            cols,
            rows,
        };
        params.validate()?;
        Ok(params)
    }

    /// Whether these params keep every rule of `pty-resize.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)?;
        check_size(self.cols, self.rows)
    }
}

impl PtyScreenParams {
    /// `pty.screen` of `pty_id`, the screen alone.
    pub fn new(pty_id: impl Into<String>) -> Result<Self, Invalid> {
        let params = Self {
            pty_id: pty_id.into(),
            scrollback: None,
        };
        params.validate()?;
        Ok(params)
    }

    /// These params, with `lines` of history above the screen.
    #[must_use]
    pub fn with_scrollback(mut self, lines: u32) -> Self {
        self.scrollback = Some(lines);
        self
    }

    /// Whether these params keep every rule of `pty-screen.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)?;
        if let Some(lines) = self.scrollback {
            check::at_most("scrollback", u64::from(lines), u64::from(MAX_SCROLLBACK))?;
        }
        Ok(())
    }
}

impl PtyKillParams {
    /// `pty.kill` of `pty_id`.
    pub fn new(pty_id: impl Into<String>) -> Result<Self, Invalid> {
        let params = Self {
            pty_id: pty_id.into(),
        };
        params.validate()?;
        Ok(params)
    }

    /// Whether these params keep every rule of `pty-kill.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)
    }
}

impl PtySubscribeParams {
    /// `pty.subscribe` to `pty_id`, watching only when `read_only`.
    pub fn new(pty_id: impl Into<String>, read_only: bool) -> Result<Self, Invalid> {
        let params = Self {
            pty_id: pty_id.into(),
            read_only,
        };
        params.validate()?;
        Ok(params)
    }

    /// Whether these params keep every rule of `pty-subscribe.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)
    }
}

impl PtyScreen {
    /// The screen of `pty_id`, `cols` by `rows`, printed as `text`, taking in the events
    /// up to `seq`. Set the rest with the `with_` methods.
    pub fn new(
        pty_id: impl Into<String>,
        text: impl Into<String>,
        cursor: PtyCursor,
        cols: u16,
        rows: u16,
        seq: u64,
    ) -> Result<Self, Invalid> {
        let screen = Self {
            pty_id: pty_id.into(),
            text: text.into(),
            cursor,
            cols,
            rows,
            alternate: false,
            history: 0,
            exited: false,
            status: None,
            signal: None,
            error: None,
            seq,
        };
        screen.validate()?;
        Ok(screen)
    }

    /// This screen, whose process rlxd is ending or has ended itself, for `error`.
    pub fn with_error(mut self, error: Option<String>) -> Result<Self, Invalid> {
        self.error = error;
        self.validate()?;
        Ok(self)
    }

    /// This screen, on the alternate screen when `alternate`, with `history` lines above.
    pub fn with_state(mut self, alternate: bool, history: u32) -> Result<Self, Invalid> {
        self.alternate = alternate;
        self.history = history;
        self.validate()?;
        Ok(self)
    }

    /// This screen, of a process that ended with `status`, by `signal` when one ended it.
    pub fn with_exit(mut self, status: u32, signal: Option<u32>) -> Result<Self, Invalid> {
        self.exited = true;
        self.status = Some(status);
        self.signal = signal;
        self.validate()?;
        Ok(self)
    }

    /// This screen as an event's `data`, for `pty.snapshot`.
    pub fn to_data(&self) -> Result<Data, Invalid> {
        self.validate()?;
        object(serde_json::to_value(self))
    }

    /// Whether this screen keeps every rule of `pty-screen-reply.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)?;
        check_size(self.cols, self.rows)?;
        check::at_most(
            "history",
            u64::from(self.history),
            u64::from(MAX_SCROLLBACK),
        )?;
        exit_fields(self.exited, self.status, self.signal)?;
        check_error("error", self.error.as_deref())
    }
}

/// `exited` with a `status`, or neither, and a `signal` only with a `status`.
fn exit_fields(exited: bool, status: Option<u32>, signal: Option<u32>) -> Result<(), Invalid> {
    if exited != status.is_some() {
        return Err(Invalid::new(
            "status",
            "is given exactly when the process has exited",
        ));
    }
    if signal.is_some() && status.is_none() {
        return Err(Invalid::new("signal", "is given only with a status"));
    }
    check_signal("signal", signal)
}

impl PtyInfo {
    /// The PTY `pty_id`, running `argv` in `cwd` as `pid` since `started_at`, `cols` by
    /// `rows`. Set the rest with the `with_` methods.
    pub fn new(
        pty_id: impl Into<String>,
        argv: Vec<String>,
        cwd: impl Into<String>,
        pid: u32,
        cols: u16,
        rows: u16,
        started_at: impl Into<String>,
    ) -> Result<Self, Invalid> {
        let info = Self {
            pty_id: pty_id.into(),
            label: None,
            argv,
            cwd: cwd.into(),
            pid,
            cols,
            rows,
            started_at: started_at.into(),
            exited: false,
            status: None,
            signal: None,
            error: None,
        };
        info.validate()?;
        Ok(info)
    }

    /// This PTY, whose process rlxd is ending or has ended itself, for `error`.
    pub fn with_error(mut self, error: Option<String>) -> Result<Self, Invalid> {
        self.error = error;
        self.validate()?;
        Ok(self)
    }

    /// This PTY, labelled `label`.
    pub fn with_label(mut self, label: Option<String>) -> Result<Self, Invalid> {
        self.label = label;
        self.validate()?;
        Ok(self)
    }

    /// This PTY, whose process ended with `status`, by `signal` when one ended it.
    pub fn with_exit(mut self, status: u32, signal: Option<u32>) -> Result<Self, Invalid> {
        self.exited = true;
        self.status = Some(status);
        self.signal = signal;
        self.validate()?;
        Ok(self)
    }

    /// Whether this PTY keeps every rule of its part of `pty-list-reply.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)?;
        if let Some(label) = &self.label {
            check_label("label", label)?;
        }
        if self.argv.is_empty() {
            return Err(Invalid::new("argv", "is empty"));
        }
        check::pid("pid", self.pid)?;
        check_size(self.cols, self.rows)?;
        check::date_time("started_at", &self.started_at)?;
        exit_fields(self.exited, self.status, self.signal)?;
        check_error("error", self.error.as_deref())
    }
}

impl PtyList {
    /// A list of `ptys`.
    pub fn new(ptys: Vec<PtyInfo>) -> Result<Self, Invalid> {
        let list = Self { ptys };
        list.validate()?;
        Ok(list)
    }

    /// Whether this list keeps every rule of `pty-list-reply.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        for (i, pty) in self.ptys.iter().enumerate() {
            pty.validate()
                .map_err(|e| e.within(&format!("ptys[{i}]")))?;
        }
        Ok(())
    }
}

impl PtyOutput {
    /// Output of `pty_id`, already in base64.
    pub fn new(pty_id: impl Into<String>, bytes_b64: impl Into<String>) -> Result<Self, Invalid> {
        let output = Self {
            pty_id: pty_id.into(),
            bytes_b64: bytes_b64.into(),
        };
        output.validate()?;
        Ok(output)
    }

    /// This output as an event's `data`.
    pub fn to_data(&self) -> Result<Data, Invalid> {
        self.validate()?;
        object(serde_json::to_value(self))
    }

    /// Whether this output keeps every rule of `pty-output.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)?;
        check_base64("bytes_b64", &self.bytes_b64)
    }
}

impl PtyExited {
    /// `pty_id`'s process ended with `status`, by `signal` when one ended it.
    pub fn new(
        pty_id: impl Into<String>,
        status: u32,
        signal: Option<u32>,
    ) -> Result<Self, Invalid> {
        let exited = Self {
            pty_id: pty_id.into(),
            status,
            signal,
            error: None,
        };
        exited.validate()?;
        Ok(exited)
    }

    /// This end, whose process rlxd ended itself, for `error`.
    pub fn with_error(mut self, error: Option<String>) -> Result<Self, Invalid> {
        self.error = error;
        self.validate()?;
        Ok(self)
    }

    /// This end as an event's `data`.
    pub fn to_data(&self) -> Result<Data, Invalid> {
        self.validate()?;
        object(serde_json::to_value(self))
    }

    /// Whether this keeps every rule of `pty-exited.json`.
    pub fn validate(&self) -> Result<(), Invalid> {
        check_id("pty_id", &self.pty_id)?;
        check_signal("signal", self.signal)?;
        check_error("error", self.error.as_deref())
    }
}

fn object(value: Result<Value, serde_json::Error>) -> Result<Data, Invalid> {
    match value {
        Ok(Value::Object(data)) => Ok(data),
        _ => Err(Invalid::new("data", "did not print as an object")),
    }
}

/// Each item a string with no NUL, and the program's not empty.
fn argv_items(schema: &mut Schema) {
    schema.insert(
        "items".to_owned(),
        json!({"type": "string", "not": {"pattern": "\\x00"}}),
    );
    schema.insert("prefixItems".to_owned(), json!([{"minLength": 1}]));
}

/// Names with no `=` or NUL, and each value a string with no NUL, or `null`.
fn env_schema(schema: &mut Schema) {
    drop_null(schema);
    schema.insert(
        "propertyNames".to_owned(),
        json!({"minLength": 1, "not": {"pattern": "[=\\x00]"}}),
    );
    schema.insert(
        "additionalProperties".to_owned(),
        json!({"anyOf": [{"type": "string", "not": {"pattern": "\\x00"}}, {"type": "null"}]}),
    );
}

/// Each key name 1 to 32 characters, with no whitespace or control character. The pattern
/// spells the controls out, as every validator's regex reads that.
fn key_names(schema: &mut Schema) {
    drop_null(schema);
    schema.insert(
        "items".to_owned(),
        json!({"type": "string", "minLength": 1, "maxLength": 32,
               "not": {"pattern": "[\\s\\x00-\\x1f\\x7f-\\x9f]"}}),
    );
}

/// Up to 128 characters, none of them a control character.
fn label_schema(schema: &mut Schema) {
    drop_null(schema);
    schema.insert("minLength".to_owned(), 1.into());
    schema.insert("maxLength".to_owned(), 128.into());
    schema.insert(
        "not".to_owned(),
        json!({"pattern": "[\\x00-\\x1f\\x7f-\\x9f]"}),
    );
}

/// A string with no NUL.
fn no_nul_schema(schema: &mut Schema) {
    drop_null(schema);
    schema.insert("not".to_owned(), json!({"pattern": "\\x00"}));
}

/// `status` exactly when `exited`, and `signal` only with it.
fn exit_rules(schema: &mut Schema) {
    schema.insert(
        "allOf".to_owned(),
        json!([
            {
                "if": {"properties": {"exited": {"const": true}}, "required": ["exited"]},
                "then": {"required": ["status"]},
                "else": {"not": {"anyOf": [{"required": ["status"]}, {"required": ["signal"]}]}},
            },
        ]),
    );
}

/// Exactly one of `text`, `keys` and `paste`.
fn one_input(schema: &mut Schema) {
    schema.insert(
        "oneOf".to_owned(),
        json!([
            {"required": ["text"]},
            {"required": ["keys"]},
            {"required": ["paste"]},
        ]),
    );
}

macro_rules! checked_serde {
    ($type:ident, $unchecked:ident) => {
        impl Serialize for $type {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.validate().map_err(serde::ser::Error::custom)?;
                $unchecked::serialize(self, serializer)
            }
        }

        /// Reads an object only, as the schema does.
        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let object = Data::deserialize(deserializer)?;
                let value =
                    $unchecked::deserialize(Value::Object(object)).map_err(de::Error::custom)?;
                value.validate().map_err(de::Error::custom)?;
                Ok(value)
            }
        }
    };
}

checked_serde!(PtySpawnParams, UncheckedSpawn);
checked_serde!(PtyRef, UncheckedRef);
checked_serde!(PtyWriteParams, UncheckedWrite);
checked_serde!(PtyResizeParams, UncheckedResize);
checked_serde!(PtyScreenParams, UncheckedScreenParams);
checked_serde!(PtyKillParams, UncheckedKill);
checked_serde!(PtySubscribeParams, UncheckedSubscribe);
checked_serde!(PtyScreen, UncheckedScreen);
checked_serde!(PtyList, UncheckedPtyList);
checked_serde!(PtyOutput, UncheckedOutput);
checked_serde!(PtyExited, UncheckedExited);
