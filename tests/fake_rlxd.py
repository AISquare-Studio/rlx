"""A stand-in for rlxd and its command line, for ``tests/test_upgrade_daemons.py``.

Run as ``python tests/fake_rlxd.py <command> …`` with ``FAKE_RLXD_DIR`` naming a folder:

- ``serve --background`` is the daemon: it registers in this home's daemon registry
  (``core.daemons``, kind ``rlxd``, restartable, its argv this very command), reads its
  version from ``<dir>/version`` (the "binary" a swap replaces), and runs until
  ``<dir>/stop-<kind>`` appears or it is signalled. As real rlxd does, it always finishes a
  stop it was asked for: while ``<dir>/stuck-<kind>`` is there it keeps running, and it
  stops once that is gone. ``<dir>/teardown`` ends every one of them, for good (a test's
  end). ``watch`` is the same, as a ``watch`` daemon.
- ``status --json``, ``call <method> [params]`` and ``stop`` answer as rlxd's command line:
  one JSON envelope (``ok``, ``data`` or ``error``). The runs and PTYs are
  ``<dir>/state.json``'s, which a test writes; ``pty.kill`` marks one exited.
"""

from __future__ import annotations

import json
import os
import signal
import sys
import time
from pathlib import Path
from typing import Any, NoReturn

from aisquare.core import daemons

DIR = Path(os.environ["FAKE_RLXD_DIR"])
STATE = DIR / "state.json"


def _state() -> dict[str, Any]:
    try:
        loaded: dict[str, Any] = json.loads(STATE.read_text(encoding="utf-8"))
        return loaded
    except (OSError, ValueError):
        return {}


def _save(state: dict[str, Any]) -> None:
    STATE.write_text(json.dumps(state), encoding="utf-8")


def _say(envelope: dict[str, Any], code: int = 0) -> NoReturn:
    print(json.dumps(envelope))
    sys.exit(code)


def _live() -> daemons.Daemon | None:
    found = daemons.list_live(kind="rlxd", prune=False)
    return found[0] if found else None


def _daemon(kind: str) -> None:
    asked = False

    def stop(signum: int, frame: object) -> None:
        nonlocal asked
        asked = True  # taken, and finished once not stuck

    signal.signal(signal.SIGTERM, stop)
    version = (DIR / "version").read_text(encoding="utf-8").strip()
    argv = [sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]]
    registration = daemons.register(kind, argv=argv, restartable=True)  # type: ignore[arg-type]
    state = _state()
    state[kind] = {"pid": os.getpid(), "version": version}
    _save(state)
    try:
        while not (DIR / "teardown").exists():
            flag = DIR / f"stop-{kind}"
            if flag.exists():
                flag.unlink(missing_ok=True)
                asked = True
            if asked and not (DIR / f"stuck-{kind}").exists():
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
        live = _live()
        if live is None:
            _say({"ok": False, "error": {"code": "not_running", "message": "not running"}}, 3)
        version = _state().get("rlxd", {}).get("version")
        _say({"ok": True, "data": {"pid": live.pid, "version": version}})
    if command[:1] == ["call"]:
        if _live() is None:
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
            for pty in state.get("ptys", []):
                if pty.get("pty_id") == params.get("pty_id"):
                    pty["exited"] = True
            state.setdefault("killed", []).append(params.get("pty_id"))
            _save(state)
            _say({"ok": True, "data": {}})
        _say({"ok": False, "error": {"code": "unknown_method", "message": method}}, 1)
    if command == ["stop"]:
        live = _live()
        if live is None:
            _say({"ok": True, "data": {}})
        (DIR / "stop-rlxd").write_text("", encoding="utf-8")
        deadline = time.monotonic() + float(os.environ.get("FAKE_RLXD_STOP_WAIT", "10"))
        while time.monotonic() < deadline:
            if _live() is None:
                _say({"ok": True, "data": {}})
            time.sleep(0.05)
        _say({"ok": False, "error": {"code": "stop_timed_out", "message": "still running"}}, 1)
    _say({"ok": False, "error": {"code": "usage", "message": " ".join(command)}}, 2)


if __name__ == "__main__":
    main()
