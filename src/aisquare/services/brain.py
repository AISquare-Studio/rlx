"""The resident Python brain: ``rlx brain --stdio``, which rlxd supervises (handoff 0.10.3,
1.6).

rlxd starts one brain per home and keeps it running: it starts it again, with backoff, when
it dies, and kills its process tree when rlxd stops. What rlxd asks Python, it asks the
brain, so no Python process starts for each question (1.5's hooks first; later the
connector map and the runner's prompts and board calls).

The protocol
------------

JSON lines over the brain's stdin (rlxd's frames) and stdout (the brain's). Every frame is
one JSON object on one line, ASCII only, with a ``type``. The shapes are rlxd-proto's
(``rlxd/schemas/v1/brain-input.json`` and ``brain-output.json``):

1. rlxd says ``{"type": "hello", "protocol": 1, "rlxd_version": "…", "env_allowlist":
   […], "env_prefixes": […], "env_secret_words": […]}`` first.
2. The brain answers ``{"type": "hello", "protocol": 1, "pid": …, "rlx_version": "…"}``,
   or, when rlxd's ``env_allowlist``, ``env_prefixes`` and ``env_secret_words`` are not this
   brain's (:data:`ENV_NAMES`, :data:`ENV_PREFIXES`, :data:`ENV_SECRET_WORDS`: rlxd-hook
   would send other variables than the brain sets), ``{"type": "refused", "code":
   "env_allowlist_differs", …}``, or, for a ``protocol`` it does not speak,
   ``{"type": "refused", "code": "incompatible_protocol", "message": "…", "supported":
   [1]}`` and exits. rlxd refuses
   a brain whose ``protocol`` it does not speak the same way. So an rlx and an rlxd of
   different releases fail loudly, with a named error in rlxd's ``status``, and never
   misread each other's frames.
3. Then requests, one at a time: ``{"type": "request", "id": 7, "method": "ping",
   "params": {}}``. The brain answers each with ``{"type": "accepted", "id": 7}`` as soon
   as it has read it, before it runs anything, then with ``{"type": "reply", "id": 7,
   "ok": true, "result": {…}}`` or ``{"type": "reply", "id": 7, "ok": false, "error":
   {"code": "…", "message": "…"}}``. ``accepted`` is the line a caller's fallback turns
   on: a request the brain never accepted was never run, and may be run elsewhere; one it
   accepted may have been, and must not be run again.

rlxd keeps one request in flight, so the brain runs them strictly one after another. When
its stdin closes (rlxd stopped or died), the brain exits.

The channel
-----------

The brain moves the protocol off fds 0 and 1 before it reads anything: it reads and
writes private duplicates of them, points fd 0 at the null device, and points fd 1 at
fd 2, its log. So nothing the brain runs, and no process it starts, can read a frame
meant for it or write into its replies: a stray print lands in rlxd's log of the brain.
What the process writes to stdout before that (as it imports and starts) reaches rlxd
before the hello: rlxd logs a line that is not JSON and passes over it, and takes any
JSON line that is not a hello of its protocol for a brain of another release.

Hooks (1.5)
-----------

``hook {event, stdin_b64, cwd?, env}`` runs one of Claude Code's six fleet hooks
(:data:`HOOK_EVENTS`) as ``rlx hook <event>`` runs it: ``aisquare.cli.app.main`` itself,
the same entry point, root callback and handler code, so what ``rlx hook`` does (0.10.2's
slimmer session-end hook included) the brain does. It answers ``{stdout_b64, stderr_b64,
exit_code}``: the bytes ``rlx hook`` would have written to its stdout and stderr, and its
exit status. ``pre-tool-use`` is the gate's (5.1), rlxd's own, and never reaches the brain.

A hook runs in Claude Code's process, not rlxd's: its working directory, and its
environment. rlxd-hook sends its working directory and the variables of the allowlist it
has (:func:`forwarded`: :data:`ENV_NAMES`, and the agents' own by :data:`ENV_PREFIXES`). For
one request, the brain becomes that process:

- **Environment.** Each allowlisted variable is set to rlxd-hook's value, or removed when
  rlxd-hook had none, so neither rlxd's value nor an earlier hook's leaks in, and
  ``TZ`` is read again (``time.tzset``). Every other variable is the brain's own, which
  rlxd started it with. What a process reads only as it starts is the brain's: the
  locale's ``LC_*`` set Python's own encodings then, and pass on to the hook's children
  only.
- **Directory.** ``os.chdir`` to rlxd-hook's working directory (the brain's own when that
  one is gone, as it can be by the time the hook runs).
- **Process.** ``sys.argv`` is ``[<this rlx>, "hook", <event>]``. ``sys.stdin`` reads the
  hook's bytes; fds 1 and 2 are pointed at files of their own and ``sys.stdout`` and
  ``sys.stderr`` made over them as Python makes them for a pipe (the encoding and errors
  of the brain's own, ``\r\n`` on Windows), so the bytes are the ones ``rlx hook`` writes,
  a child's output included. fd 0 stays the null device, which a child of ``rlx hook``
  would read as the end of a stdin its parent read to the end.
- **State.** ``core.state``'s runtime state is a fresh one, as the root callback makes it.

All of it is put back afterwards, whatever the hook did. Requests never overlap: rlxd sends
one at a time, so changing process-wide state for one is safe, and nothing of one is left
for the next.

Nothing a hook starts outlives its request but what ``rlx hook``'s would outlive its
process: the brain waits for each non-daemon thread the hook started, as a process does
before it exits (what they print is the hook's), and every child still running after it
is a detached worker (``services.hooks._detach``: a session of its own, which also leaves
``rlx hook`` behind on purpose, such as the code index's refresh). The brain collects each
child that has ended after every hook, so none stays a zombie. Daemon threads (a Remote
push, say) run on: ``rlx hook`` would wait for them at exit (``atexit``), and the brain,
which does not exit, lets them finish. The protocol's own fds are not inheritable, so no
child holds the brain's pipes.

The allowlist, which rlxd-hook sends and the brain applies:

- **rlx's own:** every ``AISQUARE_*`` variable rlx's code names, and ``RLX_HOME``: the role,
  the fleet agent and launch, the team, the broker's scope, the home. A test reads every
  ``AISQUARE_*`` name in ``src/aisquare`` and fails on one that is not here.
- **The agents':** every variable under an agent's own prefix (:data:`ENV_PREFIXES`, read
  off the adapters: the first word of each one's home variable), its config dir above all,
  but a credential's (:data:`ENV_SECRET_WORDS`: its token), which no hook reads. A test
  holds every agent variable rlx's code names to that rule.
- **tmux's:** ``TMUX``, ``TMUX_PANE``, ``TMUX_TMPDIR``: which pane the agent runs in.
- **The process's:** ``PATH`` (which ``tmux``, ``git`` and ``claude`` a hook's children
  run), ``HOME`` and the user's names and folders (``USER``, ``LOGNAME``, ``USERNAME``,
  ``USERPROFILE``, ``APPDATA``, ``LOCALAPPDATA``, ``XDG_*``), the temporary folders, ``TZ``
  and the locale (times a hook prints), the terminal's (``TERM``, ``COLORTERM``,
  ``NO_COLOR``, ``FORCE_COLOR``, ``COLUMNS``, ``LINES``), and the TLS and proxy settings a
  call to the team hub uses.

The parity test (``tests/test_brain_hooks.py``) runs each event both ways, ``rlx hook`` in a
process of its own and the brain, under two different environments (role, config dir,
agent ids) in turn, and compares stdout byte for byte and the store's rows; it also records
every variable the handlers read and fails on one that is neither in the allowlist nor one
that describes the brain's own process (:data:`ENV_OF_THE_BRAIN`, and Python's own,
:data:`ENV_OF_THE_INTERPRETER`).

Caches (the staleness audit)
----------------------------

A brain lives for days; ``rlx hook`` re-reads everything each time. So before each hook the
brain empties every ``functools`` cache of every ``aisquare`` module loaded
(:func:`fresh_caches`): whatever a cache holds is computed again from this hook's
environment and files, as a fresh process would. The module state written with ``global``
on the hook path, each reset or safe as it is (:data:`MODULE_STATE`; a test fails on any
other):

- ``core.config._LAST_PARSE``: keyed by the config file's path and its bytes, which every
  load reads again: never stale.
- ``core.state._state``: the runtime state, which the root callback replaces on every
  command; the brain puts a fresh one back after each hook.
- ``core.program._in_app``: true only while asq's UI runs in this process, never in the
  brain; reset to false.
- ``core.instances._this``: this process's asq instance record, set only by asq's UI;
  reset to none.
- ``core.tmux._boot`` and ``_booted_at``: the machine's boot id and time, the same for
  every process until a reboot, which also restarts rlxd.
- ``services.native_telemetry._config_stamp``: the config file's stamp, read again on
  each use; a change empties the insights cache, which the brain empties anyway.
- ``core.clipboard``'s ``_cache``, ``_worker`` and ``_pending``: asq's clipboard, never on
  the hook path.

Module-level values computed from the environment at import would be stale too: none on the
hook path is (no module of ``aisquare`` reads the environment, the directory or ``argv`` at
import), and the parity test's second environment would show one.
"""

