"""The rlx service (S1, #32): with no TUI open, Remote serves, the usage reads go on and a
manager nudge a draft held back goes out (``services.service_host``).

Every test drives the service alone, with no ``FleetApp`` anywhere, in an isolated home
(``AISQUARE_HOME`` and the agents' homes are the conftest's). The parts are driven through
the same code a TUI drives: Remote's controller over the Remote suite's fake server and
tunnel, the usage read over a scripted endpoint, the nudge retry over the fleet suite's fake
tmux. The process itself (``start``/``stop``) runs as a real child that hosts no parts: a
child does not inherit a test's stubs, and the usage part would read this machine's login.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import threading
import time
from collections.abc import Callable, Iterator
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.cli.service import status_lines
from aisquare.cli.ui import remote_control, service_client
from aisquare.cli.ui.app import FleetApp
from aisquare.cli.ui.instances_chip import InstancesChip, chip_text, place
from aisquare.cli.ui.remote_control import RemoteController
from aisquare.core import instances
from aisquare.core.state_file import update_state
from aisquare.models import ProjectInfo
from aisquare.services import claude_accounts as accounts_service
from aisquare.services import fleet as fleet_service
from aisquare.services import remote_server, service_host
from aisquare.services.fleet import NUDGE_TEXT
from aisquare.services.ngrok_tunnel import build_public_url
from aisquare.services.service_host import (
    NudgePart,
    PartHealth,
    RemotePart,
    ServiceError,
    ServiceHost,
    ServiceRemote,
    ServiceStatus,
    UsagePart,
)
from aisquare.statusline import read_fleet
from tests import test_delivery_guards as guards
from tests import test_fleet_service as fleet_suite
from tests.test_fleet_service import FakeTmux
from tests.test_remote_control import FakeServer, fake_server, fake_tunnel_factory
from tests.test_ui_shell import no_real_tmux as _no_real_tmux
from tests.test_ui_shell import refreshed, seed, settle
from tests.test_ui_shell import script as _script
from tests.test_usage_aware_accounts import _payload, _slot, _Usage

# The fleet suite's fixtures, bound here so pytest finds them for this module's tests.
tmux = fleet_suite.tmux
clock = fleet_suite.clock
claude_on_path = fleet_suite.claude_on_path
repo = fleet_suite.repo
project = fleet_suite.project
no_real_tmux = _no_real_tmux
script = _script

pytestmark = pytest.mark.skipif(sys.platform == "win32", reason=service_host.NOT_HERE)

PUBLIC = "https://quiet-otter.ngrok-free.app"


def _until(done: Callable[[], bool], what: str, *, within: float = 20.0) -> None:
    """Wait for ``done`` (a bounded poll on the condition itself, never a sleep for time)."""
    deadline = time.monotonic() + within
    while not done():
        if time.monotonic() >= deadline:
            raise AssertionError(f"not within {within:.0f} s: {what}")
        time.sleep(0.05)


# --- the host: each part at its own cadence, a failure no end ----------------------------------


class Counting:
    """A part that counts its ticks, and raises on the ticks ``fail`` names."""

    def __init__(
        self, name: str, every: float, *, first: float = 0.0, fail: set[int] | None = None
    ):
        self.name, self.every, self.first = name, every, first
        self.ticks = 0
        self.started = self.stopped = False
        self._fail = fail or set()

    def start_part(self) -> None:
        self.started = True

    def tick_part(self) -> PartHealth:
        self.ticks += 1
        if self.ticks in self._fail:
            raise RuntimeError(f"tick {self.ticks} failed")
        return PartHealth("ok", f"{self.ticks} ticks")

    def stop_part(self) -> None:
        self.stopped = True


def test_each_part_ticks_at_its_own_cadence_and_a_failure_is_said_not_fatal(tmp_path: Path) -> None:
    now = [100.0]
    fast = Counting("fast", 2.0)
    slow = Counting("slow", 60.0, first=45.0, fail={1})
    written = tmp_path / "service" / "status.json"
    host = ServiceHost([fast, slow], clock=lambda: now[0], status_path=written)
    host.start_parts()
    assert fast.started and slow.started

    waits = []
    while now[0] < 220.0:  # two minutes of the clock, a tick wherever the host says
        waits.append(host.tick_parts())
        now[0] += waits[-1]
    assert (fast.ticks, slow.ticks) == (60, 2), "every 2 s; at 45 s, then a minute on"
    assert max(waits) <= service_host.STATUS_SECONDS, "it wakes for the status, at least"
    health = host.health()
    assert health["slow"].state == "ok", "the failed tick is said, and the next one runs"
    assert json.loads(written.read_text())["parts"]["fast"]["state"] == "ok"
    assert oct(written.stat().st_mode & 0o777) == "0o600"

    failing = ServiceHost([Counting("bad", 5.0, fail={1})], clock=lambda: now[0])
    failing.start_parts()
    failing.tick_parts()
    assert failing.health()["bad"].state == "error"
    assert failing.health()["bad"].detail == "RuntimeError: tick 1 failed"
    host.stop_parts()
    assert fast.stopped and slow.stopped


# --- acceptance 2: with the service alone --------------------------------------------------------


@pytest.fixture
def server() -> FakeServer:
    return fake_server()


def _remote(server: FakeServer) -> RemotePart:
    return RemotePart(
        lambda: RemoteController(server=server, tunnel_factory=fake_tunnel_factory(url=PUBLIC))
    )


def test_remote_serves_with_no_tui_and_follows_the_switch_a_tui_flips(server: FakeServer) -> None:
    """The switch a TUI saved on: the service serves Remote at its start, publicly tunnelled,
    with no TUI open. A TUI flipping the switch off (``state.json``) turns it off at the next
    follow; on again, on. Stopping the service keeps the switch: its next start serves."""
    update_state("remote_enabled", True)
    part = _remote(server)
    part.start_part()

    health = part.tick_part()

    assert server.running and health.state == "ok"
    assert health.extra["on"] is True
    _until(lambda: part.health().extra["public_origin"] == PUBLIC, "ngrok's link")

    update_state("remote_enabled", False)  # a TUI's switch, as it saves it
    assert part.tick_part().state == "off" and not server.running
    update_state("remote_enabled", True)
    assert part.tick_part().state == "ok" and server.running

    part.stop_part()
    assert not server.running
    assert remote_control.load_remote_state().remote_enabled, "the switch kept for the next start"
    again = _remote(server)
    again.start_part()
    assert again.tick_part().state == "ok" and server.running, "the next start serves again"
    again.stop_part()


def test_remote_another_rlx_serves_is_waited_for_not_fought(server: FakeServer) -> None:
    """A TUI that served Remote before the service started keeps it; the service says it waits,
    and takes it once that TUI let the home go."""
    update_state("remote_enabled", True)
    server.served_elsewhere = True
    server.fail_start = remote_server.RemoteAlreadyOn(remote_server.remote_already_on())
    part = _remote(server)
    part.start_part()
    assert part.tick_part().state == "waiting"
    server.served_elsewhere, server.fail_start = False, None
    assert part.tick_part().state == "ok" and server.running
    part.stop_part()


def test_usage_reads_go_on_with_no_tui_and_the_before_limit_hook_reads_them(
    fake_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The usage part reads the accounts (the network only past five minutes), records the
    readings, and writes every agent's status line figure: what the before-limit hand-over's
    hook reads on record (``line_reading(on_record=True)``), which asks nothing itself."""
    _slot("work@example.com", "tok-work")  # slot 2
    fetch = _Usage({"tok-work": _payload(64)})
    monkeypatch.setattr(accounts_service, "_http_get", fetch)
    lines = accounts_service.statusline_dir()
    lines.mkdir(parents=True, exist_ok=True)  # where a status line has run: it writes there
    part = UsagePart()

    health = part.tick_part()

    assert health.state == "ok" and "claude-code" in (health.detail or ""), health
    assert fetch.calls == ["tok-work"]
    assert read_fleet(lines), "every agent's status line has the fleet's figure"
    line = accounts_service.line_reading(2, switch_at=90, on_record=True)
    assert line is not None and not line.over, "the hook finds the service's reading on record"
    part.tick_part()
    assert fetch.calls == ["tok-work"], "a minute later the cache answers"


