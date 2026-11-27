"""``rlx upgrade`` and rlx's daemons (handoff 0.10.3, 4.2 and 7.8's Upgrade part).

The daemons are real processes: ``tests/fake_rlxd.py`` stands in for rlxd (a daemon that
registers in this home's registry and answers rlxd's command line) and for a ``watch``. The
installer, PyPI and the new install's ``daemon install`` are this module's stand-ins
(``tests.installer_seams`` closes the real ones). The handoff's Accept lines run against
real builds in rlxd-release.yml's ``upgrade`` job.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Iterator
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import daemons as daemons_core
from aisquare.core import locking, registry
from aisquare.services import install_route, lifecycle, upgrade_daemons
from aisquare.services.install_route import Captured, Facts, LatestRelease
from aisquare.services.upgrade_daemons import Around, Events, Options, Refused
from tests.installer_seams import no_real_installer  # noqa: F401 — autouse, applied by import

FAKE = Path(__file__).with_name("fake_rlxd.py")
REAL_PYTHON_COMMAND = upgrade_daemons.python_command
REAL_RLXD_COMMAND = upgrade_daemons.rlxd_command
"""The real commands records must start with, which the `fake` fixture replaces."""


@dataclass
class Daemons:
    """The fake rlxd's folder, and what the test started."""

    dir: Path
    pids: list[int] = field(default_factory=list)
    swap_fails: bool = False
    swaps: list[list[str]] = field(default_factory=list)

    @property
    def rlxd(self) -> upgrade_daemons.Rlxd:
        return upgrade_daemons.Rlxd((sys.executable, str(FAKE)))

    def version(self, version: str) -> None:
        (self.dir / "version").write_text(version, encoding="utf-8")

    def state(self, **values: Any) -> None:
        path = self.dir / "state.json"
        state = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
        state.update(values)
        path.write_text(json.dumps(state), encoding="utf-8")

    def read_state(self) -> dict[str, Any]:
        state: dict[str, Any] = json.loads((self.dir / "state.json").read_text(encoding="utf-8"))
        return state

    def start(self, kind: str = "rlxd") -> daemons_core.Daemon:
        argv = [sys.executable, str(FAKE), "serve" if kind == "rlxd" else "watch", "--detach"]
        upgrade_daemons.spawn_daemon(
            daemons_core.Daemon(
                id="000000000000",
                kind=kind,
                pid=0,
                host="",
                version="",
                started_at=registry.moment("2027-01-01T00:00:00+00:00"),
                heartbeat_at=registry.moment("2027-01-01T00:00:00+00:00"),
                argv=tuple(argv),
                restartable=True,
            )
        )
        return self.wait_live(kind)

    def wait_live(self, kind: str, *, not_pid: int | None = None) -> daemons_core.Daemon:
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            for daemon in daemons_core.list_live(kind=kind, prune=False):
                if daemon.pid != not_pid:
                    self.pids.append(daemon.pid)
                    return daemon
            time.sleep(0.05)
        raise AssertionError(f"no live {kind}")

    def live(self, kind: str) -> list[daemons_core.Daemon]:
        return daemons_core.list_live(kind=kind, prune=False)


@pytest.fixture
def fake(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Daemons]:
    """The fake rlxd's folder, version 1.0.0; rlxd found as the fake, installed beside rlx;
    the new install's ``daemon install`` a stand-in that swaps the fake's version."""
    folder = tmp_path / "fake-rlxd"
    folder.mkdir()
    world = Daemons(folder)
    world.version("1.0.0")
    world.state()
    monkeypatch.setenv("FAKE_RLXD_DIR", str(folder))
    monkeypatch.setenv("FAKE_RLXD_STOP_WAIT", "1")  # `rlxd stop` gives up after 1 s
    monkeypatch.setattr(upgrade_daemons, "find_rlxd", lambda **_: world.rlxd)
    monkeypatch.setattr(upgrade_daemons, "installed_rlxd", lambda: folder / "rlxd")
    # The fake's records, as rlx's and rlxd's own are: `<fake> serve --detach`, and
    # `<fake> watch --detach` where rlx writes `python -P -m aisquare watch --detach`.
    fake_program = str(FAKE.resolve())
    monkeypatch.setattr(
        upgrade_daemons,
        "rlxd_command",
        lambda: (sys.executable, fake_program, "serve", "--detach"),
    )
    monkeypatch.setattr(upgrade_daemons, "python_command", lambda: (sys.executable, fake_program))
    monkeypatch.setattr(upgrade_daemons, "STOP_TIMEOUT_SECONDS", 8.0)
    monkeypatch.setattr(upgrade_daemons, "RESTART_WHEN_GONE_SECONDS", 60.0)
    monkeypatch.setattr(upgrade_daemons, "RESTART_GRACE_SECONDS", 1.0)

    def run_captured(argv: Any, *, timeout: float) -> Captured:
        argv = list(argv)
        if argv[-2:] == ["daemon", "install"]:
            world.swaps.append(argv)
            if world.swap_fails:
                said = {"error": "checksum_mismatch", "detail": "expected aa, got bb"}
                return Captured(1, json.dumps(said))
            world.version(world.dir.joinpath("next").read_text(encoding="utf-8"))
            return Captured(0, json.dumps({"version": "next"}))
        if argv[-1] == "--version":
            return Captured(0, f"aisquare {world.dir.joinpath('next').read_text()}\n")
        return Captured(0, "connected\n")

    monkeypatch.setattr(install_route, "run_captured", run_captured)
    (folder / "next").write_text("2.0.0", encoding="utf-8")
    yield world
    (folder / "teardown").write_text("", encoding="utf-8")  # every fake ends, for good
    (folder / "stuck-rlxd").unlink(missing_ok=True)
    for kind in ("rlxd", "watch"):
        (folder / f"stop-{kind}").write_text("", encoding="utf-8")
    for pid in world.pids:
        with contextlib.suppress(OSError):
            os.kill(pid, 15)
    deadline = time.monotonic() + 10
    while daemons_core.list_live(prune=False) and time.monotonic() < deadline:
        time.sleep(0.05)


def _events(out: io.StringIO) -> list[dict[str, Any]]:
    return [json.loads(line) for line in out.getvalue().splitlines()]


def _steps(events: list[dict[str, Any]]) -> list[tuple[str, str]]:
    return [(e["data"]["step"], e["data"]["status"]) for e in events if e["type"] == "upgrade.step"]


def _around(world: Daemons, options: Options) -> tuple[Around, io.StringIO]:
    out = io.StringIO()
    found = upgrade_daemons.census(world.rlxd)
    around = Around(
        found,
        options,
        Events(json_lines=True, out=out, err=io.StringIO()),
        world.rlxd,
        new_install=lambda: [sys.executable, "-P", "-m", "aisquare"],
    )
    return around, out


# --- the census --------------------------------------------------------------------------


def test_the_census_counts_daemons_runs_in_flight_and_live_ptys(fake: Daemons) -> None:
    rlxd = fake.start()
    fake.state(
        runs=[
            {"run_id": "r1", "kind": "task", "state": "running", "job_id": None},
            {"run_id": "r2", "kind": "task", "state": "done", "job_id": None},
        ],
        ptys=[
            {"pty_id": "p1", "pid": 7, "argv": ["sh"], "exited": False},
            {"pty_id": "p2", "pid": 8, "argv": ["sh"], "exited": True},
        ],
    )

    found = upgrade_daemons.census(fake.rlxd)

    assert [d.pid for d in found.daemons] == [rlxd.pid]
    assert [run["run_id"] for run in found.runs] == ["r1"], "queued and running only"
    assert [pty["pty_id"] for pty in found.ptys] == ["p1"], "live ones only"
    shown = found.as_json()
    assert set(shown) == {
        "daemons",
        "runs_in_flight",
        "ptys",
        "rlxd_error",
        "rlxd_version",
        "restarts_pending",
    }
    assert shown["daemons"][0]["kind"] == "rlxd" and shown["daemons"][0]["restartable"]
    assert shown["runs_in_flight"] == [
        {"run_id": "r1", "kind": "task", "state": "running", "job_id": None}
    ]
    assert shown["ptys"][0]["pty_id"] == "p1"


def test_with_no_rlxd_running_there_are_no_runs_or_ptys_to_count(fake: Daemons) -> None:
    found = upgrade_daemons.census(fake.rlxd)
    assert found.daemons == () and found.runs == () and found.ptys == ()
    assert found.rlxd_error is None


# --- refusals ------------------------------------------------------------------------------


def _census(**values: Any) -> upgrade_daemons.Census:
    daemon = daemons_core.Daemon(
        id="0123456789ab",
        kind="rlxd",
        pid=4242,
        host=registry.host_name(),
        version="1.0.0",
        started_at=registry.moment("2027-01-01T00:00:00+00:00"),
        heartbeat_at=registry.moment("2027-01-01T00:00:00+00:00"),
        argv=("rlxd", "serve", "--background"),
        restartable=False,
    )
    return upgrade_daemons.Census(daemons=(daemon,), **values)