from __future__ import annotations

import base64
import contextlib
import io
import json
import os
import sys
import tempfile
import threading
import time
import traceback
from collections.abc import Callable, Iterator, Mapping
from typing import IO, Any

#: The brain protocols this brain speaks. rlxd's own list is rlxd-proto's
#: ``BRAIN_PROTOCOLS``; they must share one, or rlxd reports ``brain_incompatible``.
PROTOCOLS: tuple[int, ...] = (1,)

#: The longest frame the brain reads: 16 MiB. A hook's payload is at most rlxd's own
#: frame limit (1 MiB), base64 and all.
MAX_FRAME = 16 << 20

Result = dict[str, Any]
Method = Callable[[Mapping[str, Any]], Result]


class BrainError(Exception):
    """A request the brain refused or failed: its ``code`` and ``message`` are the reply's
    ``error``."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class ProtocolError(Exception):
    """A frame from rlxd that is not one this protocol has: the brain says why on its log
    and exits, rather than guess."""


def _ping(params: Mapping[str, Any]) -> Result:
    """``ping``: the brain is here, and answers. ``{"pid": …}``."""
    del params
    return {"pid": os.getpid()}


#: The six fleet hooks of Claude Code's that ``rlx hook`` handles and the brain runs.
#: ``pre-tool-use`` is the gate's, rlxd's own, and is not one.
HOOK_EVENTS: tuple[str, ...] = (
    "session-start",
    "user-prompt-submit",
    "session-end",
    "stop",
    "stop-failure",
    "notification",
)

#: rlx's own variables, which rlx's code names: every one, a test says.
_RLX_NAMES = (
    "AISQUARE_ACCOUNTS_SHARED",
    "AISQUARE_AGENT_BIN",
    "AISQUARE_AGENT_NAME",
    "AISQUARE_API_URL",
    "AISQUARE_BOARD_CONTEXT",
    "AISQUARE_BROKER",
    "AISQUARE_BROKER_NONCE",
    "AISQUARE_BROKERED",
    "AISQUARE_BROKERED_BOARD",
    "AISQUARE_BROKERED_MEMBERS",
    "AISQUARE_BROKERED_POOL",
    "AISQUARE_BROKERED_REPORTS",
    "AISQUARE_BROKERED_ROOT_IDS",
    "AISQUARE_BROKERED_ROOTS",
    "AISQUARE_BROKERED_SEALS",
    "AISQUARE_CI",
    "AISQUARE_CI_DELIVERY_OVERRIDE",
    "AISQUARE_CI_KEY",
    "AISQUARE_CI_RUN",
    "AISQUARE_CI_URL",
    "AISQUARE_CODING_AGENT",
    "AISQUARE_DB_BUSY_MS",
    "AISQUARE_EFFORT",
    "AISQUARE_EXPERIMENTAL_CAPTAIN",
    "AISQUARE_EXPLAINABILITY_TARGET",
    "AISQUARE_FLEET_AGENT",
    "AISQUARE_HARNESS_PROBE",
    "AISQUARE_HOME",
    "AISQUARE_IN_APP",
    "AISQUARE_LAUNCH_AGENT",
    "AISQUARE_LAUNCH_ID",
    "AISQUARE_LAUNCH_PID",
    "AISQUARE_LAUNCH_SETTINGS",
    "AISQUARE_NO_TELEMETRY",
    "AISQUARE_PASTE_LOG",
    "AISQUARE_PERSONA",
    "AISQUARE_PIN_SESSION_ID",
    "AISQUARE_PIPELINE_ID",
    "AISQUARE_PLAIN_CLAUDE_CODE_TMPDIR",
    "AISQUARE_PLAIN_CLAUDE_CONFIG_DIR",
    "AISQUARE_PLAIN_CODEX_HOME",
    "AISQUARE_RELEASES_URL",
    "AISQUARE_REMOTE_AUTO_OFF",
    "AISQUARE_REMOTE_NGROK_URL",
    "AISQUARE_REMOTE_PORT",
    "AISQUARE_REPORTS",
    "AISQUARE_ROLE",
    "AISQUARE_RUN_TRACE_ID",
    "AISQUARE_SERVE_CLIENT",
    "AISQUARE_SERVE_CLOSE_AFTER",
    "AISQUARE_SERVE_PORT",
    "AISQUARE_SERVE_ROLE",
    "AISQUARE_TEAM",
    "AISQUARE_TEAM_DELTA",
    "AISQUARE_TEAM_HUB",
    "AISQUARE_TEAM_LEASE_MIN",
    "AISQUARE_TOKEN",
    "AISQUARE_TRACE_AGENT_NAME",
    "AISQUARE_WATCH_DETACHED",
    "RLX_HOME",
)

#: tmux's, which say which pane the agent runs in.
_TMUX_NAMES = ("TMUX", "TMUX_PANE", "TMUX_TMPDIR")

#: The process's own: its programs, user, folders, time, locale, terminal and network.
_PROCESS_NAMES = (
    "ALL_PROXY",
    "APPDATA",
    "COLORTERM",
    "COLUMNS",
    "FORCE_COLOR",
    "HOME",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_TERMINAL",
    "LINES",
    "LOCALAPPDATA",
    "LOGNAME",
    "NO_COLOR",
    "NO_PROXY",
    "PATH",
    "PATHEXT",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "TZ",
    "USER",
    "USERNAME",
    "USERPROFILE",
    "XDG_BIN_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "all_proxy",
    "http_proxy",
    "https_proxy",
    "no_proxy",
)

#: The allowlist's names: variables rlxd-hook sends with each hook, and the brain sets (or
#: removes) for it. rlxd-proto's ``HOOK_ENV_NAMES`` is the same list, which rlxd names in
#: its hello and the brain checks; see the module's doc.
ENV_NAMES: tuple[str, ...] = tuple(sorted({*_RLX_NAMES, *_TMUX_NAMES, *_PROCESS_NAMES}))
_ENV_SET = frozenset(ENV_NAMES)


def _agent_prefixes() -> tuple[str, ...]:
    from aisquare.core.agent_adapters import adapters

    return tuple(sorted({a.home_env.split("_")[0] for a in adapters() if a.home_env}))


#: The allowlist's prefixes: every coding agent's own variables, named by the first word of
#: its home's variable, as its adapter declares it (Claude Code's ``CLAUDE``, Codex's
#: ``CODEX``): its config dir, its pid, its session's marks, its effort. A new agent's come
#: with its adapter. rlxd-proto's ``HOOK_ENV_PREFIXES`` is the same list.
ENV_PREFIXES: tuple[str, ...] = _agent_prefixes()

#: The words that make a variable under :data:`ENV_PREFIXES` a credential, which is never
#: forwarded: no hook reads an agent's token (rlx's own launch and account checks do, in
#: processes of their own). rlxd-proto's ``HOOK_ENV_SECRET_WORDS`` is the same list.
ENV_SECRET_WORDS: tuple[str, ...] = ("CREDENTIALS", "KEY", "PASSWORD", "SECRET", "TOKEN")


def is_credential(name: str) -> bool:
    """Whether ``name`` is a credential's, by its words (:data:`ENV_SECRET_WORDS`)."""
    return bool(set(name.upper().split("_")) & set(ENV_SECRET_WORDS))