def test_a_nudge_a_draft_held_back_goes_out_with_no_tui(
    tmux: FakeTmux, claude_on_path: Path, project: ProjectInfo
) -> None:
    manager = guards._waiting(tmux, project)
    tmux.screens[manager.pane_id] = guards._box(guards.DRAFT)
    assert fleet_service.nudge_manager(project.id, reason="task_done") is False
    part = NudgePart()

    assert part.tick_part().detail == "0 sent since the service started", "the draft is there"
    tmux.screens[manager.pane_id] = guards._box()  # the human cleared it
    health = part.tick_part()

    assert (manager.pane_id, "paste", NUDGE_TEXT) in tmux.typed
    assert health.detail == "1 sent since the service started"
    assert guards._deferred(manager) is None


# --- the process: one per home, registered, stopped by a quit request ---------------------------


@pytest.fixture
def serving() -> Iterator[Callable[..., threading.Thread]]:
    """Run ``serve`` in a thread of this process (no signals: they are the main thread's);
    every one started is asked to quit and joined at teardown."""
    threads: list[threading.Thread] = []

    def run(parts: list[Any] | None = None) -> threading.Thread:
        failed: list[BaseException] = []

        def body() -> None:
            try:
                service_host.run_service(parts=[] if parts is None else parts, signals=False)
            except BaseException as exc:  # the test reads it
                failed.append(exc)

        thread = threading.Thread(target=body, daemon=True)
        thread.failed = failed  # type: ignore[attr-defined]
        thread.start()
        threads.append(thread)
        _until(lambda: service_host.running_service() is not None or failed, "it registered")
        return thread

    yield run
    service = service_host.running_service()
    if service is not None:
        instances.request_quit(service, by=instances.describe_this_process())
    for thread in threads:
        thread.join(20)


