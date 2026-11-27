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
   […]}`` first.
2. The brain answers ``{"type": "hello", "protocol": 1, "pid": …, "rlx_version": "…"}``,
   or, when rlxd's ``env_allowlist`` is not this brain's (:data:`ENV_NAMES`: rlxd-hook
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
has (:data:`ENV_NAMES`). For one request, the brain becomes that process:

- **Environment.** Each allowlisted variable is set to rlxd-hook's value, or removed when
  rlxd-hook had none, so neither rlxd's value nor an earlier hook's leaks in; ``TZ`` is
  read again (``time.tzset``), and so is the temporary folder (``tempfile.tempdir``).
  Every other variable is the brain's own, which rlxd started it with. What a process
  reads only as it starts is the brain's: the locale's ``LC_*`` set Python's own encodings
  then, and pass on to the hook's children only. So a worker a hook detaches
  (``services.hooks._detach``, the hand-over) starts from the brain's environment with the
  allowlist's values, not from the agent's whole one: the agent's ``LANG``, proxies or
  ``ANTHROPIC_BASE_URL`` are not its, as only the allowlist's names, none a credential,
  cross from rlxd-hook.
- **Directory.** ``os.chdir`` to rlxd-hook's working directory (the brain's own when that
  one is gone, as it can be by the time the hook runs).