def forwarded(name: str) -> bool:
    """Whether the variable ``name`` crosses from rlxd-hook to the brain: one of
    :data:`ENV_NAMES`, or an agent's own (:data:`ENV_PREFIXES`) that is not a credential."""
    if name in _ENV_SET:
        return True
    return name.upper().startswith(ENV_PREFIXES) and not is_credential(name)


#: Variables the hook path reads that describe the brain's own process, not the hook's:
#: the shell-completion switches of click and typer (a hook never completes), macOS's
#: per-user text encoding, and the interpreter's own (:data:`ENV_OF_THE_INTERPRETER`).
ENV_OF_THE_BRAIN: frozenset[str] = frozenset(
    {
        "_AISQUARE_COMPLETE",
        "_RLX_COMPLETE",
        "_TYPER_COMPLETE_TEST_DISABLE_SHELL_DETECTION",
        "__CF_USER_TEXT_ENCODING",
        "ARCHFLAGS",
    }
)

#: The prefixes of the variables Python reads about itself (``sysconfig``, ``site``,
#: ``zoneinfo``, ``subprocess``): the brain's interpreter is the one ``rlx hook``'s would be.
ENV_OF_THE_INTERPRETER: tuple[str, ...] = ("PYTHON", "_PYTHON_")

#: The module state written with ``global`` on the hook path, and what each is put back to
#: before a hook (``...`` for one that is safe as it is): see the module's doc.
MODULE_STATE: dict[tuple[str, str], object] = {
    ("aisquare.core.config", "_LAST_PARSE"): ...,
    ("aisquare.core.state", "_state"): ...,
    ("aisquare.core.program", "_in_app"): False,
    ("aisquare.core.instances", "_this"): None,
    ("aisquare.core.tmux", "_boot"): ...,
    ("aisquare.core.tmux", "_booted_at"): ...,
    ("aisquare.services.native_telemetry", "_config_stamp"): ...,
    ("aisquare.core.clipboard", "_cache"): ...,
    ("aisquare.core.clipboard", "_worker"): ...,
    ("aisquare.core.clipboard", "_pending"): ...,
}