def test_the_service_registers_as_one_instance_writes_its_status_and_quits_on_request(
    serving: Callable[..., threading.Thread],
) -> None:
    part = Counting("probe", 5.0)
    thread = serving([part])

    service = service_host.running_service()
    assert service is not None and service.kind == "service" and service.pid == os.getpid()
    _until(lambda: (service_host.read_status() or {}).get("parts", {}).get("probe"), "status")
    status = service_host.read_status()
    assert status is not None and status["pid"] == os.getpid() and status["instance"] == service.id
    _until(lambda: instances.live_instances()[0].kind == "service", "a beat keeps its kind")

    with pytest.raises(ServiceError, match="already runs on this home"):
        service_host.run_service(parts=[], signals=False)

    instances.request_quit(service, by=instances.describe_this_process())
    thread.join(20)
    assert not thread.is_alive() and part.stopped
    assert service_host.running_service() is None and service_host.read_status() is None


def test_the_service_starts_nothing_beside_an_rlx_of_another_release() -> None:
    older = replace(instances.describe_this_process(kind="native"), version="0.9.2")
    with instances.gate():
        registration = instances.register(older)
    try:
        with pytest.raises(ServiceError, match=r"rlx 0\.9\.2 \(native .*runs another release"):
            service_host.run_service(parts=[], signals=False)
        assert service_host.running_service() is None
    finally:
        registration.deregister()


def test_status_json_reports_pid_version_uptime_and_parts(
    serving: Callable[..., threading.Thread],
) -> None:
    serving([Counting("probe", 5.0)])
    _until(lambda: service_host.read_status() is not None, "its status")

    result = CliRunner().invoke(app, ["--json", "service", "status"])

    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert data["running"] is True and data["pid"] == os.getpid()
    assert data["version"] == data["this_version"] and data["version_mismatch"] is None
    assert isinstance(data["uptime_s"], int) and data["uptime_s"] >= 0
    assert data["parts"]["probe"]["state"] == "ok"


def test_a_version_mismatch_between_the_service_and_this_rlx_is_named() -> None:
    now = datetime.now(UTC)
    service = replace(
        instances.describe_this_process(kind="service"),
        version="0.10.3",
        started_at=now - timedelta(hours=2, minutes=5),
    )
    status = ServiceStatus(
        service, {"parts": {"remote": {"state": "off", "detail": None}}}, "0.10.2"
    )

    assert status.mismatch == "version mismatch: the service runs rlx 0.10.3, this rlx is 0.10.2"
    assert status.to_json(now)["version_mismatch"] == status.mismatch
    lines = status_lines(status, now)
    assert lines[0].startswith("the rlx service runs: rlx 0.10.3, pid ") and "up 2h 5m" in lines[0]
    assert lines[1] == f"⚠ {status.mismatch}" and lines[2] == "  remote: off"


def test_status_with_no_service_says_so_and_exits_3() -> None:
    result = CliRunner().invoke(app, ["service", "status"])
    assert result.exit_code == 3
    assert "the rlx service is not running" in result.stdout


