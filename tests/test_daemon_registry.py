"""The real rlxd binary, read by Python's daemon registry (handoff 0.10.3, 4.1).

While rlxd runs, :func:`aisquare.core.daemons.list_live` lists it, by the lock rlxd holds
(``flock``, or ``LockFileEx`` on byte 0 on Windows, against ``core.locking``'s), and the
resident brain it runs (1.6), whose record's lock rlxd holds too. After a stop both are
gone; after a kill the OS frees rlxd's locks, so both read as dead.

``RLXD_BIN`` names the binary; ``.github/workflows/rlxd.yml`` sets it to the one its job
built. The tests skip when ``RLXD_BIN`` is unset, so the Python suite, which builds no
Rust, passes without it, unless ``RLXD_INTEROP_REQUIRED`` is set (rlxd.yml sets it): then
a missing binary fails instead. ``RLXD_BIN`` set to a file that is not there always fails.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest

from aisquare.core import daemons

# Read at import: the suite's autouse fixtures clear what they call ambient, not these.
_RLXD_BIN = os.environ.get("RLXD_BIN") or None
_REQUIRED = bool(os.environ.get("RLXD_INTEROP_REQUIRED"))


def _rlxd() -> Path:
    if _RLXD_BIN is None:
        if _REQUIRED:
            pytest.fail("RLXD_INTEROP_REQUIRED is set, but RLXD_BIN names no rlxd binary")
        pytest.skip("RLXD_BIN is unset: rlxd.yml runs this with the rlxd it built")
    path = Path(_RLXD_BIN)
    if not path.is_file():
        pytest.fail(f"RLXD_BIN={_RLXD_BIN} is not a file")
    return path


class Rlxd:
    """The rlxd binary, run in one fresh home."""

    def __init__(self, binary: Path, home: Path) -> None:
        self.binary = binary
        self.home = home

    def run(self, *args: str) -> subprocess.CompletedProcess[str]:
        env = {**os.environ, "AISQUARE_HOME": str(self.home)}
        env["RLXD_CLAUDE_BIN"] = str(self.home / "no-claude")
        env["RLXD_RLX_BIN"] = _rlx()
        env.pop("RLX_HOME", None)
        return subprocess.run(
            [str(self.binary), *args],
            env=env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )

    def start(self) -> tuple[int, int]:
        """Starts rlxd, which returns once its brain runs: rlxd's pid and the brain's."""
        started = self.run("serve", "--detach")
        assert started.returncode == 0, started
        data = json.loads(started.stdout)["data"]
        pid, brain = data["pid"], data["brain"].get("pid")
        assert isinstance(pid, int)
        assert isinstance(brain, int), data["brain"]
        return pid, brain


def _rlx() -> str:
    """This checkout's ``rlx``, which rlxd runs as its brain."""
    found = shutil.which("rlx", path=str(Path(sys.executable).parent))
    assert found, f"no rlx beside {sys.executable}"
    return found


@pytest.fixture
def rlxd(monkeypatch: pytest.MonkeyPatch) -> Iterator[Rlxd]:
    """rlxd in a fresh home with a short path, as a Unix socket's path must fit about 100
    bytes (pytest's tmp_path on macOS does not). Python's registry reads that home."""
    binary = _rlxd()
    home = Path(tempfile.mkdtemp(prefix="rlxd-"))
    monkeypatch.setenv("AISQUARE_HOME", str(home))
    monkeypatch.delenv("RLX_HOME", raising=False)
    try:
        yield Rlxd(binary, home)
    finally:
        Rlxd(binary, home).run("stop")
        shutil.rmtree(home, ignore_errors=True)


def _live() -> list[tuple[str, int]]:
    return [(daemon.kind, daemon.pid) for daemon in daemons.list_live()]


def _eventually(what: str, done: Callable[[], bool]) -> None:
    deadline = time.monotonic() + 10
    while not done():
        assert time.monotonic() < deadline, f"never: {what}"
        time.sleep(0.05)


def test_a_running_rlxd_reads_as_live_and_after_a_stop_is_gone(rlxd: Rlxd) -> None:
    pid, brain = rlxd.start()
    assert sorted(_live()) == [("brain", brain), ("rlxd", pid)]
    records = {record.kind: record for record in daemons.list_live()}
    assert records["rlxd"].restartable is True
    assert Path(records["rlxd"].argv[0]).is_absolute()
    assert records["rlxd"].argv[1:] == ("serve", "--detach")
    # rlxd starts its brain again itself: an upgrade does not.
    assert records["brain"].restartable is False
    assert records["brain"].argv == (_rlx(), "brain", "--stdio")

    stopped = rlxd.run("stop")
    assert stopped.returncode == 0, stopped
    assert _live() == []
    assert sorted(p.name for p in (rlxd.home / "daemons").iterdir()) == [".gate.lock"]


def test_after_a_kill_rlxd_reads_as_dead(rlxd: Rlxd) -> None:
    pid, brain = rlxd.start()
    assert sorted(_live()) == [("brain", brain), ("rlxd", pid)]
    if sys.platform == "win32":
        # os.kill is TerminateProcess there, as Stop-Process -Force is.
        os.kill(pid, signal.SIGTERM)
    else:
        os.kill(pid, signal.SIGKILL)
    _eventually("the killed rlxd reads as dead", lambda: _live() == [])