def fresh_caches() -> int:
    """Empties every ``functools`` cache of every ``aisquare`` module loaded, and puts the
    module state of :data:`MODULE_STATE` back: what a fresh process starts with. How many
    caches it emptied."""
    emptied = 0
    for name, module in list(sys.modules.items()):
        if module is None or not (name == "aisquare" or name.startswith("aisquare.")):
            continue
        for value in list(vars(module).values()):
            clear = getattr(value, "cache_clear", None)
            if callable(clear) and getattr(value, "__module__", None) == name:
                clear()
                emptied += 1
    for (name, attribute), value in MODULE_STATE.items():
        holder = sys.modules.get(name)
        if holder is not None and value is not ...:
            setattr(holder, attribute, value)
    return emptied


#: How Python makes ``sys.stdout`` and ``sys.stderr`` over a pipe, read from the brain's
#: own before anything replaces them: ``(encoding, errors)`` of each.
_STREAM_FORM: dict[str, tuple[str, str]] = {}

#: Python translates ``\n`` to ``\r\n`` on a standard stream on Windows only.
_NEWLINE = None if sys.platform == "win32" else "\n"


def _remember_stream_form() -> None:
    for name in ("stdout", "stderr", "stdin"):
        stream = getattr(sys, name)
        encoding = getattr(stream, "encoding", None) or "utf-8"
        errors = getattr(stream, "errors", None) or "strict"
        _STREAM_FORM.setdefault(name, (encoding, errors))