def test_start_and_stop_run_it_in_the_background_and_wind_it_down(tmp_path: Path) -> None:
    """``start`` and ``stop`` are what the home migration and the upgrade call: a child in a
    session of its own, live in the registry when ``start`` returns, gone when ``stop`` does.
    The child hosts no parts (it inherits no test stub)."""
    child = [
        sys.executable,
        "-P",
        "-c",
        "from aisquare.services import service_host as s; s.run_service(parts=[])",
    ]
    service = service_host.start_service(argv=child)
    try:
        assert service.kind == "service" and service.pid != os.getpid()
        assert service_host.start_service(argv=child).id == service.id, "one already runs: that one"
    finally:
        stopped = service_host.stop_service()
    assert stopped is True
    assert service_host.running_service() is None
    assert service_host.stop_service() is False, "none runs"


# --- the chip ------------------------------------------------------------------------------------


def test_the_chip_names_the_service_beside_the_tuis() -> None:
    here = instances.host_name()
    tui = instances.describe_this_process(kind="native")
    service = instances.describe_this_process(kind="service")
    assert place(service, host=here) == "service"
    text = chip_text([tui, service], tui.id, host=here)
    assert text is not None
    assert text.plain in ("2 rlx · Mac (here) + service", "2 rlx · local (here) + service")


def test_a_record_of_kind_service_reads_back_as_one() -> None:
    record = instances.describe_this_process(kind="service").to_record()
    read = instances.Instance.from_record(json.loads(json.dumps(record)))
    assert read is not None and read.kind == "service"


# --- a TUI is the service's client, and takes back what it hosted when it dies ------------------


def test_a_tui_is_the_services_client_its_switch_saved_and_nothing_started_here(
    server: FakeServer,
) -> None:
    """While the service serves Remote, a TUI's controller shows it on, with its link, and its
    switch saves what the service follows; a TUI start and quit start and stop nothing."""
    update_state("remote_enabled", True)
    controller = RemoteController(server=server, tunnel_factory=fake_tunnel_factory(url=PUBLIC))
    controller.service = ServiceRemote(on=True, public_origin=PUBLIC)

    controller.restore()  # the TUI's start: the service's to bring back, not this one's
    assert not server.running and server.calls.count("start_remote_server") == 0
    assert controller.client and controller.running and not controller.remote_serving
    assert controller.remote_link() == (build_public_url(PUBLIC, server.token), True)
    assert controller.password() == server.password
    assert remote_control.SERVED_BY_SERVICE in controller.status_line()

    controller.turn_off()  # the panel's switch
    assert remote_control.load_remote_state().remote_enabled is False, "what the service follows"
    assert remote_control.SERVICE_TURNS_OFF in controller.status_line()
    controller.service = ServiceRemote(on=False)  # the next frame: the service did
    assert not controller.running and controller.status_line() == ""

    controller.turn_on()
    assert remote_control.load_remote_state().remote_enabled is True and not server.running
    controller.shutdown_for_exit()  # the TUI quits: the service's Remote and switch stay
    assert remote_control.load_remote_state().remote_enabled is True
    assert "stop_remote_server" not in server.calls and "start_remote_server" not in server.calls


def test_the_service_follows_the_auto_off_a_tui_client_picks(server: FakeServer) -> None:
    update_state("remote_enabled", True)
    part = _remote(server)
    part.start_part()
    part.tick_part()
    assert part.controller is not None and part.controller.remote_serving
    client = RemoteController(server=server, tunnel_factory=fake_tunnel_factory(url=PUBLIC))
    client.service = ServiceRemote(on=True, public_origin=PUBLIC)

    client.set_auto_off(30)
    assert "set_auto_off" not in server.calls, "nothing written by the client"
    part.tick_part()

    assert part.controller.state.auto_off_minutes == 30
    assert server.auto_off_calls and server.auto_off_calls[-1] is not None, "the service's write"
    part.stop_part()


@pytest.fixture
def registered_service() -> Iterator[Callable[[], None]]:
    """A live service record on this home (this process's), with its Remote serving per its
    status; the fixture's call ends it, as the service dying does."""
    me = instances.describe_this_process(kind="service")
    with instances.gate():
        registration = instances.register(me)
    status = {
        "instance": me.id,
        "parts": {"remote": {"state": "ok", "on": True, "public_origin": PUBLIC}},
    }
    service_host.status_path().parent.mkdir(parents=True, exist_ok=True)
    service_host.status_path().write_text(json.dumps(status), encoding="utf-8")

    def die() -> None:
        registration.deregister()
        service_host.status_path().unlink(missing_ok=True)

    yield die
    die()