def test_windows_refuses_a_running_daemon_unless_it_is_restarted() -> None:
    found = _census()
    refused = upgrade_daemons.refusal(found, Options(), windows=True)
    assert refused is not None and refused.code == "daemon_running"
    assert upgrade_daemons.refusal(found, Options(restart_daemons=True), windows=True) is None
    # macOS and Linux only warn: mixed releases until the daemons restart.
    assert upgrade_daemons.refusal(found, Options(), windows=False) is None
    assert "keeps running the release" in (upgrade_daemons.mixed_releases(found, Options()) or "")
    assert upgrade_daemons.mixed_releases(found, Options(restart_daemons=True)) is None


def test_a_live_pty_refuses_a_restart_unless_it_may_be_ended() -> None:
    found = _census(ptys=({"pty_id": "p1"},))
    refused = upgrade_daemons.refusal(found, Options(restart_daemons=True), windows=False)
    assert refused is not None and refused.code == "ptys_live" and refused.detail == "p1"
    ended = Options(restart_daemons=True, end_ptys=True)
    assert upgrade_daemons.refusal(found, ended, windows=False) is None
    # Without a restart, rlxd keeps running and so does the PTY.
    assert upgrade_daemons.refusal(found, Options(), windows=False) is None


def test_runs_in_flight_refuse_the_upgrade_restarted_or_not_unless_it_waits() -> None:
    found = _census(runs=({"run_id": "r1"},))
    for options in (Options(), Options(restart_daemons=True)):
        refused = upgrade_daemons.refusal(found, options, windows=False)
        assert refused is not None and refused.code == "runs_in_flight"
    assert upgrade_daemons.refusal(found, Options(wait_runs=1), windows=False) is None