def _writer(fd: int, name: str, *, line_buffering: bool) -> io.TextIOWrapper:
    """A text stream over ``fd`` as Python makes ``sys.<name>`` over a pipe."""
    encoding, errors = _STREAM_FORM.get(name, ("utf-8", "strict"))
    buffered = io.BufferedWriter(io.FileIO(fd, "w", closefd=False))
    return io.TextIOWrapper(
        buffered,
        encoding=encoding,
        errors=errors,
        newline=_NEWLINE,
        line_buffering=line_buffering,
    )


def _reader(data: bytes) -> io.TextIOWrapper:
    """``sys.stdin`` over the hook's bytes, as Python makes it over a pipe."""
    encoding, errors = _STREAM_FORM.get("stdin", ("utf-8", "strict"))
    return io.TextIOWrapper(
        io.BufferedReader(io.BytesIO(data)), encoding=encoding, errors=errors, newline=_NEWLINE
    )


def _read_tz() -> None:
    """Reads ``TZ`` again, as a process does when it starts: local times follow the hook's
    zone (POSIX; Windows reads its zone from the system)."""
    tzset = getattr(time, "tzset", None)
    if tzset is not None:
        tzset()


@contextlib.contextmanager
def _as_the_hook(
    env: Mapping[str, str], cwd: str | None, argv: list[str], stdin: bytes
) -> Iterator[dict[str, bytes]]:
    """For its block, this process is the hook's: its environment, directory, ``argv``,
    stdin and fds 1 and 2. Yields where the block's stdout and stderr land, filled once it
    ends, and puts every one of them back."""
    from aisquare.core import state

    _remember_stream_form()
    saved_env = dict(os.environ)
    try:
        saved_cwd: str | None = os.getcwd()
    except OSError:
        saved_cwd = None
    saved_argv = sys.argv
    saved_streams = (sys.stdin, sys.stdout, sys.stderr)
    saved_state = state.get_state()
    sys.stdout.flush()
    sys.stderr.flush()
    saved_fds = (os.dup(1), os.dup(2))
    captured: dict[str, bytes] = {}
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        try:
            for name in [*ENV_NAMES, *(name for name in os.environ if forwarded(name))]:
                if name not in env:
                    os.environ.pop(name, None)
            os.environ.update(env)
            _read_tz()
            if cwd:
                try:
                    os.chdir(cwd)
                except OSError as exc:
                    _say(f"hook: cannot enter {cwd!r} ({exc}); it runs in the brain's own")
            sys.argv = argv
            state.reset_state()
            os.dup2(out.fileno(), 1)
            os.dup2(err.fileno(), 2)
            sys.stdin = _reader(stdin)
            sys.stdout = _writer(1, "stdout", line_buffering=False)
            sys.stderr = _writer(2, "stderr", line_buffering=True)
            yield captured
        finally:
            for stream in (sys.stdout, sys.stderr):
                with contextlib.suppress(Exception):
                    stream.flush()
            sys.stdin, sys.stdout, sys.stderr = saved_streams
            os.dup2(saved_fds[0], 1)
            os.dup2(saved_fds[1], 2)
            for fd in saved_fds:
                os.close(fd)
            sys.argv = saved_argv
            state.set_state(saved_state)
            if saved_cwd is not None:
                with contextlib.suppress(OSError):
                    os.chdir(saved_cwd)
            os.environ.clear()
            os.environ.update(saved_env)
            _read_tz()
            for name, file in (("stdout", out), ("stderr", err)):
                file.seek(0)
                captured[name] = file.read()