def test_a_tui_leaves_remote_and_nudges_to_the_service_and_takes_them_back_when_it_dies(
    tmp_path: Path,
    script: Any,
    no_real_tmux: Any,
    registered_service: Callable[[], None],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Q4 (the manager's ruling on #32): with the service live, a TUI opened with Remote's
    switch on starts no Remote of its own, retries no held-back nudge, and its chip names the
    service. When the service dies, the next housekeeping tick starts Remote here and says so,
    once; the next frame retries the nudges again."""
    seed(tmp_path, ("prj_a", "alpha", None))
    update_state("remote_enabled", True)
    server = fake_server()
    with instances.gate():  # this TUI's own record, beside the service's: the chip shows both
        tui = instances.register(instances.describe_this_process(kind="native"))
    nudged: list[object] = []
    monkeypatch.setattr(fleet_service, "send_deferred_nudges", lambda owed: nudged.append(owed))

    async def run() -> tuple[bool, bool, str, int, bool, list[str]]:
        controller = RemoteController(server=server, tunnel_factory=fake_tunnel_factory(url=PUBLIC))
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [], remote=controller)
        async with app.run_test(size=(120, 36), notifications=True) as pilot:
            await pilot.pause()
            await settle(app)
            await refreshed(app)
            client_then = app.remote.client and not server.running
            nudges_then = not nudged
            chip = str(app.query_one(InstancesChip).render())
            registered_service()  # the service dies
            await refreshed(app)
            app._housekeeping()
            await settle(app)
            app._housekeeping()  # said once
            await settle(app)
            said = [str(n.message) for n in app._notifications]
            return client_then, nudges_then, chip, len(nudged), server.running, said

    try:
        client_then, nudges_then, chip, nudges_after, serving_after, said = asyncio.run(run())
    finally:
        tui.deregister()
    assert client_then, "the service's Remote: none started here"
    assert nudges_then, "the service retries them, not the TUI's frame"
    assert chip.endswith("(here) + service"), chip
    assert serving_after, "Remote taken back: its switch is on"
    assert nudges_after >= 1, "the frame retries the nudges again"
    assert said.count(service_client.TAKEN_BACK) == 1, said


def test_the_cli_every_hook_imports_does_not_import_the_service() -> None:
    """``rlx service`` is registered with the CLI, which every hook imports: its module imports
    the service only when a command runs, so the hook path pays nothing for it."""
    import subprocess

    probe = (
        "import sys, aisquare.cli.app; "
        "print('aisquare.services.service_host' in sys.modules, 'aisquare.cli.ui' in sys.modules)"
    )
    out = subprocess.run(
        [sys.executable, "-P", "-c", probe], capture_output=True, text=True, check=True
    ).stdout.split()
    assert out == ["False", "False"], out


class _Child:
    """A child process as ``subprocess.Popen`` gives it: running until ``ends``."""

    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.ended = False

    def poll(self) -> int | None:
        return 0 if self.ended else None


def test_the_sweep_runs_in_a_child_one_at_a_time(monkeypatch: pytest.MonkeyPatch) -> None:
    """The sweep keeps what it read in the process that runs it (about 140 MB, measured for the
    idle budget): the service runs ``metrics stats`` as a detached child instead, and starts
    no second one while the first still sweeps."""
    started: list[list[str]] = []

    def detached(command: list[str]) -> _Child:
        started.append(list(command))
        return _Child(4000 + len(started))

    monkeypatch.setattr(service_host, "_detached", detached)
    part = service_host.SweepPart()

    assert part.tick_part().detail == "started a sweep (pid 4001)"
    assert part.tick_part().detail == "a sweep runs (pid 4001)"
    part._child.ended = True  # type: ignore[union-attr]
    assert part.tick_part().detail == "started a sweep (pid 4002)"
    sweep = ["--json", "metrics", "stats", "--range", "today"]
    assert [command[-len(sweep) :] for command in started] == [sweep, sweep]


def test_the_shipment_is_the_clis_detached_one_and_nothing_while_sharing_is_off(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from aisquare.core import sharing

    calls: list[str] = []
    monkeypatch.setattr(sharing, "start_background_ship", lambda: calls.append("ship") or True)
    part = service_host.ShipPart()

    monkeypatch.setattr(sharing, "enabled", lambda: False)
    assert part.tick_part().state == "off" and calls == []
    monkeypatch.setattr(sharing, "enabled", lambda: True)
    assert part.tick_part().detail == "started a shipment" and calls == ["ship"]
