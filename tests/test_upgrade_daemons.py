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
import sys
import threading
import time
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import daemons as daemons_core
from aisquare.core import registry
from aisquare.services import install_route, lifecycle, upgrade_daemons
from aisquare.services.install_route import Captured, Facts, LatestRelease
from aisquare.services.upgrade_daemons import Around, Events, Options, Refused
from tests.installer_seams import no_real_installer  # noqa: F401 — autouse, applied by import

FAKE = Path(__file__).with_name("fake_rlxd.py")


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
        argv = [sys.executable, str(FAKE), "serve" if kind == "rlxd" else "watch"]
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
    monkeypatch.setenv("FAKE_RLXD_STOP_WAIT", "2")
    monkeypatch.setattr(upgrade_daemons, "find_rlxd", lambda live: world.rlxd)
    monkeypatch.setattr(upgrade_daemons, "installed_rlxd", lambda: folder / "rlxd")
    monkeypatch.setattr(upgrade_daemons, "STOP_TIMEOUT_SECONDS", 15.0)

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
    assert set(shown) == {"daemons", "runs_in_flight", "ptys", "rlxd_error"}
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
        restartable=True,
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
    events = Events(json_lines=True, out=io.StringIO())
    found = upgrade_daemons.census(fake.rlxd)

    with pytest.raises(Refused) as refused:
        upgrade_daemons.wait_for_runs(found, Options(wait_runs=1 / 60), fake.rlxd, events)
    assert refused.value.code == "runs_in_flight"

    threading.Timer(1.0, lambda: fake.state(runs=[])).start()
    waited = upgrade_daemons.wait_for_runs(found, Options(wait_runs=0.5), fake.rlxd, events)
    assert waited.runs == ()


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
    around, _ = _around(fake, Options(restart_daemons=True, end_ptys=True))
    around.before_install()
    assert fake.read_state()["killed"] == ["p1", "p2"]
    around.installed("2.0.0", None)
    around.finish(())


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
    fake.start()
    watch = fake.start("watch")
    (fake.dir / "stuck-rlxd").write_text("", encoding="utf-8")  # rlxd ignores its stop
    around, out = _around(fake, Options(restart_daemons=True))

    with pytest.raises(Refused) as refused:
        around.before_install()

    assert refused.value.code == "daemon_stop_failed"
    assert len(fake.live("rlxd")) == 1, "rlxd never stopped"
    (fake.dir / "stuck-rlxd").unlink()
    # The watch, stopped first, runs again.
    assert fake.wait_live("watch", not_pid=watch.pid).pid != watch.pid
    assert ("stop_daemons", "failed") in _steps(_events(out))


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
    assert _steps(_events(out))[0] == ("stop_daemons", "skipped")


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


def test_on_windows_a_running_daemon_refuses_a_plain_upgrade(
    fake: Daemons, tool: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake.start()
    monkeypatch.setattr(sys, "platform", "win32")
    result = CliRunner().invoke(app, ["upgrade", "--yes", "--json-lines"])
    assert result.exit_code == 1, result.output
    last = json.loads(result.stdout.splitlines()[-1])
    assert last["type"] == "upgrade.error" and last["data"]["code"] == "daemon_running"


def test_json_lines_needs_yes() -> None:
    result = CliRunner().invoke(app, ["upgrade", "--json-lines"])
    assert result.exit_code == 2