def _run_rlx() -> int:
    """``aisquare.cli.app.main`` on ``sys.argv``, as the console script runs it: its exit
    status."""
    from aisquare.cli import app as app_module

    try:
        app_module.main()
    except SystemExit as exc:
        code = exc.code
        if code is None:
            return 0
        if isinstance(code, int):
            return code
        sys.stderr.write(f"{code}\n")
        return 1
    except BaseException:  # what the interpreter would print, and its status
        traceback.print_exc()
        return 1
    return 0


def _join_threads_since(before: set[threading.Thread]) -> None:
    """Waits for every non-daemon thread the hook started, as a process waits for them
    before it exits: what they print is the hook's, and none outlives its request."""
    for thread in threading.enumerate():
        if thread not in before and not thread.daemon and thread is not threading.current_thread():
            thread.join()


def _reap_children() -> None:
    """Collects every child that has ended, a detached worker the hook started among them,
    which would otherwise stay a zombie of the brain's until the next start of a process.
    POSIX only: Windows keeps no zombies."""
    if sys.platform == "win32":
        return
    while True:
        try:
            pid, _ = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return
        if pid == 0:
            return


def _hook(params: Mapping[str, Any]) -> Result:
    """``hook {event, stdin_b64, cwd?, env}``: ``rlx hook <event>`` with the hook's stdin,
    directory and allowlisted environment. ``{stdout_b64, stderr_b64, exit_code}``."""
    event = params.get("event")
    if event not in HOOK_EVENTS:
        raise BrainError("unknown_event", f"the brain runs no hook {event!r}")
    try:
        stdin = base64.b64decode(str(params.get("stdin_b64", "")), validate=True)
    except ValueError as exc:
        raise BrainError("invalid_params", f"stdin_b64 is not base64: {exc}") from exc
    cwd = params.get("cwd")
    raw_env = params.get("env", {})
    if not isinstance(raw_env, dict) or not all(
        isinstance(k, str) and isinstance(v, str) for k, v in raw_env.items()
    ):
        raise BrainError("invalid_params", "env must map names to strings")
    env = {name: value for name, value in raw_env.items() if forwarded(name)}
    fresh_caches()
    argv = [sys.argv[0], "hook", str(event)]
    before = set(threading.enumerate())
    with _as_the_hook(env, cwd if isinstance(cwd, str) else None, argv, stdin) as captured:
        code = _run_rlx()
        _join_threads_since(before)
    _reap_children()
    return {
        "stdout_b64": base64.b64encode(captured["stdout"]).decode("ascii"),
        "stderr_b64": base64.b64encode(captured["stderr"]).decode("ascii"),
        "exit_code": code,
    }


