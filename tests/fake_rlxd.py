"""A stand-in for rlxd and its command line, for ``tests/test_upgrade_daemons.py``.

Run as ``python tests/fake_rlxd.py <command> …`` with ``FAKE_RLXD_DIR`` naming a folder:

- ``serve --background`` is the daemon: it registers in this home's daemon registry
  (``core.daemons``, kind ``rlxd``, restartable, its argv this very command), reads its
  version from ``<dir>/version`` (the "binary" a swap replaces), and runs until
  ``<dir>/stop-<kind>`` appears or it is signalled. As real rlxd does, it always finishes a
  stop it was asked for: while ``<dir>/stuck-<kind>`` is there it keeps running, and it
  stops once that is gone. ``<dir>/teardown`` ends every one of them, for good (a test's
  end). ``<dir>/argv-<kind>`` (a JSON list) is the argv it records instead of its own, and
  ``<dir>/mute-<kind>`` (seconds) how long it is registered before it answers.
  ``watch`` is the same, as a ``watch`` daemon.
- ``status --json``, ``call <method> [params]`` and ``stop`` answer as rlxd's command line:
  one JSON envelope (``ok``, ``data`` or ``error``). The runs and PTYs are
  ``<dir>/state.json``'s, which a test writes; ``pty.kill`` marks one exited.

``state.json`` is read and written only under ``<dir>/state.lock``
(:func:`read_state`, :func:`update_state`), the test's writes as the fake's: a write
is a read, a change and a write under the one lock, so none loses another's (#44). A
daemon saves its own entry before it registers, so a test that sees it live writes after
it. ``FAKE_RLXD_SAVE_LATE=<seconds>`` (a test's) puts back the old order, registered
first, and holds its save that long between its read and its write, with
``<dir>/saving`` there meanwhile.
"""

from __future__ import annotations

import json
import os
import signal
import sys
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any, NoReturn

from aisquare.core import daemons, locking

#: How long a read or a write of ``state.json`` waits for another to finish.
STATE_WAIT_S = 20.0


def _dir() -> Path:
    return Path(os.environ["FAKE_RLXD_DIR"])


def _read(directory: Path) -> dict[str, Any]:
    try:
        loaded: dict[str, Any] = json.loads((directory / "state.json").read_text(encoding="utf-8"))
        return loaded
    except (OSError, ValueError):
        return {}


def read_state(directory: Path) -> dict[str, Any]:
    """``<directory>/state.json``, read under its lock: ``{}`` before the first write."""
    with locking.holding(directory / "state.lock", wait_s=STATE_WAIT_S):
        return _read(directory)


def update_state(
    directory: Path, change: Callable[[dict[str, Any]], None], *, hold_s: float = 0.0
) -> None:
    """Changes ``<directory>/state.json`` as ``change`` changes what it holds: read, changed
    and written under its one lock, so this write and another's never lose each other.
    ``hold_s`` holds it between the read and the write, ``<directory>/saving`` there
    meanwhile (``FAKE_RLXD_SAVE_LATE``)."""
    with locking.holding(directory / "state.lock", wait_s=STATE_WAIT_S):
        state = _read(directory)
        if hold_s:
            (directory / "saving").touch()
            time.sleep(hold_s)
        change(state)
        (directory / "state.json").write_text(json.dumps(state), encoding="utf-8")
        (directory / "saving").unlink(missing_ok=True)


def _state() -> dict[str, Any]:
    return read_state(_dir())


def _say(envelope: dict[str, Any], code: int = 0) -> NoReturn:
    print(json.dumps(envelope))
    sys.exit(code)


def _live() -> daemons.Daemon | None:
    found = daemons.list_live(kind="rlxd", prune=False)
    return found[0] if found else None


def _answering() -> daemons.Daemon | None:
    """The live rlxd, once it answers (``mute-rlxd``)."""
    live = _live()
    if live is None or time.time() < float(_state().get("rlxd", {}).get("answers_at", 0.0)):
        return None
    return live