- **Process.** ``sys.argv`` is ``[<this rlx>, "hook", <event>]``. ``sys.stdin`` reads the
  hook's bytes; fds 1 and 2 are pointed at files of their own and ``sys.stdout`` and
  ``sys.stderr`` made over them as Python makes them for a pipe (the encoding and errors
  of the brain's own, ``\r\n`` on Windows), so the bytes are the ones ``rlx hook`` writes,
  a child's output included. fd 0 stays the null device, which a child of ``rlx hook``
  would read as the end of a stdin its parent read to the end.
- **State.** ``core.state``'s runtime state is a fresh one, as the root callback makes it,
  and every module of rlx's is as a fresh process has it (below).

All of it is put back afterwards, whatever the hook did. Requests never overlap: rlxd sends
one at a time, so changing process-wide state for one is safe, and nothing of one is left
for the next.

**The end of a hook is a process's end.** The brain waits for each non-daemon thread the
hook started; runs the exit handlers it registered (``atexit.register`` is the brain's
for the hook), the last first, then those rlx's modules register at import
(:data:`EXIT_HANDLERS`: a Remote push the hook began is sent, tmux control clients close).
All of it inside the hook, so what they print is the hook's. It does not wait for the
daemon threads the hook started, as an exit waits for none: those still running are named
on the brain's log, never in the hook's stderr (``rlx hook``'s exit says nothing of them),
and run on in the brain until they end. Then it collects the children the hook let go of
(``subprocess``'s own record), and only those. A child still running after a hook is a
detached worker (``services.hooks._detach``), which leaves ``rlx hook`` behind on purpose
(the code index's refresh, a hand-over): on POSIX in a session of its own, outside the
brain's process group; on Windows its start asks to leave the brain's job
(:func:`breaking_away`), which rlxd's job allows. The protocol's own fds are not
inheritable, so no child holds the brain's pipes.

The allowlist (:data:`ENV_NAMES`), exact names, which rlxd-hook sends and the brain applies:

- **rlx's own the hook path reads:** the home (``AISQUARE_HOME``, and ``RLX_HOME``, its
  name from 0.10.2's sync on), the role, the fleet agent and its launch, the team and its
  hub, the broker's scope, the persona, the store's wait.
- **The agents' own,** which only their adapters name: ``AgentAdapter.hook_env`` (Claude
  Code's config dir, pid, temporary folder and effort). No credential is among them.
- **tmux's:** ``TMUX`` and ``TMUX_PANE``, the agent's pane.
- **Python's start:** ``PYTHONIOENCODING`` and ``PYTHONUTF8``, which Python reads only as a
  process starts. The brain's are rlxd's; rlxd hands a hook whose values differ to ``rlx
  hook`` (``hook_not_delivered``), so the brain's streams are always the ones ``rlx hook``
  would make.
- **The process's:** ``PATH`` (which ``tmux``, ``git`` and ``claude`` a hook's children
  run), the user's home (``HOME``, and Windows' ``USERPROFILE``), ``TZ`` (times a hook
  prints), the locale's ``LC_CTYPE``, the temporary folder (``TMPDIR``, Windows' ``TEMP``
  and ``TMP``), and Windows' user folders.

The parity test (``tests/test_brain_hooks.py``) runs each event both ways, ``rlx hook`` in a
process of its own and the brain, under three environments in turn in one brain, and
compares stdout and stderr byte for byte and the store's rows. It records every variable
the handlers read: each must be in the allowlist, or describe the brain's own process
(:data:`ENV_OF_THE_BRAIN`, and Python's own, :data:`ENV_OF_THE_INTERPRETER`); and each name
in the allowlist must be one they read, or be in :data:`ENV_READ_ELSEWHERE`, with why.

Module state (the staleness audit)
----------------------------------

A brain lives for days; ``rlx hook`` starts from its modules as they are imported, every
time. So the brain keeps that state as it is before its first request
(:func:`take_baseline`: every module-level value of every ``aisquare`` module, and a copy
of each container's content), and before each hook puts it back (:func:`fresh_state`).
The baseline is taken once the hook path is imported (:func:`_import_the_hook_path`: the CLI,
and every module of the agents' adapters, whose transcript readers and usage tallies a Stop
loads lazily), so no module a hook loads is first imported, and kept, after it.

- every ``functools`` cache is emptied: whatever it held is computed again from this
  hook's environment and files;
- every module-level name is bound to its object again, so state written with ``global``
  is back (``core.config._LAST_PARSE``, ``core.state._state``, ``core.program._in_app``,
  ``core.instances._this``, ``core.tmux._boot``…);
- every module-level container's content is put back in place, so state mutated where it
  lies is back too: ``core.launch_seal._SEEN`` (the session facts a hook was handed, which
  a later hook's ``adopted`` would otherwise act on), ``core.orchestrator._WARNED_HUBS``
  (a warning each ``rlx hook`` prints once), ``core.process_table``'s held lookups, the
  accounts' read-through caches;
- a name a hook added at module level is removed (a submodule it imported stays).

**State that outlives a request.** What a hook leaves running is the brain's from then on,
as it would have been the dying process's: a non-daemon thread is joined before the reply,
a daemon thread runs on until it ends (named on the log), a detached worker is the OS's.
The registries that track such threads and locks are containers like any other, so before
the next hook they are empty, as a fresh ``rlx hook`` process has them, while what they held
lives on in the brain: ``core.process_table._STUCK`` (a stuck ``ps``/``lsof`` thread: each
hook may start its own four, as each ``rlx hook`` process may, and does not wait for them),
``services.remote_push._push_in_flight``, and the locks of ``services.claude_accounts.
_FETCH_LOCKS`` and ``core.claude_accounts._KEYCHAIN_ASKING`` / ``_ITEM_ASKING`` (a thread of
an earlier hook still holding one no longer excludes the next hook's caller, as it would not
exclude another ``rlx hook`` process's). That is the fresh process's behaviour, by design.

Tests hold it: after the six events, every module-level value is its baseline's and every
container's content equals it; the session facts and the hub warning are shown back; and
the six events, with real payloads (a Stop's ``transcript_path``) for an agent on a managed
slot, import no module of rlx's the baseline lacks (one imported later would keep its state
across hooks).

Module-level values computed from the environment at import would be stale too: none on the
hook path is (no module of ``aisquare`` reads the environment, the directory or ``argv`` at
import), and the parity test's second and third environments would show one.
"""

from __future__ import annotations

import atexit
import base64
import collections
import contextlib
import copy
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import types
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

#: rlx's own variables the hook path reads: the home, the role, the fleet agent and its
#: launch, the team and its hub, the broker's scope, the persona, the store's wait, the
#: captain's switch, and a traced launch's marker (its pipeline, agent name and Run key,
#: which session-start's join to the gateway's Run records, ``services.explainability``).
_RLX_NAMES = (
    "AISQUARE_BOARD_CONTEXT",
    "AISQUARE_BROKER",
    "AISQUARE_BROKERED",
    "AISQUARE_CI",
    "AISQUARE_DB_BUSY_MS",
    "AISQUARE_EXPERIMENTAL_CAPTAIN",
    "AISQUARE_FLEET_AGENT",
    "AISQUARE_HOME",
    "AISQUARE_LAUNCH_ID",
    "AISQUARE_LAUNCH_PID",
    "AISQUARE_PERSONA",
    "AISQUARE_PIPELINE_ID",
    "AISQUARE_ROLE",
    "AISQUARE_RUN_TRACE_ID",
    "AISQUARE_TEAM",
    "AISQUARE_TEAM_DELTA",
    "AISQUARE_TEAM_HUB",
    "AISQUARE_TEAM_LEASE_MIN",
    "AISQUARE_TRACE_AGENT_NAME",
    "RLX_HOME",
)

#: tmux's: the pane the agent runs in.
_TMUX_NAMES = ("TMUX", "TMUX_PANE")

#: Python's, read only as a process starts: its stdio's encoding and UTF-8 mode. rlxd hands
#: a hook whose values are not the brain's own to ``rlx hook`` (it would not start as the
#: brain did); the brain sets the rest's, for the hook's children.
_PYTHON_NAMES = ("PYTHONIOENCODING", "PYTHONUTF8")

#: The process's own that the hook path reads: its programs, its user's home, its zone and
#: locale; and Windows' for the same (its user's home and folders, its temporary folder).
_PROCESS_NAMES = (
    "HOME",
    "LC_CTYPE",
    "PATH",
    "TZ",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "TMPDIR",
    "USERNAME",
    "USERPROFILE",
)


def _agent_names() -> set[str]:
    """Each agent's own, which only its adapter may name: what its hooks read
    (``AgentAdapter.hook_env``), and its accounts' variables, read off them so the list
    never drifts from them (``AgentAccounts``: its home's, each slot's others, and the
    copies a launch onto a slot keeps of the shell's own, ``plain_vars``, which tell the
    hook path a slot from the plain home, ``core.claude_accounts.plain_environment``)."""
    from aisquare.core.agent_adapters import adapters

    names: set[str] = set()
    for adapter in adapters():
        names.update(adapter.hook_env)
        accounts = adapter.accounts
        if accounts is not None:
            names.add(accounts.home_env)
            names.update(accounts.extra_env)
            names.update(accounts.plain_vars.values())
    return names


#: The allowlist, exact names only: the variables rlxd-hook sends with each hook, and the
#: brain sets for it (or removes, when rlxd-hook had none). rlxd-proto's
#: ``HOOK_ENV_NAMES`` is the same list, which rlxd names in its hello and the brain checks.
#: The parity test holds it to what the hooks read: every variable they read is here, and
#: every one here they read, or is in :data:`ENV_READ_ELSEWHERE`.
ENV_NAMES: tuple[str, ...] = tuple(
    sorted({*_RLX_NAMES, *_TMUX_NAMES, *_PYTHON_NAMES, *_PROCESS_NAMES, *_agent_names()})
)
_ENV_SET = frozenset(ENV_NAMES)

#: Names in the allowlist the parity test's recording does not see read (the C library, ``os``
#: itself, a program a hook starts, or another platform reads them), and why each is sent.
ENV_READ_ELSEWHERE: dict[str, str] = {
    "RLX_HOME": "the home's variable from 0.10.2's sync on, read before AISQUARE_HOME; "
    "rlxd's home check reads it now",
    "LC_CTYPE": "the locale the C library reads on macOS, where Python asks it",
    "APPDATA": "Windows: the user's roaming folder",
    "LOCALAPPDATA": "Windows: the user's local folder, where rlx keeps its own",
    "TEMP": "Windows: the temporary folder tempfile takes",
    "TMP": "Windows: the temporary folder tempfile takes when TEMP is unset",
    "TMPDIR": "the temporary folder tempfile takes first (os.getenv: os's own read), a "
    "process's own on macOS",
    "USERNAME": "Windows: the user's name, as getpass reads it",
    "USERPROFILE": "Windows: the user's home, where Path.home() looks",
    "PATH": "where every program a hook starts is found (os.get_exec_path, shutil.which: "
    "os's own reads), and the PATH a worker a hook detaches inherits",
    "TZ": "the zone the C library reads for every local time a hook prints or stores (the "
    "brain calls time.tzset); core.claude_accounts reads it for a reset's time",
    "TMUX": "the tmux server every tmux a hook starts reaches, and a worker it detaches "
    "(fleet switch, spawn) finds; core.instances, core.clipboard and services.fleet read it "
    "on paths these runs do not take",
    "TMUX_PANE": "the pane every tmux a hook starts, or a worker it detaches, takes as its "
    "own when a command names none",
    "PYTHONIOENCODING": "Python reads it as a process starts, for its stdio's encoding: rlxd "
    "does not deliver a hook whose value is not the brain's (it runs as a process of its "
    "own); a child of the hook reads it",
    "PYTHONUTF8": "Python reads it as a process starts (UTF-8 mode: stdio and text files): "
    "rlxd does not deliver a hook whose value is not the brain's (it runs as a process of "
    "its own); a child of the hook reads it",
    "HOME": "POSIX: the user's home, where Path.home() looks",
}


def forwarded(name: str) -> bool:
    """Whether the variable ``name`` crosses from rlxd-hook to the brain: one of
    :data:`ENV_NAMES`, as Windows compares names, whatever their case."""
    return name.upper() in _ENV_SET or name in _ENV_SET


#: Variables the hook path reads that describe the brain's own process or machine, not the
#: hook's, and so are not sent: the shell-completion switches of click and typer (a hook
#: never completes), macOS's per-user text encoding, and Windows' system folders and
#: program lookup, the same for every process of the user's.
ENV_OF_THE_BRAIN: frozenset[str] = frozenset(
    {
        "_AISQUARE_COMPLETE",
        "_RLX_COMPLETE",
        "_TYPER_COMPLETE_TEST_DISABLE_SHELL_DETECTION",
        "__CF_USER_TEXT_ENCODING",
        "ARCHFLAGS",
        "COMSPEC",
        "PATHEXT",
        "SYSTEMROOT",
        "WINDIR",
    }
)

#: The prefixes of the variables Python reads about itself (``sysconfig``, ``site``,
#: ``zoneinfo``, ``subprocess``): the brain's interpreter is the one ``rlx hook``'s would be.
ENV_OF_THE_INTERPRETER: tuple[str, ...] = ("PYTHON", "_PYTHON_")

#: The exit handlers rlx's modules register as they are imported, which a process runs as it
#: exits: the brain runs them after each hook (a test holds this to every ``atexit.register``
#: of rlx's): a Remote push the hook began is sent, tmux control clients it opened close.
EXIT_HANDLERS: tuple[tuple[str, str], ...] = (
    ("aisquare.services.remote_push", "push_drain"),
    ("aisquare.core.clipboard", "flush"),
    ("aisquare.core.tmux_control", "_tidy_up"),
)


#: The containers a module-level value can be, whose content :func:`fresh_state` puts back.
_CONTAINERS = (dict, list, set, collections.deque, bytearray)

#: Not a container: nothing of its content to put back.
_PLAIN = object()

#: Every module-level value of every rlx module, and the content of each container among
#: them, as they were before the first hook (:func:`take_baseline`).
_BASELINE: dict[tuple[str, str], tuple[object, object]] = {}
_BASELINE_MODULES: set[str] = set()


def _ours(name: str, module: object) -> bool:
    return (
        module is not None
        and (name == "aisquare" or name.startswith("aisquare."))
        and name != __name__
    )


def _copy(value: object, memo: dict[int, object] | None = None) -> object:
    """A copy of the container ``value`` and of every container in it; anything else in it
    is the same object, so a registry's entries (an adapter, a lock, a compiled pattern)
    stay the ones its holders hold, and only what holds them is put back."""
    memo = {} if memo is None else memo
    if id(value) in memo:
        return memo[id(value)]
    if isinstance(value, dict):
        made: dict[Any, Any] = copy.copy(value)
        memo[id(value)] = made
        for key, item in value.items():
            made[key] = _copy(item, memo)
        return made
    if isinstance(value, list):
        listed: list[Any] = copy.copy(value)
        memo[id(value)] = listed
        listed[:] = [_copy(item, memo) for item in value]
        return listed
    if isinstance(value, collections.deque):
        queued: collections.deque[Any] = copy.copy(value)
        memo[id(value)] = queued
        queued.clear()
        queued.extend(_copy(item, memo) for item in value)
        return queued
    if isinstance(value, (set, bytearray)):  # a set's members are hashable: kept as they are
        return copy.copy(value)
    return value


#: The modules the hook path loads (:func:`_import_the_hook_path`), imported before the
#: baseline so their state is put back like the rest: the CLI, which imports the handlers,
#: and every module of the agents' adapters, which load their transcript readers and usage
#: tallies only when a Stop with a ``transcript_path`` first asks. The parity test holds the
#: six events, with real payloads, to importing none the baseline lacks.
HOOK_PATH_MODULES: tuple[str, ...] = ("aisquare.cli.app",)
HOOK_PATH_PACKAGES: tuple[str, ...] = ("aisquare.core.agent_adapters",)


def _import_the_hook_path() -> None:
    """Imports what the six hooks load, before the baseline: :data:`HOOK_PATH_MODULES`, and
    each module of :data:`HOOK_PATH_PACKAGES` (no module there registers anything as it is
    imported; the package's own ``__init__`` registers the adapters)."""
    import importlib
    import pkgutil

    for name in HOOK_PATH_MODULES:
        importlib.import_module(name)
    for package_name in HOOK_PATH_PACKAGES:
        package = importlib.import_module(package_name)
        for found in pkgutil.iter_modules(package.__path__, f"{package_name}."):
            with contextlib.suppress(ImportError):  # one an optional dependency lacks
                importlib.import_module(found.name)


def take_baseline() -> None:
    """Keeps every module-level value of every rlx module loaded, and a copy of the content
    of each container among them: the state a fresh ``rlx hook`` starts from, which
    :func:`fresh_state` puts back before each hook. :func:`serve` takes it after the hello,
    before any request."""
    _import_the_hook_path()
    _BASELINE.clear()
    _BASELINE_MODULES.clear()
    for name, module in list(sys.modules.items()):
        if not _ours(name, module):
            continue
        _BASELINE_MODULES.add(name)
        for attribute, value in list(vars(module).items()):
            # A module's own (``__builtins__``, ``__dict__``-like dunders) is Python's.
            container = isinstance(value, _CONTAINERS) and not attribute.startswith("__")
            kept = _copy(value) if container else _PLAIN
            _BASELINE[(name, attribute)] = (value, kept)


def drop_baseline() -> None:
    """Forgets the baseline. :func:`serve` drops its own as it returns, so a process that
    goes on after it (a test's) keeps nothing of it: a later in-process hook would otherwise
    put back what that process changed since (a test's ``monkeypatch``)."""
    _BASELINE.clear()
    _BASELINE_MODULES.clear()


def _put_back(value: object, kept: object) -> None:
    """Gives the container ``value`` the content ``kept`` (a copy of it), in place, so every
    holder of it sees the change."""
    fresh = _copy(kept)
    if isinstance(value, dict) and isinstance(fresh, dict):
        value.clear()
        value.update(fresh)
    elif isinstance(value, (list, bytearray)) and isinstance(fresh, (list, bytearray)):
        value[:] = fresh
    elif isinstance(value, set) and isinstance(fresh, set):
        value.clear()
        value.update(fresh)
    elif isinstance(value, collections.deque) and isinstance(fresh, collections.deque):
        value.clear()
        value.extend(fresh)


def fresh_state() -> int:
    """Puts rlx's modules back as a fresh process has them: every ``functools`` cache
    emptied, and with a baseline (:func:`take_baseline`) every module-level name it knows
    bound to its object again, each container's content put back, and every name added
    since removed (but a submodule's). How many values it changed."""
    changed = 0
    for name, module in list(sys.modules.items()):
        if not _ours(name, module):
            continue
        for value in list(vars(module).values()):
            clear = getattr(value, "cache_clear", None)
            if callable(clear) and getattr(value, "__module__", None) == name:
                clear()
        if name not in _BASELINE_MODULES:
            continue
        namespace = vars(module)
        added = [
            attribute
            for attribute, value in namespace.items()
            if (name, attribute) not in _BASELINE and not isinstance(value, types.ModuleType)
        ]
        for attribute in added:  # a submodule imported since stays its package's
            del namespace[attribute]
            changed += 1
    for (name, attribute), (value, kept) in _BASELINE.items():
        holder = sys.modules.get(name)
        if holder is None:
            continue
        namespace = vars(holder)
        if namespace.get(attribute, _PLAIN) is not value:
            namespace[attribute] = value
            changed += 1
        if kept is not _PLAIN:
            try:
                same = bool(value == kept)
            except Exception:
                same = False
            if not same:
                _put_back(value, kept)
                changed += 1
    return changed


#: An exit handler a hook registered: the function, and what to call it with.
ExitHandler = tuple[Callable[..., Any], tuple[Any, ...], dict[str, Any]]

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


#: Windows' ``CREATE_BREAKAWAY_FROM_JOB``: a process started with it leaves the brain's job.
_CREATE_BREAKAWAY_FROM_JOB = 0x01000000


def breaking_away(popen: Callable[..., Any]) -> Callable[..., Any]:
    """``subprocess.Popen`` as a hook sees it on Windows: a start that asks for a session of
    its own (``start_new_session``, which Windows ignores: ``services.hooks._detach``'s
    workers ask it) asks to leave the brain's job as well, so the worker outlives the brain
    as it outlives ``rlx hook``. rlxd's job allows it; a job above rlxd's that does not
    refuses the start, which is then made in the brain's job."""

    def start(*args: Any, **kwargs: Any) -> Any:
        if kwargs.get("start_new_session"):
            flags = kwargs.get("creationflags") or 0
            try:
                return popen(
                    *args, **{**kwargs, "creationflags": flags | _CREATE_BREAKAWAY_FROM_JOB}
                )
            except OSError:
                pass
        return popen(*args, **kwargs)

    return start


@contextlib.contextmanager
def _as_the_hook(
    env: Mapping[str, str], cwd: str | None, argv: list[str], stdin: bytes
) -> Iterator[tuple[dict[str, bytes], list[ExitHandler]]]:
    """For its block, this process is the hook's: its environment, directory, ``argv``,
    stdin, fds 1 and 2, and on Windows its spawns' breakaway. Yields where the block's
    stdout and stderr land, filled once it ends, and the exit handlers registered in it,
    and puts every one of them back."""
    from aisquare.core import state

    _remember_stream_form()
    captured: dict[str, bytes] = {}
    handlers: list[ExitHandler] = []
    sys.stdout.flush()
    sys.stderr.flush()
    with contextlib.ExitStack() as held:
        out = held.enter_context(tempfile.TemporaryFile())
        err = held.enter_context(tempfile.TemporaryFile())
        saved_out = os.dup(1)
        held.callback(os.close, saved_out)
        saved_err = os.dup(2)
        held.callback(os.close, saved_err)
        saved_env = dict(os.environ)
        try:
            saved_cwd: str | None = os.getcwd()
        except OSError:
            saved_cwd = None
        saved_argv = sys.argv
        saved_streams = (sys.stdin, sys.stdout, sys.stderr)
        saved_state = state.get_state()
        saved_register, saved_unregister = atexit.register, atexit.unregister
        saved_popen = subprocess.Popen

        def register(function: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
            handlers.append((function, args, kwargs))
            return function

        def unregister(function: Callable[..., Any]) -> None:
            handlers[:] = [handler for handler in handlers if handler[0] != function]

        try:
            for name in [*ENV_NAMES, *(name for name in os.environ if forwarded(name))]:
                if name not in env:
                    os.environ.pop(name, None)
            os.environ.update(env)
            _read_tz()
            tempfile.tempdir = None  # read again, from the hook's TMPDIR, TEMP or TMP
            if cwd:
                try:
                    os.chdir(cwd)
                except OSError as exc:
                    _say(f"hook: cannot enter {cwd!r} ({exc}); it runs in the brain's own")
            sys.argv = argv
            state.reset_state()
            atexit.register, atexit.unregister = register, unregister  # type: ignore[assignment]
            if sys.platform == "win32":
                vars(subprocess)["Popen"] = breaking_away(saved_popen)
            os.dup2(out.fileno(), 1)
            os.dup2(err.fileno(), 2)
            sys.stdin = _reader(stdin)
            sys.stdout = _writer(1, "stdout", line_buffering=False)
            sys.stderr = _writer(2, "stderr", line_buffering=True)
            yield captured, handlers
        finally:
            for stream in (sys.stdout, sys.stderr):
                with contextlib.suppress(Exception):
                    stream.flush()
            sys.stdin, sys.stdout, sys.stderr = saved_streams
            os.dup2(saved_out, 1)
            os.dup2(saved_err, 2)
            vars(subprocess)["Popen"] = saved_popen
            atexit.register, atexit.unregister = saved_register, saved_unregister
            sys.argv = saved_argv
            state.set_state(saved_state)
            if saved_cwd is not None:
                with contextlib.suppress(OSError):
                    os.chdir(saved_cwd)
            os.environ.clear()
            os.environ.update(saved_env)
            _read_tz()
            tempfile.tempdir = None
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


def _end_as_a_process(
    before: set[threading.Thread],
    handlers: list[ExitHandler],
) -> list[str]:
    """What a process does as it exits, for the hook: waits for each non-daemon thread it
    started, then runs the exit handlers it registered (the last first), then rlx's own
    (:data:`EXIT_HANDLERS`), as ``atexit`` does. What they print is the hook's. A daemon
    thread is not waited for, as an exit waits for none: the names of those the hook
    started that still run, for the brain's log."""
    current = threading.current_thread()
    for thread in threading.enumerate():
        if thread not in before and not thread.daemon and thread is not current:
            thread.join()
    exits = [*reversed(handlers)]
    for module_name, attribute in EXIT_HANDLERS:
        module = sys.modules.get(module_name)
        function = getattr(module, attribute, None) if module is not None else None
        if callable(function):
            exits.append((function, (), {}))
    for function, args, kwargs in exits:
        try:
            function(*args, **kwargs)
        except Exception:  # as atexit prints it, and goes on
            print(f"Exception ignored in atexit callback {function!r}:", file=sys.stderr)
            traceback.print_exc()
    return [
        thread.name
        for thread in threading.enumerate()
        if thread not in before and thread is not current and thread.is_alive()
    ]


def _reap_abandoned() -> None:
    """Collects the children the hook started and let go of (``subprocess``'s own record of
    them: a detached worker that has ended, say), and no other: a child something still
    waits on keeps its status."""
    cleanup = getattr(subprocess, "_cleanup", None)
    if callable(cleanup):
        cleanup()


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
    fresh_state()
    argv = [sys.argv[0], "hook", str(event)]
    before = set(threading.enumerate())
    hook_cwd = cwd if isinstance(cwd, str) else None
    with _as_the_hook(env, hook_cwd, argv, stdin) as (captured, handlers):
        code = _run_rlx()
        left = _end_as_a_process(before, handlers)
    # On the brain's log, not the hook's stderr: rlx hook's exit would say nothing of them.
    if left:
        _say(f"hook {event}: daemon threads {left} still run after it (an exit stops them)")
    _reap_abandoned()
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
    theirs = frame.get("env_allowlist")
    if theirs != list(ENV_NAMES):
        named = set(theirs) if isinstance(theirs, list) else set()
        message = (
            f"rlxd sends other variables with a hook than this rlx {_rlx_version()} sets: "
            f"only rlxd's {sorted(map(str, named - set(ENV_NAMES)))}, only rlx's "
            f"{sorted(set(ENV_NAMES) - named)}; install the rlx and rlxd of one release"
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
        take_baseline()
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
    finally:
        drop_baseline()


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