#: The methods, by name.
METHODS: dict[str, Method] = {"ping": _ping, "hook": _hook}


def _rlx_version() -> str:
    from aisquare.core.version import __version__

    return __version__


class Channel:
    """The protocol's two ends: frames read from ``reader``, written to ``writer``."""

    def __init__(self, reader: IO[bytes], writer: IO[bytes]) -> None:
        self._reader = reader
        self._writer = writer

    def read(self) -> dict[str, Any] | None:
        """The next frame, or ``None`` at the end of the input. Raises
        :class:`ProtocolError` for a line that is not one JSON object, or too long."""
        line = self._reader.readline(MAX_FRAME + 1)
        if not line:
            return None
        if len(line) > MAX_FRAME:
            raise ProtocolError(f"a frame is longer than {MAX_FRAME} bytes")
        try:
            frame = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise ProtocolError(f"a frame is not JSON: {exc}") from exc
        if not isinstance(frame, dict):
            raise ProtocolError("a frame is not a JSON object")
        return frame

    def write(self, frame: Mapping[str, Any]) -> None:
        """Writes ``frame`` as one line, at once."""
        line = json.dumps(frame, ensure_ascii=True, separators=(",", ":")) + "\n"
        self._writer.write(line.encode("ascii"))
        self._writer.flush()


def _say(message: str) -> None:
    """One line on the brain's log (its stderr, which rlxd keeps)."""
    sys.stderr.write(f"brain: {message}\n")
    sys.stderr.flush()


def _protocol_of(frame: Mapping[str, Any]) -> int | None:
    protocol = frame.get("protocol")
    if isinstance(protocol, bool) or not isinstance(protocol, int):
        return None
    return protocol