def _daemon(kind: str) -> None:
    asked = False

    def stop(signum: int, frame: object) -> None:
        nonlocal asked
        asked = True  # taken, and finished once not stuck

    # As rlx's watch does (services.watch._stop_on): a SIGTERM ignored at start stays so.
    if signal.getsignal(signal.SIGTERM) is not signal.SIG_IGN:
        signal.signal(signal.SIGTERM, stop)
    directory = _dir()
    version = (directory / "version").read_text(encoding="utf-8").strip()
    argv = [sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]]
    planted = directory / f"argv-{kind}"  # a record that names another program (a review's H1a)
    if planted.exists():
        argv = json.loads(planted.read_text(encoding="utf-8"))
    mute = directory / f"mute-{kind}"
    answers_at = time.time() + float(mute.read_text(encoding="utf-8")) if mute.exists() else 0.0
    entry = {"pid": os.getpid(), "version": version, "answers_at": answers_at}

    def save(state: dict[str, Any]) -> None:
        state[kind] = entry

    late = float(os.environ.get("FAKE_RLXD_SAVE_LATE", "0"))
    if not late:
        # Saved before it is live: a test that sees it registered writes after this.
        update_state(directory, save)
    registration = daemons.register(kind, argv=argv, restartable=True)  # type: ignore[arg-type]
    if late:
        update_state(directory, save, hold_s=late)
    try:
        while not (directory / "teardown").exists():
            flag = directory / f"stop-{kind}"
            if flag.exists():
                flag.unlink(missing_ok=True)
                asked = True
            if asked and not (directory / f"stuck-{kind}").exists():
                break
            time.sleep(0.05)
    finally:
        registration.deregister()


def main() -> None:
    command = sys.argv[1:]
    if command[:1] == ["serve"]:
        _daemon("rlxd")
        return
    if command[:1] == ["watch"]:
        _daemon("watch")
        return
    if command == ["status", "--json"]:
        live = _answering()
        if live is None:
            _say({"ok": False, "error": {"code": "not_running", "message": "not running"}}, 3)
        version = _state().get("rlxd", {}).get("version")
        _say({"ok": True, "data": {"pid": live.pid, "version": version}})
    if command[:1] == ["call"]:
        if _answering() is None:
            _say({"ok": False, "error": {"code": "not_running", "message": "not running"}}, 3)
        method = command[1]
        params = json.loads(command[2]) if len(command) > 2 else {}
        state = _state()
        if method == "run.list":
            wanted = set(params.get("states") or [])
            runs = [r for r in state.get("runs", []) if not wanted or r.get("state") in wanted]
            _say({"ok": True, "data": {"runs": runs}})
        if method == "pty.list":
            _say({"ok": True, "data": {"ptys": state.get("ptys", [])}})
        if method == "pty.kill":

            def kill(state: dict[str, Any]) -> None:
                for pty in state.get("ptys", []):
                    if pty.get("pty_id") == params.get("pty_id"):
                        pty["exited"] = True
                state.setdefault("killed", []).append(params.get("pty_id"))

            update_state(_dir(), kill)
            _say({"ok": True, "data": {}})
        _say({"ok": False, "error": {"code": "unknown_method", "message": method}}, 1)
    if command == ["stop"]:
        live = _live()
        if live is None:
            _say({"ok": True, "data": {}})
        (_dir() / "stop-rlxd").write_text("", encoding="utf-8")
        deadline = time.monotonic() + float(os.environ.get("FAKE_RLXD_STOP_WAIT", "10"))
        while time.monotonic() < deadline:
            if _live() is None:
                _say({"ok": True, "data": {}})
            time.sleep(0.05)
        _say({"ok": False, "error": {"code": "stop_timed_out", "message": "still running"}}, 1)
    _say({"ok": False, "error": {"code": "usage", "message": " ".join(command)}}, 2)


if __name__ == "__main__":
    main()