def test_wait_runs_waits_for_the_runs_to_end_and_refuses_once_its_minutes_pass(
    fake: Daemons, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake.start()
    fake.state(runs=[{"run_id": "r1", "kind": "task", "state": "running"}])
    monkeypatch.setattr(upgrade_daemons, "WAIT_POLL_SECONDS", 0.1)
    out = io.StringIO()
    events = Events(json_lines=True, out=out, err=io.StringIO())
    found = upgrade_daemons.census(fake.rlxd)

    with pytest.raises(Refused) as refused:
        upgrade_daemons.wait_for_runs(found, Options(wait_runs=1 / 60), fake.rlxd, events)
    assert refused.value.code == "runs_in_flight"

    threading.Timer(1.0, lambda: fake.state(runs=[])).start()
    waited, said = upgrade_daemons.wait_for_runs(found, Options(wait_runs=0.5), fake.rlxd, events)
    assert waited.runs == ()
    assert said is not None and said.startswith("waited ")
    assert _events(out) == [], "said on stderr: no second `check started` (review n1)"


# --- around the install --------------------------------------------------------------------


def test_restart_daemons_stops_them_swaps_rlxd_and_starts_them_on_the_new_release(
    fake: Daemons,
) -> None:
    old = fake.start()
    watch = fake.start("watch")
    around, out = _around(fake, Options(restart_daemons=True))

    around.before_install()
    assert fake.live("rlxd") == [] and fake.live("watch") == [], "both stopped"
    around.installed("2.0.0", None)
    assert len(fake.swaps) == 1, "the new install's own daemon install"
    around.finish(())

    new = fake.wait_live("rlxd", not_pid=old.pid)
    assert new.pid != old.pid
    assert fake.rlxd.daemon_status() == {"pid": new.pid, "version": "2.0.0"}
    assert fake.wait_live("watch", not_pid=watch.pid).pid != watch.pid
    assert around.restarted == ["rlxd", "watch"]
    assert around.failure is None
    assert _steps(_events(out)) == [
        ("check", "done"),
        ("stop_daemons", "started"),
        ("stop_daemons", "done"),
        ("install", "started"),
        ("install", "done"),
        ("rlxd", "started"),
        ("rlxd", "done"),
        ("refresh_hooks", "done"),
        ("start_daemons", "started"),
        ("start_daemons", "done"),
    ]


def test_end_ptys_ends_each_live_pty_before_rlxd_stops(fake: Daemons) -> None:
    fake.start()
    fake.state(ptys=[{"pty_id": "p1", "exited": False}, {"pty_id": "p2", "exited": False}])
    around, out = _around(fake, Options(restart_daemons=True, end_ptys=True))
    around.before_install()
    assert fake.read_state()["killed"] == ["p1", "p2"]
    stopped = [e["data"] for e in _events(out) if e["data"].get("step") == "stop_daemons"]
    assert stopped[-1] == {
        "step": "stop_daemons",
        "status": "done",
        "detail": "ended 2 PTYs; stopped: rlxd",
    }
    around.installed("2.0.0", None)
    around.finish(())


def test_without_end_ptys_a_live_pty_refuses_at_the_stop_and_none_is_ended(
    fake: Daemons,
) -> None:
    fake.start()
    fake.state(ptys=[{"pty_id": "p1", "exited": False}])
    around, out = _around(fake, Options(restart_daemons=True))
    with pytest.raises(Refused) as refused:
        around.before_install()
    assert refused.value.code == "ptys_live"
    assert "killed" not in fake.read_state()
    assert _steps(_events(out)) == [("check", "failed")]


def test_a_tampered_archive_keeps_the_old_rlxd_and_starts_it_again(fake: Daemons) -> None:
    old = fake.start()
    fake.swap_fails = True
    around, out = _around(fake, Options(restart_daemons=True))

    around.before_install()
    around.installed("2.0.0", None)
    around.finish(())

    again = fake.wait_live("rlxd", not_pid=old.pid)
    assert fake.rlxd.daemon_status() == {"pid": again.pid, "version": "1.0.0"}, "the old rlxd runs"
    assert around.failure is not None
    assert around.failure.code == "rlxd_install_failed"
    assert around.failure.detail == "checksum_mismatch"
    assert "daemon install" in around.failure.message, "what installs rlxd, said (#3107)"
    assert ("rlxd", "failed") in _steps(_events(out))
    assert around.restarted == ["rlxd"]


def test_a_failed_install_starts_the_old_daemons_again_at_once(fake: Daemons) -> None:
    old = fake.start()
    around, out = _around(fake, Options(restart_daemons=True))

    around.before_install()
    around.installed("1.0.0", "uv exited 1")

    again = fake.wait_live("rlxd", not_pid=old.pid)
    assert fake.rlxd.daemon_status() == {"pid": again.pid, "version": "1.0.0"}
    assert fake.swaps == [], "no swap after a failed install"
    assert ("install", "failed") in _steps(_events(out))
    around.finish(None)  # once only
    assert around.restarted == ["rlxd"]


def test_a_stop_that_fails_installs_nothing_and_starts_again_what_it_stopped(
    fake: Daemons,
) -> None:
    """M1 of #626's review (its probe P1): rlxd took the stop and finishes it only after the
    upgrade gave up on it. Nothing is installed; the watch, stopped first, runs again; rlxd
    runs meanwhile, and once it has ended a helper starts it again: some rlxd always runs."""
    old = fake.start()
    watch = fake.start("watch")
    (fake.dir / "stuck-rlxd").write_text("", encoding="utf-8")  # rlxd's stop takes long
    around, out = _around(fake, Options(restart_daemons=True))

    with pytest.raises(Refused) as refused:
        around.before_install()

    assert refused.value.code == "daemon_stop_failed"
    assert [d.pid for d in fake.live("rlxd")] == [old.pid], "it still runs"
    assert fake.wait_live("watch", not_pid=watch.pid).pid != watch.pid
    assert [later.split(" ")[0] for later in around.later] == ["rlxd"]
    steps = _steps(_events(out))
    assert ("stop_daemons", "failed") in steps
    assert not any(step == "install" for step, _ in steps), "the install never started"
    (fake.dir / "stuck-rlxd").unlink()  # rlxd finishes the stop it was asked for
    again = fake.wait_live("rlxd", not_pid=old.pid)
    assert fake.rlxd.daemon_status() == {"pid": again.pid, "version": "1.0.0"}


def test_an_rlxd_that_ends_after_its_command_line_gave_up_counts_as_stopped(
    fake: Daemons,
) -> None:
    """M1: `rlxd stop` gives up after its 1 s here, while rlxd finishes the stop it took; the
    upgrade waits for the lock and the process to go, and goes on."""
    old = fake.start()
    (fake.dir / "stuck-rlxd").write_text("", encoding="utf-8")
    threading.Timer(2.5, lambda: (fake.dir / "stuck-rlxd").unlink(missing_ok=True)).start()
    around, _ = _around(fake, Options(restart_daemons=True))

    around.before_install()

    assert fake.live("rlxd") == [], "stopped, late"
    assert [d.pid for d in around.stopped] == [old.pid]
    around.installed("2.0.0", None)
    around.finish(())
    new = fake.wait_live("rlxd", not_pid=old.pid)
    assert fake.rlxd.daemon_status() == {"pid": new.pid, "version": "2.0.0"}
    assert around.failure is None


def test_without_restart_daemons_nothing_stops_and_rlxds_binaries_are_still_swapped(
    fake: Daemons,
) -> None:
    old = fake.start()
    around, out = _around(fake, Options())
    around.before_install()
    assert [d.pid for d in fake.live("rlxd")] == [old.pid]
    around.installed("2.0.0", None)
    around.finish(())
    assert len(fake.swaps) == 1
    assert around.restarted == []
    assert _steps(_events(out))[:2] == [("check", "done"), ("stop_daemons", "skipped")]


# --- events and the package ------------------------------------------------------------------


def test_each_event_is_a_7_3_envelope_with_a_local_seq() -> None:
    out = io.StringIO()
    events = Events(json_lines=True, out=out)
    events.step("check", "started")
    events.done("1.0.0", "2.0.0", ["rlxd"])
    events.error(Refused("ptys_live", "a PTY is live", "p1"))
    lines = _events(out)
    assert [line["seq"] for line in lines] == [1, 2, 3]
    assert all(line["schema_version"] == 1 and line["topic"] == "upgrade" for line in lines)
    assert all(line["time"].endswith("Z") for line in lines)
    assert lines[1]["data"] == {"from": "1.0.0", "to": "2.0.0", "restarted": ["rlxd"]}
    assert lines[2]["data"] == {"code": "ptys_live", "message": "a PTY is live", "detail": "p1"}


def test_a_wheels_version_is_read_from_its_file_name() -> None:
    wheel = "dist2/aisquare_cli-0.10.1+upgrade-py3-none-any.whl"
    assert upgrade_daemons.wheel_version(wheel) == "0.10.1+upgrade"
    assert upgrade_daemons.wheel_version("aisquare_cli-0.10.1.tar.gz") is None


# --- the command ---------------------------------------------------------------------------


def _receipt() -> str:
    return (
        "[tool]\nrequirements = [\n"
        '    { name = "aisquare-cli", extras = ["serve"] },\n'
        ']\npython = "3.13"\n'
    )


@pytest.fixture
def tool(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A uv tool install of 1.0.0, the running one; PyPI's latest 2.0.0; the installer a
    stand-in that succeeds."""
    prefix = tmp_path / "uv" / "tools" / "aisquare-cli"
    (prefix / "bin").mkdir(parents=True)
    (prefix / install_route.RECEIPT_NAME).write_text(_receipt(), encoding="utf-8")
    facts = Facts(
        prefix=prefix,
        base_prefix=prefix.parent / "base-python",
        executable=prefix / "bin" / "python",
        platform="linux" if sys.platform != "win32" else "win32",
        python_version="3.13",
    )
    monkeypatch.setattr(install_route, "facts", lambda: facts)
    monkeypatch.setattr(install_route, "find_uv", lambda: "/usr/bin/uv")
    monkeypatch.setattr(install_route, "fetch_latest", lambda *a, **k: LatestRelease("2.0.0"))
    monkeypatch.setattr(install_route, "run_installer", lambda argv, **k: 0)
    monkeypatch.setattr(lifecycle, "__version__", "1.0.0")
    monkeypatch.setattr(lifecycle, "refresh_sites", lambda facts: ((), ()))
    return prefix


def test_check_keeps_todays_fields_and_adds_the_three_of_4_2(fake: Daemons, tool: Path) -> None:
    fake.start()
    fake.state(ptys=[{"pty_id": "p1", "pid": 7, "argv": ["sh"], "exited": False}])
    result = CliRunner().invoke(app, ["--json", "upgrade", "--check"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    keys = [
        "current",
        "latest",
        "update_available",
        "live_agents",
        "remote",
        "asqs",
        "launchers",
        "commands_taken",
        "daemons",
        "runs_in_flight",
        "ptys",
    ]
    assert [key for key in keys if key not in payload] == [], "7.8's 11 keys"
    assert [d["kind"] for d in payload["daemons"]] == ["rlxd"]
    assert len(payload["ptys"]) == 1 and payload["runs_in_flight"] == []


@pytest.mark.skipif(sys.platform == "win32", reason="Windows can't replace a running rlx")
def test_json_lines_restarts_rlxd_on_the_new_release_and_ends_upgrade_done(
    fake: Daemons, tool: Path
) -> None:
    old = fake.start()
    result = CliRunner().invoke(
        app, ["upgrade", "--yes", "--restart-daemons", "--json-lines"], catch_exceptions=False
    )
    assert result.exit_code == 0, result.output
    lines = [json.loads(line) for line in result.stdout.splitlines()]
    assert lines[-1]["type"] == "upgrade.done", lines
    assert lines[-1]["data"]["restarted"] == ["rlxd"]
    assert lines[-1]["data"]["from"] == "1.0.0" and lines[-1]["data"]["to"] == "2.0.0"
    new = fake.wait_live("rlxd", not_pid=old.pid)
    assert fake.rlxd.daemon_status() == {"pid": new.pid, "version": "2.0.0"}
    assert [step for step, _ in _steps(lines)][:2] == ["check", "check"]


@pytest.mark.parametrize(
    ("state", "flags", "code"),
    [
        ({"ptys": [{"pty_id": "p1", "exited": False}]}, ["--restart-daemons"], "ptys_live"),
        ({"runs": [{"run_id": "r1", "state": "running"}]}, ["--restart-daemons"], "runs_in_flight"),
        ({"runs": [{"run_id": "r1", "state": "running"}]}, [], "runs_in_flight"),
    ],
)
def test_json_lines_refuses_with_its_code_and_stops_nothing(
    fake: Daemons, tool: Path, state: dict[str, Any], flags: list[str], code: str
) -> None:
    old = fake.start()
    fake.state(**state)
    result = CliRunner().invoke(app, ["upgrade", "--yes", *flags, "--json-lines"])
    assert result.exit_code == 1, result.output
    lines = [json.loads(line) for line in result.stdout.splitlines()]
    assert lines[-1]["type"] == "upgrade.error" and lines[-1]["data"]["code"] == code, lines
    assert [d.pid for d in fake.live("rlxd")] == [old.pid], "nothing stopped"


def _on_windows(tool: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    facts = install_route.facts()
    monkeypatch.setattr(install_route, "facts", lambda: replace(facts, platform="win32"))


def test_on_windows_a_running_daemon_refuses_a_plain_upgrade(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """4.2's Windows Accept line. rlx can't replace itself there (its route refuses), so the
    refusal names the daemons too, with the command to run by hand, and never advises
    --restart-daemons, which could not help (review M6)."""
    fake.start()
    _on_windows(tool, monkeypatch)
    result = CliRunner().invoke(app, ["upgrade", "--yes", "--json-lines"])
    assert result.exit_code == 1, result.output
    last = json.loads(result.stdout.splitlines()[-1])
    assert last["type"] == "upgrade.error" and last["data"]["code"] == "daemon_running"
    assert "uv tool install" in last["data"]["message"]
    assert "--restart-daemons" not in last["data"]["message"]
    restarted = CliRunner().invoke(app, ["upgrade", "--yes", "--restart-daemons", "--json-lines"])
    last = json.loads(restarted.stdout.splitlines()[-1])
    assert last["data"]["code"] == "upgrade_unsupported_route"
    plain = CliRunner().invoke(app, ["--json", "upgrade", "--yes"])
    assert plain.exit_code == 1
    assert json.loads(plain.stdout)["error"] == "daemon_running"


def test_json_lines_needs_yes() -> None:
    result = CliRunner().invoke(app, ["upgrade", "--json-lines"])
    assert result.exit_code == 2


# --- the review of #626 (#2826): its probes and minors ------------------------------------

posix_only = pytest.mark.skipif(
    sys.platform == "win32", reason="Windows can't replace a running rlx: its route refuses"
)


def _lines(result: Any) -> list[dict[str, Any]]:
    return [json.loads(line) for line in result.stdout.splitlines() if line.strip()]


def _upgrade(*flags: str) -> Any:
    return CliRunner().invoke(app, ["upgrade", "--yes", *flags, "--json-lines"])


@posix_only
@pytest.mark.parametrize("json_lines", [True, False])
def test_ctrl_c_in_the_installer_starts_what_was_stopped_again(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch, json_lines: bool
) -> None:
    """M2 (probe P2): a Ctrl-C during uv's run unwinds through the guard."""
    old = fake.start()

    def interrupted(argv: Any, **kwargs: Any) -> int:
        raise KeyboardInterrupt

    monkeypatch.setattr(install_route, "run_installer", interrupted)
    flags = ["--json-lines"] if json_lines else []
    result = CliRunner().invoke(app, ["upgrade", "--yes", "--restart-daemons", *flags])
    assert result.exit_code != 0
    new = fake.wait_live("rlxd", not_pid=old.pid)
    assert fake.rlxd.daemon_status() == {"pid": new.pid, "version": "1.0.0"}
    if json_lines:
        lines = _lines(result)
        assert lines[-1]["type"] == "upgrade.error"
        assert lines[-1]["data"]["code"] == "interrupted"
        steps = _steps(lines)
        assert steps.index(("install", "failed")) < steps.index(("start_daemons", "started"))


@posix_only
def test_sigterm_under_json_lines_starts_what_was_stopped_and_says_interrupted(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """M2: the app's SIGTERM is an exception there, not the end of the process."""
    old = fake.start()
    before = signal.getsignal(signal.SIGTERM)

    def terminated(argv: Any, **kwargs: Any) -> int:
        signal.raise_signal(signal.SIGTERM)
        return 0

    monkeypatch.setattr(install_route, "run_installer", terminated)
    result = _upgrade("--restart-daemons")
    assert result.exit_code == 1, result.output
    assert _lines(result)[-1]["data"]["code"] == "interrupted"
    new = fake.wait_live("rlxd", not_pid=old.pid)
    assert fake.rlxd.daemon_status() == {"pid": new.pid, "version": "1.0.0"}
    assert signal.getsignal(signal.SIGTERM) == before, "SIGTERM's handler put back"


@posix_only
def test_an_unexpected_error_ends_with_upgrade_error_internal_and_rlxd_running(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    old = fake.start()

    def broken(argv: Any, **kwargs: Any) -> int:
        raise RuntimeError("boom")

    monkeypatch.setattr(install_route, "run_installer", broken)
    result = _upgrade("--restart-daemons")
    assert result.exit_code == 1
    last = _lines(result)[-1]
    assert last["type"] == "upgrade.error" and last["data"]["code"] == "internal"
    assert last["data"]["detail"] == "RuntimeError"
    assert fake.wait_live("rlxd", not_pid=old.pid).pid != old.pid


@posix_only
def test_a_pty_opened_while_waiting_for_runs_refuses_and_nothing_stops(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """M3 (probe P3): counted again right before the stop."""
    monkeypatch.setattr(upgrade_daemons, "WAIT_POLL_SECONDS", 0.1)
    old = fake.start()
    fake.state(runs=[{"run_id": "r1", "state": "running"}], ptys=[])
    opened = {"runs": [], "ptys": [{"pty_id": "p-new", "exited": False}]}
    threading.Timer(1.0, lambda: fake.state(**opened)).start()
    result = _upgrade("--restart-daemons", "--wait-runs", "1")
    assert result.exit_code == 1, result.output
    lines = _lines(result)
    assert lines[-1]["data"]["code"] == "ptys_live", lines[-1]
    assert ("check", "failed") in _steps(lines)
    assert [d.pid for d in fake.live("rlxd")] == [old.pid], "nothing stopped"


def test_a_run_started_after_the_count_refuses_at_the_stop(fake: Daemons) -> None:
    """M3: a run that appeared since the count refuses as before, nothing stopped."""
    old = fake.start()
    around, out = _around(fake, Options(restart_daemons=True))
    fake.state(runs=[{"run_id": "r2", "state": "queued"}])
    with pytest.raises(Refused) as refused:
        around.before_install()
    assert refused.value.code == "runs_in_flight" and refused.value.detail == "r2"
    assert [d.pid for d in fake.live("rlxd")] == [old.pid]
    around.start_again()
    assert _steps(_events(out)) == [("check", "failed")], "nothing to start again"


@posix_only
@pytest.mark.parametrize(
    ("flags", "code"),
    [(["--restart-daemons"], "ptys_live"), ([], "runs_in_flight")],
)
def test_runs_and_ptys_that_cant_be_read_refuse_and_rlxd_is_not_stopped(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch, flags: list[str], code: str
) -> None:
    """M4 (probe P5): the census fails closed."""
    old = fake.start()
    real_call = upgrade_daemons.Rlxd.call

    def call(self: Any, method: str, params: Any = None) -> Any:
        if method in ("run.list", "pty.list"):
            raise upgrade_daemons.RlxdError("unreachable", "timed out after 30 s")
        return real_call(self, method, params)

    monkeypatch.setattr(upgrade_daemons.Rlxd, "call", call)
    result = _upgrade(*flags)
    last = _lines(result)[-1]
    assert last["type"] == "upgrade.error" and last["data"]["code"] == code, last
    assert last["data"]["detail"] == "timed out after 30 s"
    assert [d.pid for d in fake.live("rlxd")] == [old.pid]


def test_unknown_runs_and_ptys_refuse_as_some_would() -> None:
    found = _census(rlxd_error="unreachable")
    restart = upgrade_daemons.refusal(found, Options(restart_daemons=True), windows=False)
    assert restart is not None and restart.code == "ptys_live"
    plain = upgrade_daemons.refusal(found, Options(), windows=False)
    assert plain is not None and plain.code == "runs_in_flight"
    ended = Options(restart_daemons=True, end_ptys=True)
    refused = upgrade_daemons.refusal(found, ended, windows=False)
    assert refused is not None and refused.code == "runs_in_flight"


@posix_only
def test_rlxd_started_again_at_another_version_than_swapped_fails_the_upgrade(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """M5 (probe P4): `daemon install` says it swapped, but the rlxd the record runs is
    still 1.0.0: no upgrade.done."""
    fake.start()

    def run_captured(argv: Any, *, timeout: float) -> Captured:
        argv = list(argv)
        if argv[-2:] == ["daemon", "install"]:
            return Captured(0, json.dumps({"version": "2.0.0"}))  # wrote elsewhere
        if argv[-1] == "--version":
            return Captured(0, "aisquare 2.0.0\n")
        return Captured(0, "connected\n")

    monkeypatch.setattr(install_route, "run_captured", run_captured)
    result = _upgrade("--restart-daemons")
    assert result.exit_code == 1
    last = _lines(result)[-1]
    assert last["type"] == "upgrade.error" and last["data"]["code"] == "daemon_start_failed"
    assert "reports 1.0.0, not 2.0.0" in last["data"]["message"]
    assert (fake.rlxd.daemon_status() or {}).get("version") == "1.0.0", "an rlxd runs"


@posix_only
def test_an_up_to_date_upgrade_is_nothing_to_do_whatever_runs(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """M6 (probe P6): the daemons are reckoned with only for an upgrade that runs."""
    monkeypatch.setattr(install_route, "fetch_latest", lambda *a, **k: LatestRelease("1.0.0"))
    fake.start()
    fake.state(runs=[{"run_id": "r1", "state": "running"}])
    plain = CliRunner().invoke(app, ["upgrade", "--yes"])
    assert plain.exit_code == 0, plain.output
    assert "nothing to do" in plain.output
    assert "keeps running the release" not in plain.stderr, "no warning on a no-op"
    lines = _lines(_upgrade("--restart-daemons"))
    assert lines[-1] == {**lines[-1], "type": "upgrade.done"}
    assert lines[-1]["data"] == {"from": "1.0.0", "to": "1.0.0", "restarted": []}


@posix_only
def test_off_a_terminal_without_yes_it_is_a_dry_run_whatever_runs(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """M6: off a terminal, no question and no refusal: today's dry run."""
    from aisquare.cli import install as install_cli

    monkeypatch.setattr(install_cli, "_stdin_is_a_terminal", lambda: False)
    fake.start()
    fake.state(runs=[{"run_id": "r1", "state": "running"}])
    result = CliRunner().invoke(app, ["upgrade", "--wait-runs", "5"])
    assert result.exit_code == 0, result.output
    assert "dry run" in result.output


@pytest.mark.parametrize("minutes", ["nan", "inf", "-1", "1441"])
def test_wait_runs_takes_a_bounded_number_of_minutes(minutes: str) -> None:
    """m6 (probe P7)."""
    result = CliRunner().invoke(app, ["--json", "upgrade", "--check", "--wait-runs", minutes])
    assert result.exit_code == 2, result.output
    with pytest.raises(ValueError):
        Options(wait_runs=float(minutes))


# --- what the upgrade stops and runs (m1, m2, m4) --------------------------------------------


def _record(**values: Any) -> daemons_core.Daemon:
    return replace(_census().daemons[0], **{"restartable": True, **values})


def test_only_rlxs_own_commands_are_stopped_and_run_again(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Round 1 m1 and round 2 M1: a record's argv must be exactly what rlx (or rlxd) writes
    for its kind before it is ever run: no `python -c`, no other module, nothing more."""
    rlxd = tmp_path / "bin" / "rlxd"
    rlxd.parent.mkdir()
    rlxd.write_text("", encoding="utf-8")
    monkeypatch.setattr(upgrade_daemons, "installed_rlxd", lambda: rlxd)
    daemons_core.registry_dir().mkdir(parents=True, exist_ok=True, mode=0o700)
    python = (sys.executable, "-P", "-m", "aisquare")
    good = [
        _record(argv=(str(rlxd), "serve", "--detach")),
        _record(argv=(str(rlxd), "serve", "--detach", "--gate-dev-mode")),
        _record(kind="watch", argv=(*python, "watch", "--detach")),
        _record(kind="watch", argv=(*python, "--no-color", "--profile=work", "watch", "--detach")),
        _record(kind="serve", argv=(*python, "serve", "--port", "8765", "--bind", "127.0.0.1")),
    ]
    for record in good:
        assert upgrade_daemons.restart_refusal(record) is None, record.argv
    other = tmp_path / "rlxd-dev"
    other.write_text("", encoding="utf-8")
    not_rlxs = "its command is not the one rlx writes for a"
    cases = [
        (_record(argv=good[0].argv, restartable=False), "not restartable"),
        (_record(kind="later_kind", argv=good[0].argv), "rlx doesn't restart a later_kind"),
        (_record(kind="brain", argv=good[0].argv), "rlx doesn't restart a brain"),
        (_record(argv=("rlxd", "serve", "--detach")), "its program rlxd is not a file"),
        (_record(argv=(str(rlxd), "call", "stop")), "its command is not rlx's"),
        (
            _record(argv=(str(rlxd), "serve", "--detach", "--log", "x")),
            "its command is not the one rlxd writes",
        ),
        (_record(argv=(str(other), "serve", "--detach")), f"it runs {other}, not {rlxd}"),
        (_record(kind="watch", argv=(sys.executable, "-c", "print(1)")), f"{not_rlxs} watch"),
        (_record(kind="watch", argv=(sys.executable, "-m", "aisquare", "watch")), "its command"),
        (_record(kind="watch", argv=(*python, "watch")), f"{not_rlxs} watch"),
        (_record(kind="watch", argv=(*python, "--json", "watch", "--detach")), f"{not_rlxs} watch"),
        (_record(kind="watch", argv=(*python, "watch", "--detach", "x")), f"{not_rlxs} watch"),
        (_record(kind="serve", argv=(*python, "serve", "--stdio")), f"{not_rlxs} serve"),
        (
            _record(kind="serve", argv=(*python, "serve", "--port", "0", "--bind", "h")),
            f"{not_rlxs} serve",
        ),
        (
            _record(kind="serve", argv=(*python, "serve", "--port", "80", "--bind", "-x")),
            f"{not_rlxs} serve",
        ),
    ]
    for record, why in cases:
        said = upgrade_daemons.restart_refusal(record)
        assert said is not None and said.startswith(why), (record.argv, said)


@posix_only
def test_a_registry_open_to_others_names_nothing_to_run(tmp_path: Path) -> None:
    folder = daemons_core.registry_dir()
    folder.mkdir(parents=True, exist_ok=True)
    folder.chmod(0o755)
    python = _record(
        kind="watch", argv=(sys.executable, "-P", "-m", "aisquare", "watch", "--detach")
    )
    said = upgrade_daemons.restart_refusal(python)
    assert said is not None and "open to others" in said
    folder.chmod(0o700)
    assert upgrade_daemons.restart_refusal(python) is None


def test_a_record_naming_another_program_refuses_and_nothing_stops(
    fake: Daemons, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Round 2 M1: a restartable record rlx did not write refuses (daemon_unrecognized)
    before anything stops."""
    old = fake.start()
    monkeypatch.setattr(
        upgrade_daemons, "rlxd_command", lambda: ("/nowhere/rlxd", "serve", "--detach")
    )
    around, out = _around(fake, Options(restart_daemons=True))
    with pytest.raises(Refused) as refused:
        around.before_install()
    assert refused.value.code == "daemon_unrecognized"
    assert refused.value.detail is not None and refused.value.detail.startswith("rlxd (")
    assert [d.pid for d in fake.live("rlxd")] == [old.pid]
    assert around.asked == []
    assert _steps(_events(out)) == [("check", "failed")]


@posix_only
def test_check_runs_no_program_a_record_names(tool: Path, tmp_path: Path) -> None:
    """m1: rlxd is asked through the one installed beside rlx (none here), never a record's
    argv[0]."""
    ran = tmp_path / "ran"
    program = tmp_path / "rlxd"
    program.write_text(f"#!/bin/sh\ntouch {ran}\n", encoding="utf-8")
    program.chmod(0o755)
    registration = daemons_core.register(
        "rlxd", argv=[str(program), "serve", "--detach"], restartable=True
    )
    try:
        result = CliRunner().invoke(app, ["--json", "upgrade", "--check"])
    finally:
        registration.deregister()
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["rlxd_error"] == (
        "rlxd runs, but no rlxd is installed beside rlx to ask"
    )
    assert not ran.exists()


def test_a_held_record_that_cant_be_read_is_counted(monkeypatch: pytest.MonkeyPatch) -> None:
    """m2: counted as a daemon of this host that can't be restarted."""
    unread = registry.Unreadable(
        id="abcdefabcdef",
        path=Path("abcdefabcdef.json"),
        written_at=None,
        held=True,
        said={"kind": "watch", "pid": 77},
    )
    free = replace(unread, id="0000000000ff", held=False)
    roll: registry.Roll[daemons_core.Daemon] = registry.Roll(
        live=[], unreadable=[unread, free], pruned=[]
    )
    monkeypatch.setattr(daemons_core, "roll", lambda **k: roll)
    found = upgrade_daemons.census()
    assert found.unreadable == (unread,)
    shown = found.as_json()["daemons"]
    assert shown == [
        {
            "id": "abcdefabcdef",
            "kind": "watch",
            "pid": 77,
            "host": None,
            "version": None,
            "restartable": False,
            "argv": [],
            "started_at": None,
            "readable": False,
        }
    ]
    refused = upgrade_daemons.refusal(found, Options(), windows=True)
    assert refused is not None and refused.code == "daemon_running"
    assert "not readable" in (upgrade_daemons.mixed_releases(found, Options()) or "")


def test_a_daemon_started_again_is_its_own_new_record(fake: Daemons) -> None:
    """m4: another live daemon of its kind does not stand in for one that never came up."""
    other = fake.start("watch")
    gone = replace(other, id="0123456789ab", pid=other.pid + 100000)
    said = upgrade_daemons._wait_started(gone, upgrade_daemons._live_ids(), None, None, 1.0)
    assert said == "did not come up in 1 s"
    before = upgrade_daemons._live_ids()
    fake.start("watch")
    assert upgrade_daemons._wait_started(gone, before, None, None, 10.0) is None


# --- the package (m3, n2) ----------------------------------------------------------------------


def _wheel(tmp_path: Path, version: str = "2.0.0") -> Path:
    wheel = tmp_path / f"aisquare_cli-{version}-py3-none-any.whl"
    wheel.write_bytes(b"")
    return wheel


def test_the_package_is_a_local_wheel_and_nothing_else(
    tool: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    plan = lifecycle.upgrade_plan(None)
    for value in (
        "https://example.com/aisquare_cli-2.0.0-py3-none-any.whl",
        "file:///tmp/aisquare_cli-2.0.0-py3-none-any.whl",
        str(tmp_path / "missing-2.0.0-py3-none-any.whl"),
        str(tmp_path),
    ):
        monkeypatch.setenv(upgrade_daemons.PACKAGE_ENV, value)
        with pytest.raises(Refused) as refused:
            upgrade_daemons.with_package(plan, lifecycle, pinned=False)
        assert refused.value.code == "invalid_version", value
    monkeypatch.setenv(upgrade_daemons.PACKAGE_ENV, str(_wheel(tmp_path)))
    with pytest.raises(Refused, match="give one of them"):
        upgrade_daemons.with_package(plan, lifecycle, pinned=True)
    planned, wheel = upgrade_daemons.with_package(plan, lifecycle, pinned=False)
    assert wheel == _wheel(tmp_path).resolve()
    assert planned.target == "2.0.0" and planned.reason is None
    assert planned.argv[-1] == f"aisquare-cli[serve] @ {wheel.as_uri()}"
    # n2: the reinstall's own prefix, held together with install_route's.
    spec = install_route._uv_spec(plan.route, None, plan.current)
    assert (
        list(planned.argv[:-1])
        == install_route._uv_install_argv(plan.route, None, plan.current)[: -len(spec)]
    )
    assert upgrade_daemons.with_package(plan, lifecycle, pinned=False)[1] is not None
    monkeypatch.delenv(upgrade_daemons.PACKAGE_ENV)
    assert upgrade_daemons.with_package(plan, lifecycle, pinned=True) == (plan, None)


def test_the_package_keeps_the_routes_own_refusals(
    tool: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(install_route, "find_uv", lambda: None)
    monkeypatch.setenv(upgrade_daemons.PACKAGE_ENV, str(_wheel(tmp_path)))
    planned, _ = upgrade_daemons.with_package(lifecycle.upgrade_plan(None), lifecycle, pinned=False)
    assert planned.reason == "uv is not on PATH"
    result = CliRunner().invoke(app, ["--json", "upgrade", "--yes"])
    assert json.loads(result.stdout)["error"] == "upgrade_unsupported_route"


def test_the_package_is_said(tool: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    wheel = _wheel(tmp_path)
    monkeypatch.setenv(upgrade_daemons.PACKAGE_ENV, str(wheel))
    result = CliRunner().invoke(app, ["--json", "upgrade", "--check"])
    assert result.exit_code == 0, result.output
    assert f"installing from {wheel.resolve()} ({upgrade_daemons.PACKAGE_ENV})" in result.stderr
    assert json.loads(result.stdout)["current"] == "1.0.0"


# --- network, the swap, the order of steps (m5, m7, n1) ---------------------------------------


@posix_only
def test_a_pypi_that_cant_be_asked_refuses_before_anything_stops(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """m5: 7.8's `network`, in the check step."""
    offline = LatestRelease(None, error="no network")
    monkeypatch.setattr(install_route, "fetch_latest", lambda *a, **k: offline)
    old = fake.start()
    lines = _lines(_upgrade("--restart-daemons"))
    assert lines[-1]["data"]["code"] == "network"
    assert _steps(lines) == [("check", "started"), ("check", "failed")]
    interactive = CliRunner().invoke(app, ["--json", "upgrade", "--yes", "--restart-daemons"])
    assert json.loads(interactive.stdout)["error"] == "network"
    assert [d.pid for d in fake.live("rlxd")] == [old.pid]


@posix_only
def test_a_failed_rlxd_swap_without_restart_daemons_warns_and_the_upgrade_stands(
    fake: Daemons, tool: Path
) -> None:
    """m7: rlx is upgraded; rlxd is not, which is said, and the run ends upgrade.done."""
    old = fake.start()
    fake.swap_fails = True
    result = _upgrade()
    assert result.exit_code == 0, result.output
    lines = _lines(result)
    assert ("rlxd", "failed") in _steps(lines)
    assert lines[-1]["type"] == "upgrade.done" and lines[-1]["data"]["to"] == "2.0.0"
    assert "rlx is upgraded to 2.0.0, but rlxd is not (checksum_mismatch)" in result.stderr
    assert [d.pid for d in fake.live("rlxd")] == [old.pid]


@posix_only
def test_json_lines_says_each_step_once_in_order(fake: Daemons, tool: Path) -> None:
    """n1."""
    fake.start()
    lines = _lines(_upgrade("--restart-daemons"))
    assert _steps(lines) == [
        ("check", "started"),
        ("check", "done"),
        ("stop_daemons", "started"),
        ("stop_daemons", "done"),
        ("install", "started"),
        ("install", "done"),
        ("rlxd", "started"),
        ("rlxd", "done"),
        ("refresh_hooks", "done"),
        ("start_daemons", "started"),
        ("start_daemons", "done"),
    ]
    assert lines[-1]["type"] == "upgrade.done"


@posix_only
def test_after_a_failed_stop_or_install_no_step_is_said_twice(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """n1: no `install failed` for an install that never started, nor a second one."""
    old = fake.start()
    (fake.dir / "stuck-rlxd").write_text("", encoding="utf-8")
    stopped = _lines(_upgrade("--restart-daemons"))
    assert stopped[-1]["data"]["code"] == "daemon_stop_failed"
    assert not any(step == "install" for step, _ in _steps(stopped))
    (fake.dir / "stuck-rlxd").unlink()
    again = fake.wait_live("rlxd", not_pid=old.pid)  # started again by the helper
    deadline = time.monotonic() + 20
    while (fake.rlxd.daemon_status() or {}).get("pid") != again.pid:
        assert time.monotonic() < deadline, "the rlxd started again never answered"
        time.sleep(0.05)
    monkeypatch.setattr(install_route, "run_installer", lambda argv, **k: 1)
    failed = _lines(_upgrade("--restart-daemons"))
    assert failed[-1]["data"]["code"] == "upgrade_failed"
    assert _steps(failed).count(("install", "failed")) == 1


# --- the restart-when-gone helper (#2829: one at a time, logged, shown) ----------------------


def _given_up_on(fake: Daemons) -> tuple[daemons_core.Daemon, Around]:
    """rlxd asked to stop and still stopping when the upgrade gives up on it (the real-stop
    model: it finishes once ``stuck-rlxd`` is gone): the helper pending."""
    old = fake.start()
    (fake.dir / "stuck-rlxd").write_text("", encoding="utf-8")
    around, _ = _around(fake, Options(restart_daemons=True))
    with pytest.raises(Refused):
        around.before_install()
    return old, around


def _settled(fake: Daemons, seconds: float = 3.0) -> list[daemons_core.Daemon]:
    time.sleep(seconds)  # the helper's poll, its grace and a start, had there been one
    return fake.live("rlxd")


def test_the_helper_is_shown_logs_each_step_and_starts_rlxd_once(fake: Daemons, tool: Path) -> None:
    old, around = _given_up_on(fake)
    assert around.later and around.later[0].startswith("rlxd (log: ")
    check = CliRunner().invoke(app, ["--json", "upgrade", "--check"])
    pending = json.loads(check.stdout)["restarts_pending"]
    assert [(p["id"], p["kind"], p["pid"]) for p in pending] == [(old.id, "rlxd", old.pid)]
    log = Path(pending[0]["log"])
    upgrade_daemons.restart_when_gone(old)  # a second helper, for the same record
    (fake.dir / "stuck-rlxd").unlink()  # rlxd finishes the stop it took
    new = fake.wait_live("rlxd", not_pid=old.pid)
    assert [d.pid for d in _settled(fake)] == [new.pid], "started once"
    said = log.read_text(encoding="utf-8")
    assert f"waiting for rlxd (record {old.id}, pid {old.pid}) to end" in said
    assert f"another helper waits for rlxd (record {old.id}): nothing to do" in said
    assert said.count("rlxd ended; started again: ") == 1
    assert upgrade_daemons.pending_restarts() == ()


def test_an_rlxd_started_by_hand_while_the_helper_waits_is_the_one_kept(fake: Daemons) -> None:
    old, _ = _given_up_on(fake)
    before = upgrade_daemons._live_ids()
    upgrade_daemons.spawn_daemon(old)  # a restart by hand, while the old one still stops
    deadline = time.monotonic() + 20
    while not (upgrade_daemons._live_ids() - before):
        assert time.monotonic() < deadline, "the rlxd started by hand never registered"
        time.sleep(0.05)
    by_hand = next(d for d in fake.live("rlxd") if d.pid != old.pid)
    fake.pids.append(by_hand.pid)
    (fake.dir / "stuck-rlxd").unlink()
    assert [d.pid for d in _settled(fake)] == [by_hand.pid], "no second rlxd"
    said = (upgrade_daemons.restarts_dir() / upgrade_daemons.RESTART_LOG).read_text("utf-8")
    assert f"rlxd ended; another runs already (pid {by_hand.pid}): kept" in said


# --- tester-upgrade's findings on 3fdf7768 (#2831) --------------------------------------------


@posix_only
def test_a_real_sigint_during_the_install_starts_rlxd_again(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """F4: Ctrl-C (SIGINT) while uv runs, under --restart-daemons --json-lines."""
    old = fake.start()

    def interrupted(argv: Any, **kwargs: Any) -> int:
        signal.raise_signal(signal.SIGINT)
        return 0

    monkeypatch.setattr(install_route, "run_installer", interrupted)
    result = _upgrade("--restart-daemons")
    assert result.exit_code == 1, result.output
    assert _lines(result)[-1]["data"]["code"] == "interrupted"
    new = fake.wait_live("rlxd", not_pid=old.pid)
    assert fake.rlxd.daemon_status() == {"pid": new.pid, "version": "1.0.0"}


@posix_only
@pytest.mark.parametrize("how", ["touch", "python -c"])
def test_a_live_record_naming_another_program_is_neither_signalled_nor_run(
    fake: Daemons, tool: Path, tmp_path: Path, how: str
) -> None:
    """H1a (tester, round 1) and R1 (review, round 2): a live, restartable record whose argv
    is `/usr/bin/touch <marker>`, or this very interpreter with `-c <code>`."""
    marker = tmp_path / "marker"
    code = f"open({str(marker)!r}, 'w').write('ran')"
    argv = ["/usr/bin/touch", str(marker)] if how == "touch" else [sys.executable, "-c", code]
    (fake.dir / "argv-watch").write_text(json.dumps(argv), "utf-8")
    old = fake.start()
    planted = fake.start("watch")
    result = _upgrade("--restart-daemons")
    assert result.exit_code == 1, result.output
    last = _lines(result)[-1]
    assert last["type"] == "upgrade.error" and last["data"]["code"] == "daemon_unrecognized"
    assert "watch" in last["data"]["detail"], last
    time.sleep(1)
    assert [d.pid for d in fake.live("watch")] == [planted.pid], "never signalled"
    assert [d.pid for d in fake.live("rlxd")] == [old.pid], "nothing stopped"
    assert not marker.exists(), "never run"


@posix_only
def test_an_up_to_date_pinned_upgrade_is_nothing_to_do_whatever_runs(
    fake: Daemons, tool: Path
) -> None:
    """M6, live: `rlx upgrade --yes --version <current>` with a run in flight."""
    fake.start()
    fake.state(runs=[{"run_id": "r1", "state": "running"}])
    result = CliRunner().invoke(app, ["upgrade", "--yes", "--version", "1.0.0"])
    assert result.exit_code == 0, result.output
    assert "already the version asked for" in result.output
    lines = _lines(_upgrade("--version", "1.0.0"))
    assert lines[-1]["type"] == "upgrade.done"


def test_queued_runs_are_in_flight_too(fake: Daemons) -> None:
    """M13."""
    fake.start()
    fake.state(runs=[{"run_id": "r1", "kind": "task", "state": "queued", "job_id": "j1"}])
    found = upgrade_daemons.census(fake.rlxd)
    assert [run["run_id"] for run in found.runs] == ["r1"]
    refused = upgrade_daemons.refusal(found, Options(), windows=False)
    assert refused is not None and refused.code == "runs_in_flight"


@posix_only
def test_a_person_running_restart_daemons_gets_rlxd_back_on_the_new_release(
    fake: Daemons, tool: Path
) -> None:
    """M18: the human path, not only --json-lines."""
    old = fake.start()
    result = CliRunner().invoke(app, ["upgrade", "--yes", "--restart-daemons"])
    assert result.exit_code == 0, result.output
    new = fake.wait_live("rlxd", not_pid=old.pid)
    assert fake.rlxd.daemon_status() == {"pid": new.pid, "version": "2.0.0"}
    assert "· start_daemons: started: rlxd" in result.stderr
    assert "· stop_daemons: stopped: rlxd" in result.stderr


def test_the_restart_waits_until_rlxd_answers(fake: Daemons) -> None:
    """M12 (a mutant tester-upgrade's tests let live): rlxd registers before it answers, and
    the restart waits for the answer, not only the record."""
    fake.start()
    around, _ = _around(fake, Options(restart_daemons=True))
    around.before_install()
    (fake.dir / "mute-rlxd").write_text("2", encoding="utf-8")
    around.installed("2.0.0", None)
    around.finish(())
    assert around.restarted == ["rlxd"] and around.failure is None
    status = fake.rlxd.daemon_status()
    assert status is not None and status["version"] == "2.0.0", "answering when restarted"


# --- round 2 of #626's review (#3038) ----------------------------------------------------------


@posix_only
def test_what_starts_while_signals_are_held_off_takes_sigterm_as_by_default() -> None:
    """m1: held off by a handler, not SIG_IGN, which a daemon or helper would inherit."""
    probe = (
        "import signal, sys; "
        "sys.stdout.write(str(signal.getsignal(signal.SIGTERM) == signal.SIG_DFL))"
    )
    with upgrade_daemons._signals_held_off():
        held = signal.getsignal(signal.SIGTERM)
        child = subprocess.run(
            [sys.executable, "-c", probe], capture_output=True, text=True, check=True
        )
    assert held not in (signal.SIG_IGN, signal.SIG_DFL)
    assert child.stdout == "True", "SIGTERM's default, not ignored"


@posix_only
def test_a_helper_the_guard_starts_ends_on_sigterm(
    fake: Daemons, monkeypatch: pytest.MonkeyPatch
) -> None:
    """R2: Ctrl-C while the upgrade waits for a stuck rlxd; the helper the guard starts is
    an ordinary process, which SIGTERM ends."""
    fake.start()
    (fake.dir / "stuck-rlxd").write_text("", encoding="utf-8")
    around, _ = _around(fake, Options(restart_daemons=True))
    real, calls = upgrade_daemons._wait_gone, {"n": 0}

    def ctrl_c_once(daemon: Any, timeout: float) -> bool:
        calls["n"] += 1
        if calls["n"] == 1:
            raise KeyboardInterrupt
        return real(daemon, timeout)

    monkeypatch.setattr(upgrade_daemons, "_wait_gone", ctrl_c_once)
    with pytest.raises(KeyboardInterrupt), upgrade_daemons.guard_daemons(around, sigterm=False):
        around.before_install()
    helper = int(upgrade_daemons.pending_restarts()[0]["helper_pid"])
    os.kill(helper, signal.SIGTERM)
    ended = False
    if sys.platform != "win32":  # waitpid's WNOHANG and SIGKILL are POSIX's, as the test is
        deadline = time.monotonic() + 10
        while not ended and time.monotonic() < deadline:
            with contextlib.suppress(ChildProcessError):
                ended = os.waitpid(helper, os.WNOHANG) != (0, 0)
            time.sleep(0.1)
        if not ended:
            os.kill(helper, signal.SIGKILL)
    assert ended, "the helper ignored SIGTERM"
    assert upgrade_daemons.pending_restarts() == (), "its claim went with it"


@posix_only
def test_a_pty_opened_while_another_daemon_stops_refuses_before_rlxd_stops(
    fake: Daemons, tool: Path
) -> None:
    """m2 (R3): counted again right before rlxd's own stop."""
    old = fake.start()
    watch = fake.start("watch")
    (fake.dir / "stuck-watch").write_text("", encoding="utf-8")

    def later() -> None:
        time.sleep(1.0)
        fake.state(ptys=[{"pty_id": "p-late", "exited": False}])
        time.sleep(0.5)
        (fake.dir / "stuck-watch").unlink()

    threading.Thread(target=later, daemon=True).start()
    lines = _lines(_upgrade("--restart-daemons"))
    assert lines[-1]["type"] == "upgrade.error" and lines[-1]["data"]["code"] == "ptys_live"
    assert ("stop_daemons", "failed") in _steps(lines)
    assert [d.pid for d in fake.live("rlxd")] == [old.pid], "rlxd never stopped"
    assert fake.wait_live("watch", not_pid=watch.pid).pid != watch.pid, "the watch is back"


def test_end_ptys_without_restart_daemons_is_a_usage_error() -> None:
    """n2."""
    result = CliRunner().invoke(app, ["upgrade", "--check", "--end-ptys"])
    assert result.exit_code == 2, result.output


@posix_only
def test_an_rlxd_not_beside_rlx_is_told_to_stop_not_to_wait(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """n1 (R5): waiting can't help when no rlxd beside rlx can say what runs."""
    fake.start()
    monkeypatch.setattr(upgrade_daemons, "find_rlxd", lambda **_: None)
    began = time.monotonic()
    for flags in ([], ["--wait-runs", "1"]):
        last = _lines(_upgrade(*flags))[-1]["data"]
        assert last["code"] == "runs_in_flight", last
        assert "stop the rlxd that runs" in last["message"], last
        assert "--wait-runs" not in last["message"], last
    assert time.monotonic() - began < 30, "refused at once, not after the wait"


def test_a_claim_no_helper_holds_is_not_pending(tmp_path: Path) -> None:
    """n4: a helper is pending while it holds its claim's lock, not while some process has
    its old pid."""
    folder = upgrade_daemons.restarts_dir()
    folder.mkdir(parents=True, exist_ok=True)
    said = {"id": "0123456789ab", "kind": "rlxd", "pid": 1, "helper_pid": os.getpid()}
    (folder / "0123456789ab.json").write_text(json.dumps(said), encoding="utf-8")
    lock = folder / "0123456789ab.lock"
    assert upgrade_daemons.pending_restarts() == (), "no lock: no helper"
    handle = locking.open_lock_file(lock)
    try:
        assert upgrade_daemons.pending_restarts() == (), "a lock nobody holds"
        held = _hold_elsewhere(lock)
        try:
            assert [p["id"] for p in upgrade_daemons.pending_restarts()] == ["0123456789ab"]
        finally:
            held.kill()
            held.wait()
    finally:
        os.close(handle)


def _hold_elsewhere(lock: Path) -> subprocess.Popen[str]:
    """Another process holding ``lock`` until it is killed, once it says it does."""
    code = (
        "import sys, time; from aisquare.core import locking; "
        f"fd = locking.open_lock_file(__import__('pathlib').Path({str(lock)!r})); "
        "locking.acquire(fd, wait_s=5); print('held', flush=True); time.sleep(60)"
    )
    holder = subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.PIPE, text=True)
    assert holder.stdout is not None and holder.stdout.readline().strip() == "held"
    return holder


def test_check_asks_rlxd_with_a_short_timeout(tool: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """n5: a frozen rlxd makes the check's runs and PTYs unknown in seconds."""
    asked: dict[str, Any] = {}

    def find(**given: Any) -> None:
        asked.update(given)

    monkeypatch.setattr(upgrade_daemons, "find_rlxd", find)
    result = CliRunner().invoke(app, ["--json", "upgrade", "--check"])
    assert result.exit_code == 0, result.output
    assert asked == {"timeout": upgrade_daemons.CHECK_CALL_TIMEOUT_SECONDS}
    assert 0 < asked["timeout"] <= 3, "the check waits on a hung rlxd 3 s at most (#3486 M4)"


# --- tester-upgrade's round 2 (#3107) -------------------------------------------------------


@posix_only
def test_a_watch_started_again_after_a_ctrl_c_stops_on_sigterm(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """(B): the guard starts the watch again after a Ctrl-C during uv. It must take SIGTERM by
    default (the fake, like rlx's watch, keeps a SIGTERM ignored at its start ignored), or
    the next upgrade's stop of it fails, 31 s later."""
    fake.start()
    watch = fake.start("watch")

    def interrupted(argv: Any, **kwargs: Any) -> int:
        raise KeyboardInterrupt

    monkeypatch.setattr(install_route, "run_installer", interrupted)
    assert _lines(_upgrade("--restart-daemons"))[-1]["data"]["code"] == "interrupted"
    again = fake.wait_live("watch", not_pid=watch.pid)
    os.kill(again.pid, signal.SIGTERM)
    deadline = time.monotonic() + 10
    while again.id in upgrade_daemons._live_ids():
        assert time.monotonic() < deadline, "the watch started again ignores SIGTERM"
        time.sleep(0.1)


@posix_only
def test_check_flags_an_rlxd_of_another_version_and_says_what_installs_it(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """After a swap that failed rlx is new and rlxd is not: the check says so (#3107)."""
    fake.start()
    same = json.loads(CliRunner().invoke(app, ["--json", "upgrade", "--check"]).stdout)
    assert same["rlxd_version"] == "1.0.0" and same["rlxd_version_mismatch"] is False
    fake.version("0.9.0")
    fake.state(rlxd={**fake.read_state()["rlxd"], "version": "0.9.0"})
    behind = json.loads(CliRunner().invoke(app, ["--json", "upgrade", "--check"]).stdout)
    assert behind["rlxd_version"] == "0.9.0" and behind["rlxd_version_mismatch"] is True
    said = CliRunner().invoke(app, ["upgrade", "--check"]).output
    assert "rlxd runs 0.9.0, not rlx's 1.0.0: install it with" in said
    assert "daemon install" in said


@posix_only
def test_an_upgrade_with_nothing_to_install_says_rlxd_lags(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The re-run after rlxd_install_failed: nothing newer for rlx, and rlxd behind (#3107)."""
    monkeypatch.setattr(install_route, "fetch_latest", lambda *a, **k: LatestRelease("1.0.0"))
    fake.start()
    fake.state(rlxd={**fake.read_state()["rlxd"], "version": "0.9.0"})
    plain = CliRunner().invoke(app, ["upgrade", "--yes"])
    assert plain.exit_code == 0, plain.output
    assert "rlxd runs 0.9.0, not rlx's 1.0.0" in plain.stderr
    lines = _lines(_upgrade())
    assert lines[-1]["type"] == "upgrade.done"
    checked = [e["data"] for e in lines if e["data"].get("step") == "check"]
    assert "rlxd runs 0.9.0" in checked[-1]["detail"], checked


# --- round 3 of #626's review (#3471): the same program, not the same file ---------------------


@posix_only
def test_a_planted_venv_on_the_same_interpreter_is_not_rlxs_watch(
    fake: Daemons, tool: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """S1: a watch record of rlx's shape whose argv[0] is another venv's `bin/python`, a link
    to the interpreter rlx runs on. Python takes its venv from the path it was started by,
    so the planted `aisquare` would run: refused, and never run."""
    monkeypatch.setattr(upgrade_daemons, "python_command", REAL_PYTHON_COMMAND)
    base = Path(os.path.realpath(sys.executable))
    venv = tmp_path / "planted-venv"
    (venv / "bin").mkdir(parents=True)
    (venv / "bin" / "python").symlink_to(base)
    (venv / "pyvenv.cfg").write_text(
        f"home = {base.parent}\ninclude-system-site-packages = false\n", encoding="utf-8"
    )
    version = f"python{sys.version_info.major}.{sys.version_info.minor}"
    package = venv / "lib" / version / "site-packages" / "aisquare"
    package.mkdir(parents=True)
    marker = tmp_path / "planted-aisquare-ran"
    (package / "__init__.py").write_text("", encoding="utf-8")
    (package / "__main__.py").write_text(
        f"open({str(marker)!r}, 'w').write('ran')\n", encoding="utf-8"
    )
    argv = [str(venv / "bin" / "python"), "-P", "-m", "aisquare", "watch", "--detach"]
    (fake.dir / "argv-watch").write_text(json.dumps(argv), "utf-8")
    fake.start()
    watch = fake.start("watch")
    said = upgrade_daemons.restart_refusal(watch)
    assert said == f"it runs {argv[0]}, not {sys.executable}", said
    last = _lines(_upgrade("--restart-daemons"))[-1]
    assert last["data"]["code"] == "daemon_unrecognized", last
    time.sleep(1)
    assert not marker.exists(), "the planted aisquare ran"
    assert [d.pid for d in fake.live("watch")] == [watch.pid], "never signalled"


@posix_only
def test_an_rlxd_linked_from_another_folder_is_not_rlxds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """S2: rlxd runs the `rlx` and `rlxd-hook` beside the path it was started by (macOS's
    current_exe is the link's path), so a link to the rlxd beside rlx from another folder
    is not rlxd's own. The folder beside rlx by another name for it (a linked folder) is."""
    monkeypatch.setattr(upgrade_daemons, "rlxd_command", REAL_RLXD_COMMAND)
    real = tmp_path / "uv-bin" / "rlxd"
    real.parent.mkdir()
    real.write_text("#!/bin/sh\n", encoding="utf-8")
    real.chmod(0o755)
    monkeypatch.setattr(upgrade_daemons, "installed_rlxd", lambda: real)
    daemons_core.registry_dir().mkdir(parents=True, exist_ok=True, mode=0o700)
    planted = tmp_path / "planted" / "rlxd"
    planted.parent.mkdir()
    planted.symlink_to(real)
    said = upgrade_daemons.restart_refusal(_record(argv=(str(planted), "serve", "--detach")))
    assert said == f"it runs {planted}, not {real}", said
    hard = tmp_path / "hard" / "rlxd"
    hard.parent.mkdir()
    os.link(real, hard)  # the same file, by a hard link in another folder (#3486)
    said = upgrade_daemons.restart_refusal(_record(argv=(str(hard), "serve", "--detach")))
    assert said == f"it runs {hard}, not {real}", said
    alias = tmp_path / "bin-alias"
    alias.symlink_to(real.parent, target_is_directory=True)
    beside = _record(argv=(str(alias / "rlxd"), "serve", "--detach"))
    assert upgrade_daemons.restart_refusal(beside) is None, "the same folder, another name"


@posix_only
def test_a_checkouts_venv_is_not_this_installs_interpreter(tmp_path: Path) -> None:
    """S1 without an attacker: a dev checkout's `.venv/bin/python` shares uv's base
    interpreter with the tool's, and is still not the interpreter rlx writes."""
    daemons_core.registry_dir().mkdir(parents=True, exist_ok=True, mode=0o700)
    checkout = tmp_path / "checkout" / ".venv" / "bin"
    checkout.mkdir(parents=True)
    python = checkout / "python"
    python.symlink_to(os.path.realpath(sys.executable))  # as uv makes a venv's
    argv = (str(python), "-P", "-m", "aisquare", "watch", "--detach")
    said = upgrade_daemons.restart_refusal(_record(kind="watch", argv=argv))
    assert said == f"it runs {python}, not {sys.executable}", said


# --- tester-upgrade's round 3 (#3486) -------------------------------------------------------


def test_a_held_unreadable_record_refuses_a_restart(monkeypatch: pytest.MonkeyPatch) -> None:
    """M14: a running daemon whose record can't be read can be neither stopped nor started
    again: --restart-daemons refuses, by name, before anything stops."""
    unread = registry.Unreadable(
        id="abcdefabcdef",
        path=Path("abcdefabcdef.json"),
        written_at=None,
        held=True,
        said={"kind": "watch"},
    )
    roll: registry.Roll[daemons_core.Daemon] = registry.Roll(
        live=[], unreadable=[unread], pruned=[]
    )
    monkeypatch.setattr(daemons_core, "roll", lambda **k: roll)
    found = upgrade_daemons.census()
    refused = upgrade_daemons.refusal(found, Options(restart_daemons=True), windows=False)
    assert refused is not None and refused.code == "daemon_unrecognized"
    assert refused.detail == "watch (record abcdefabcdef): not readable"
    assert upgrade_daemons.refusal(found, Options(), windows=False) is None, "warned of only"


@pytest.mark.parametrize("kind", ["rlxd", "watch", "serve"])
def test_a_record_with_no_command_is_refused_by_name(kind: str) -> None:
    """L2: the format says argv is never empty; a record whose argv is gets a named refusal,
    not an IndexError."""
    said = upgrade_daemons.restart_refusal(_record(kind=kind, argv=()))
    assert said == "its record names no command"