def greet(channel: Channel, frame: Mapping[str, Any]) -> bool:
    """Answers rlxd's hello, ``frame``: whether rlxd and this brain share a protocol.
    Refuses, and says why, when they do not."""
    if frame.get("type") != "hello":
        raise ProtocolError(f"the first frame must be hello, not {frame.get('type')!r}")
    protocol = _protocol_of(frame)
    if protocol not in PROTOCOLS:
        message = (
            f"rlxd speaks brain protocol {frame.get('protocol')!r}, and this rlx "
            f"{_rlx_version()} speaks {', '.join(map(str, PROTOCOLS))}: upgrade the older one"
        )
        channel.write(
            {
                "type": "refused",
                "code": "incompatible_protocol",
                "message": message,
                "supported": list(PROTOCOLS),
            }
        )
        _say(message)
        return False
    ours = {
        "env_allowlist": list(ENV_NAMES),
        "env_prefixes": list(ENV_PREFIXES),
        "env_secret_words": list(ENV_SECRET_WORDS),
    }
    differ = [key for key, value in ours.items() if frame.get(key) != value]
    if differ:
        theirs = frame.get("env_allowlist")
        named = set(theirs) if isinstance(theirs, list) else set()
        message = (
            f"rlxd sends other variables with a hook than this rlx {_rlx_version()} sets "
            f"({', '.join(differ)} differ: only rlxd's names "
            f"{sorted(map(str, named - set(ENV_NAMES)))}, only rlx's "
            f"{sorted(set(ENV_NAMES) - named)}); install the rlx and rlxd of one release"
        )
        channel.write({"type": "refused", "code": "env_allowlist_differs", "message": message})
        _say(message)
        return False
    channel.write(
        {"type": "hello", "protocol": protocol, "pid": os.getpid(), "rlx_version": _rlx_version()}
    )
    return True


def answer(method: str, params: Mapping[str, Any], methods: Mapping[str, Method]) -> Result:
    """One request's reply frame, less its ``type`` and ``id``: never raises."""
    handler = methods.get(method)
    if handler is None:
        return {
            "ok": False,
            "error": {"code": "unknown_method", "message": f"the brain has no method {method!r}"},
        }
    try:
        return {"ok": True, "result": handler(params)}
    except BrainError as exc:
        return {"ok": False, "error": {"code": exc.code, "message": exc.message}}
    except Exception as exc:  # a failed request never ends the brain
        _say(f"{method} failed: {type(exc).__name__}: {exc}")
        return {
            "ok": False,
            "error": {"code": "brain_failed", "message": f"{type(exc).__name__}: {exc}"},
        }


def serve(channel: Channel, methods: Mapping[str, Method] | None = None) -> int:
    """Greets rlxd, then answers its requests until its frames end: the exit status."""
    methods = METHODS if methods is None else methods
    try:
        hello = channel.read()
        if hello is None:
            return 0  # rlxd went away before it said anything
        if not greet(channel, hello):
            return 3
        while True:
            frame = channel.read()
            if frame is None:
                return 0
            if frame.get("type") != "request":
                raise ProtocolError(f"expected a request, not {frame.get('type')!r}")
            request_id = frame.get("id")
            method = frame.get("method")
            params = frame.get("params", {})
            if (
                isinstance(request_id, bool)
                or not isinstance(request_id, int)
                or not isinstance(method, str)
                or not isinstance(params, dict)
            ):
                raise ProtocolError("a request needs an integer id, a method and object params")
            channel.write({"type": "accepted", "id": request_id})
            channel.write({"type": "reply", "id": request_id, **answer(method, params, methods)})
    except ProtocolError as exc:
        _say(f"{exc}; exiting")
        return 2
    except (BrokenPipeError, ConnectionResetError):
        return 0  # rlxd is gone


def private_channel() -> Channel:
    """The protocol on private duplicates of fds 0 and 1, which nothing else uses: fd 0 is
    pointed at the null device and fd 1 at fd 2 (the brain's log) from here on."""
    reader = os.fdopen(os.dup(0), "rb")
    writer = os.fdopen(os.dup(1), "wb")
    sys.stdout.flush()
    null = os.open(os.devnull, os.O_RDWR)
    try:
        os.dup2(null, 0)
    finally:
        os.close(null)
    os.dup2(2, 1)
    return Channel(reader, writer)


def serve_stdio() -> int:
    """``rlx brain --stdio``: the protocol on this process's stdin and stdout."""
    _remember_stream_form()
    return serve(private_channel())
