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
import itertools
import json
import logging
import os
import subprocess
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


@pytest.mark.parametrize("over_ssh", [False, True])
def test_the_usage_part_says_the_keychain_is_locked_in_an_ssh_logins_session(
    monkeypatch: pytest.MonkeyPatch, over_ssh: bool
) -> None:
    """Design A.3 of #32, which docs/service.md promises (#1717): on macOS a service started
    from an SSH login keeps that login's security session, where the Keychain does not answer;
    its usage part's health says so. At the Mac, it reads as before."""
    from types import SimpleNamespace

    monkeypatch.setattr(accounts_service, "list_accounts", lambda: [])
    reading = SimpleNamespace(level="good", usable=0, needs_login=0)
    part = UsagePart(read=lambda **_: reading, out_of_reach=lambda: over_ssh)  # type: ignore[arg-type]
    part.start_part()

    health = part.tick_part()

    if over_ssh:
        assert health.state == "error" and health.detail is not None
        assert health.detail.startswith("Keychain locked"), health.detail
    else:
        assert health.state == "ok" and "Keychain" not in (health.detail or "")


@pytest.mark.parametrize("json_output", [False, True])
def test_service_start_from_an_ssh_login_warns_it_reads_no_keychain(
    monkeypatch: pytest.MonkeyPatch, json_output: bool
) -> None:
    """Design A.3: `rlx service start` run over SSH on macOS starts the service, as asked, and
    warns that it reads no usage there. Nothing really starts here: start_service is a stub."""
    me = instances.describe_this_process(kind="service")
    monkeypatch.setattr(service_host, "running_service", lambda **_: None)
    monkeypatch.setattr(service_host, "start_service", lambda **_: me)
    monkeypatch.setattr(service_host, "keychain_out_of_reach", lambda: True)

    args = ["--json", "service", "start"] if json_output else ["service", "start"]
    result = CliRunner().invoke(app, args)

    assert result.exit_code == 0, result.output
    if json_output:
        said = json.loads(result.output)
        assert said["started"] is True and said["warning"] == service_host.KEYCHAIN_OUT_OF_REACH
    else:
        assert f"warning: {service_host.KEYCHAIN_OUT_OF_REACH}" in result.output.replace("\n", " ")

    monkeypatch.setattr(service_host, "keychain_out_of_reach", lambda: False)
    quiet = CliRunner().invoke(app, ["--json", "service", "start"])
    assert "warning" not in json.loads(quiet.output), "at the Mac: no warning"


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
        _until(lambda: service_host.running_service() is not None or bool(failed), "it registered")
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


def test_start_and_stop_run_it_in_the_background_and_wind_it_down(
    tmp_path: Path, real_service: list[Any]
) -> None:
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


def test_a_remote_that_will_not_start_is_tried_a_few_times_then_switched_off_and_said_why(
    server: FakeServer, caplog: pytest.LogCaptureFixture
) -> None:
    """#1717 MAJOR 2: a Remote the switch asks for that cannot start (its port taken) was tried
    every follow for ever, a WARNING in service.log each time, and the switch stayed on. Now
    it is tried again after each of REMOTE_RETRY_SECONDS, then no more: the switch is saved
    off, why is in the part's health for a TUI to say, and the log says it twice in all (the
    failure, the giving up). Turned on again, it is tried afresh."""
    update_state("remote_enabled", True)
    taken = remote_server.RemoteError("port 18751 is in use by another program")
    server.fail_start = taken
    tries: list[float] = []
    real_start = server.start_remote_server
    now = [0.0]

    def start(*args: Any, **kwargs: Any) -> Any:
        tries.append(now[0])
        return real_start(*args, **kwargs)

    server.start_remote_server = start  # type: ignore[method-assign]
    part = RemotePart(
        lambda: RemoteController(server=server, tunnel_factory=fake_tunnel_factory(url=PUBLIC)),
        clock=lambda: now[0],
    )
    part.start_part()
    caplog.set_level(logging.INFO, logger=service_host.__name__)
    health = []
    while now[0] <= 200.0:  # every follow for 200 s, as the service ticks it
        health.append(part.tick_part())
        now[0] += service_host.REMOTE_FOLLOW_SECONDS

    assert len(tries) == service_host.REMOTE_TRIES == 6, tries
    waits = [later - before for before, later in itertools.pairwise(tries)]
    assert waits == list(service_host.REMOTE_RETRY_SECONDS), waits
    assert remote_control.load_remote_state().remote_enabled is False, "the switch saved off"
    last = health[-1]
    assert last.state == "off" and last.extra["problem"].endswith(str(taken)), last
    warned = [r.getMessage() for r in caplog.records if r.levelno == logging.WARNING]
    assert len(warned) == 2 and warned[-1].endswith("so its switch is saved off"), warned

    # status.json carries why to a TUI, which reads it into its ServiceRemote
    me = instances.describe_this_process(kind="service")
    status = {"instance": me.id, "parts": {"remote": last.to_json()}}
    service_host.status_path().parent.mkdir(parents=True, exist_ok=True)
    service_host.status_path().write_text(json.dumps(status), encoding="utf-8")
    seen = service_host.hosting([me])
    assert seen is not None and seen.remote.problem == last.extra["problem"]
    assert seen.remote.problem_at is not None and not seen.remote.on

    update_state("remote_enabled", True)  # a TUI's switch, on again: tried afresh
    part.tick_part()
    assert len(tries) == service_host.REMOTE_TRIES + 1 and part.gave_up is None
    part.stop_part()


def test_a_client_says_why_the_service_stopped_trying_and_its_switch_is_off(
    server: FakeServer,
) -> None:
    """#1717 MAJOR 2, a TUI's side: the panel said 'turns Remote on…' for ever and its switch
    read off. Once the service has stopped trying, the line says why and that the switch is
    off; a press of it after that asks again, and the old give-up is no longer its answer."""
    update_state("remote_enabled", False)
    controller = RemoteController(server=server, tunnel_factory=fake_tunnel_factory(url=PUBLIC))
    controller.service = ServiceRemote(on=False)
    controller.turn_on()
    assert remote_control.SERVICE_TURNS_ON in controller.status_line()

    why = "Remote could not start — port 18751 is in use by another program"
    gave_up = ServiceRemote(on=False, problem=why, problem_at=datetime.now(UTC))
    controller.service = gave_up  # the next frame: the service stopped trying
    line = controller.status_line()
    assert remote_control.service_gave_up(why) in line, line
    assert remote_control.SERVICE_TURNS_ON not in line
    assert not controller.running, "its switch reads off, as it is saved"

    controller.turn_on()  # a press after the give-up: the service tries again
    line = controller.status_line()
    assert remote_control.SERVICE_TURNS_ON in line and why not in line, line
    assert remote_control.load_remote_state().remote_enabled is True
    assert "start_remote_server" not in server.calls, "a client starts nothing itself"


def _client_of_a_live_service(
    server: FakeServer, *, switch_on: bool
) -> tuple[RemoteController, service_client.ServiceClient, list[str]]:
    """A TUI's controller and service client, built with Remote's switch saved as given, while
    a service of this release runs: the client's view, Remote served there or not."""
    update_state("remote_enabled", switch_on)
    controller = RemoteController(server=server, tunnel_factory=fake_tunnel_factory(url=PUBLIC))
    said: list[str] = []
    client = service_client.ServiceClient(controller, said.append)
    me = instances.describe_this_process(kind="service")
    client.note_hosting(service_host.Hosting(me, ServiceRemote(on=switch_on, public_origin=PUBLIC)))
    return controller, client, said


def test_a_take_back_after_the_services_auto_off_brings_no_remote_back(server: FakeServer) -> None:
    """#1717 (a): a TUI client turns Remote on, so its own copy of the switch says on, and the
    service serves it. The service's auto-off then runs out: it turns its Remote off and saves
    the switch off. When the service dies, the take-back reads the switch as saved now, so no
    public Remote and no ngrok come back here; it says it keeps the nudges, not Remote."""
    controller, client, said = _client_of_a_live_service(server, switch_on=False)
    controller.turn_on()  # the panel's switch, a client's: saved on, for the service to follow
    assert controller.state.remote_enabled and not server.running
    update_state("remote_enabled", False)  # the service's auto-off, as its controller saves it

    client.note_hosting(None)  # the next frame: the service is gone
    assert client.take_back()
    controller.writes_done(5.0)  # a start would have gone through its writes by now

    assert not server.running and "start_remote_server" not in server.calls
    assert said == [service_client.TAKEN_BACK_NUDGES], said
    assert controller.state.remote_enabled is False


def test_a_take_back_honours_a_switch_another_window_turned_on(server: FakeServer) -> None:
    """#1717 (b): a TUI client built while the switch was off; another rlx window turns Remote
    on, and the service serves it. When the service dies, the take-back starts Remote here,
    as the switch saved now says, and says so."""
    controller, client, said = _client_of_a_live_service(server, switch_on=False)
    update_state("remote_enabled", True)  # another window's switch, as it saves it

    client.note_hosting(None)
    assert client.take_back()
    try:
        _until(lambda: server.running, "Remote started here again", within=5.0)
        assert said == [service_client.TAKEN_BACK], said
    finally:
        controller.shutdown_for_exit()
        controller.wait_until_off(5.0)


@pytest.mark.parametrize("command", ["upgrade", "uninstall"])
def test_the_tuis_update_and_uninstall_hand_over_beside_the_service(
    command: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    """#1743 MAJOR 2: a TUI's Update and Uninstall refused beside the service, which the
    registry lists as one more rlx; the command they hand over to stops it itself (and the
    upgrade starts it again), as `lifecycle.running_asqs` already says. Beside a second rlx a
    person must quit, they are still refused, and the refusal names that one only."""
    from aisquare.cli.ui.views.doctor import DoctorView
    from tests.asq_elsewhere import asq_running

    monkeypatch.setattr(instances, "HEARTBEAT_SECONDS", 0.05)
    with instances.gate():
        me = instances.register(instances.describe_this_process())
    me.start()

    async def beaten(count: int) -> None:
        for _ in range(500):
            if len(me.others) == count:
                return
            await asyncio.sleep(0.01)
        pytest.fail(f"the beat thread read {me.others}")

    async def run() -> tuple[tuple[str, ...] | None, str, tuple[str, ...] | None]:
        app = FleetApp(refresh_seconds=3600, doctor=lambda: [])
        async with app.run_test(size=(120, 40), notifications=True) as pilot:
            await pilot.pause()
            with asq_running(kind="service") as service:
                with asq_running(version="0.0.1") as other:
                    await beaten(2)
                    app.on_doctor_view_hand_off(DoctorView.HandOff((command, "--reopen")))
                    await pilot.pause()
                    refused = app.hand_off
                    said = " ".join(str(note.message) for note in app._notifications)
                    assert f"({other.label})" in said and service.label not in said, said
                await beaten(1)
                app.on_doctor_view_hand_off(DoctorView.HandOff((command, "--reopen")))
                return refused, said, app.hand_off

    try:
        refused, said, handed = asyncio.run(run())
    finally:
        me.deregister()
    assert refused is None and "another rlx runs on this home" in said, "a second rlx: refused"
    assert handed == (command, "--reopen"), "beside the service alone: it hands over"


def test_no_test_starts_a_real_service_unless_it_asks_for_one() -> None:
    """#1743: tests/test_stubs.py ran `service start` with no arguments, and real services
    were left running after pytest. The suite refuses the one place a service is started
    (tests/conftest.py::no_real_service) unless a test asks for real_service."""
    with pytest.raises(ServiceError, match="a test may not start a real rlx service"):
        service_host.start_service(argv=[sys.executable, "-c", "pass"])
    result = CliRunner().invoke(app, ["service", "start"])
    assert result.exit_code != 0 and "may not start a real rlx service" in result.output
    assert service_host.running_service() is None


def test_a_start_that_fails_quotes_what_that_run_wrote_not_an_earlier_ones(
    real_service: list[Any],
) -> None:
    """#1743 nit: a failed start quoted the log's last lines, an earlier run's included, before
    its own reason. It quotes what the child it started wrote."""
    log = service_host.log_path()
    log.parent.mkdir(parents=True, exist_ok=True)
    log.write_text("2026-11-20 09:00:00 INFO an earlier run's last line\n", encoding="utf-8")
    child = [sys.executable, "-c", "import sys; print('the real reason'); sys.exit(1)"]

    with pytest.raises(ServiceError) as raised:
        service_host.start_service(argv=child)

    said = str(raised.value)
    assert said.endswith("the real reason") and "earlier run" not in said, said
    assert len(real_service) == 1 and real_service[0].wait(timeout=5) == 1


def test_the_log_rolls_over_and_the_services_own_output_follows_it() -> None:
    """#1717/#1842: service.log rolls over at LOG_BYTES, one old file kept, and the service's own
    fd 1 and 2 (its log, as start_service hands it) follow each rollover: uvicorn's raw lines
    and a traceback went on into the rolled-over file, then an unlinked one. Also rolled when
    only fd 1 and 2 filled it, which no handler sees. In a child process: the dup2 is that
    process's own."""
    import textwrap

    script = textwrap.dedent(
        """
        import logging, os
        from aisquare.services import service_host as s
        s.LOG_BYTES = 600
        s.log_to_file()
        say = logging.getLogger("rlx-test")
        for i in range(40):
            say.info("a logged line %02d %s", i, "x" * 30)
        os.write(1, b"stdout after the rolls\\n")
        os.write(2, b"stderr after the rolls\\n")
        os.write(1, b"r" * 700 + b"\\n")  # raw only, past LOG_BYTES: no handler sees it
        s.roll_log_if_due()
        os.write(1, b"stdout after the raw roll\\n")
        """
    )
    log = service_host.log_path()
    log.parent.mkdir(parents=True, exist_ok=True)
    with open(log, "ab") as out:  # as start_service hands the service its log
        subprocess.run(
            [sys.executable, "-P", "-c", script],
            stdout=out,
            stderr=subprocess.STDOUT,
            check=True,
            timeout=60,
        )

    now = log.read_text(encoding="utf-8")
    old = log.with_name(log.name + ".1").read_text(encoding="utf-8")
    assert "stdout after the raw roll" in now and len(now) < 600, now
    assert "stdout after the rolls" in old and "stderr after the rolls" in old, old
    assert "r" * 700 in old, "the raw-only bytes rolled over too"
    assert not log.with_name(log.name + ".2").exists(), "one old file kept"


@pytest.mark.parametrize(
    ("verb", "error"),
    [
        ("start", "service_not_started"),
        ("stop", "service_not_stopped"),
        ("restart", "service_not_restarted"),
        ("run", "service_not_started"),
    ],
)
def test_a_failed_service_command_says_why_in_its_json_detail(
    monkeypatch: pytest.MonkeyPatch, verb: str, error: str
) -> None:
    """#1842 nit: S3's tray reads `rlx --json service …`'s failures; each carries its why as
    detail, as the upgrade's and the uninstall's do."""
    why = "the rlx service (pid 7) did not stop within 15 s"

    def refuse(**_: Any) -> Any:
        raise ServiceError(why)

    for name in ("start_service", "stop_service", "restart_service", "run_service"):
        monkeypatch.setattr(service_host, name, refuse)
    monkeypatch.setattr(service_host, "log_to_file", lambda: None)
    monkeypatch.setattr(service_host, "running_service", lambda **_: None)

    result = CliRunner().invoke(app, ["--json", "service", verb])

    assert result.exit_code != 0
    said = json.loads(next(line for line in result.output.splitlines() if line.startswith("{")))
    assert said["error"] == error and said["detail"] == why, said


@pytest.mark.parametrize(
    ("answer", "kind"),
    [("not found", "max"), ("locked", "unknown"), ("refused", "unknown")],
)
def test_the_service_sweep_asks_each_login_once_in_six_hours_whatever_the_keychain_says(
    machine: Any, monkeypatch: pytest.MonkeyPatch, answer: str, kind: str
) -> None:
    """#1891 (b), the manager's decision: with transcripts to attribute, `rlx service sweep`
    reads each account's subscription as a TUI's sweep does, once in ACCOUNT_TYPES_MAX_AGE (6 h),
    and a Keychain that is locked or refused is not asked again before then either. The
    Keychain is ON with read_keychain recording, so every read is seen. The plain slot is the
    one account here; its login file holds 'max', which a Keychain with no item falls back to."""
    from aisquare.core import claude_accounts as accounts_core
    from aisquare.core.store import store_session
    from aisquare.services import usage_stats
    from tests.usage_stats_support import NOW, prompt, request

    reads: list[tuple[str, str]] = []

    def keychain(service: str, account: str, *, fresh: bool = False) -> Any:
        reads.append((service, account))
        if answer == "not found":
            return accounts_core.KeychainRead(found=False)
        return accounts_core.KeychainRead(found=False, unreadable=answer, problem=answer)  # type: ignore[arg-type]

    monkeypatch.setattr(accounts_core, "read_keychain", keychain)
    monkeypatch.setattr(accounts_core, "keychain_platform", lambda: True)
    machine.account(1, "max")
    repo = machine.repo("served")
    machine.session(1, repo, "s1", prompt(NOW, cwd=repo), request(NOW, "m1", cwd=repo))
    plain = next(a for a in accounts_core.list_accounts() if a.slot == 1)
    item = (accounts_core.keychain_service(plain), accounts_core.keychain_account())

    first = CliRunner().invoke(app, ["service", "sweep"])
    assert first.exit_code == 0, first.output
    assert reads == [item], "one read of the one login"

    machine.session(1, repo, "s2", prompt(NOW, cwd=repo), request(NOW, "m2", cwd=repo))
    again = usage_stats.maybe_sweep(now=datetime.now(UTC) + timedelta(minutes=11))  # the next
    assert again.skipped is None and again.transcripts == 2, again  # it ran, over both
    assert reads == [item], "within six hours the cached type is used, locked or not"
    with store_session() as store:
        assert usage_stats.account_types(store, refresh=False) == {1: kind}


@pytest.mark.parametrize(
    "home", ["a ~/.claude, no transcripts", "a slot's transcript, no ~/.claude"]
)
def test_the_service_sweep_asks_the_keychain_only_for_a_login_it_has_a_use_for(
    machine: Any, monkeypatch: pytest.MonkeyPatch, home: str
) -> None:
    """Each of #1891's two hunks pinned on its own (review of #32, #1918 MINOR 2), the Keychain
    ON and every read recorded:
    - a home with Claude Code's ~/.claude and no transcript: nothing to attribute, so no
      account's login is read (_sweep's hunk);
    - a managed slot's transcript and no ~/.claude: that slot's login is read, the plain
      slot, which never ran here, is not (account_types' hunk)."""
    import shutil

    from aisquare.core import claude_accounts as accounts_core
    from tests.usage_stats_support import NOW, prompt, request

    reads: list[tuple[str, str]] = []

    def keychain(service: str, account: str, *, fresh: bool = False) -> Any:
        reads.append((service, account))
        return accounts_core.KeychainRead(found=False)

    monkeypatch.setattr(accounts_core, "read_keychain", keychain)
    monkeypatch.setattr(accounts_core, "keychain_platform", lambda: True)
    expected: list[tuple[str, str]] = []
    if home == "a ~/.claude, no transcripts":
        assert accounts_core.default_config_dir().is_dir()  # fake_home's
    else:
        machine.account(2, "pro")
        repo = machine.repo("slot-two")
        machine.session(2, repo, "s1", prompt(NOW, cwd=repo), request(NOW, "m1", cwd=repo))
        shutil.rmtree(accounts_core.default_config_dir())
        two = next(a for a in accounts_core.list_accounts() if a.slot == 2)
        expected = [(accounts_core.keychain_service(two), accounts_core.keychain_account())]

    ran = CliRunner().invoke(app, ["service", "sweep"])

    assert ran.exit_code == 0, ran.output
    assert reads == expected, reads


def test_one_account_folder_that_cannot_be_read_costs_the_sweep_nothing(
    machine: Any, tmp_path: Path
) -> None:
    """Review of #32 (#1918) MINOR 1: account_types asked Path.exists of each account's folder
    outside its per-account try, and Path.exists raises for one it cannot stat (EACCES), so
    ONE such account made the whole sweep return 'skipped' with nothing recorded, in the TUI,
    the shipper, metrics and the service alike. Claude Code's plain slot is listed whether its
    folder exists or not; here its ~/.claude is a link into a folder this user cannot enter,
    and a managed slot holds the transcript. That plain slot is skipped; the sweep records."""
    if hasattr(os, "geteuid") and os.geteuid() == 0:
        pytest.skip("root enters any folder")
    import shutil

    from aisquare.core import claude_accounts as accounts_core
    from aisquare.services import usage_stats
    from tests.usage_stats_support import NOW, prompt, request

    machine.account(2, "pro")
    repo = machine.repo("readable")
    machine.session(2, repo, "s1", prompt(NOW, cwd=repo), request(NOW, "m1", cwd=repo))
    plain = accounts_core.default_config_dir()
    shutil.rmtree(plain)
    locked = tmp_path / "locked"
    (locked / ".claude").mkdir(parents=True)
    plain.symlink_to(locked / ".claude")
    locked.chmod(0o000)  # the plain slot's folder cannot be stat'ed
    try:
        report = usage_stats.sweep()
    finally:
        locked.chmod(0o700)

    assert report.skipped is None, report
    assert report.sessions_new == 1, report


def test_a_log_folder_it_cannot_write_does_not_end_the_service_at_a_roll(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """#1889/#1891 MINOR 2: a log folder the service cannot write (0o500) plus a roll due ended
    the service within 5 s (roll_if_due -> doRollover -> PermissionError), its stream closed
    by the failed roll; under S2's KeepAlive, a crash loop. It goes on logging to the file it
    has, says once that it cannot roll, and tries no more."""
    if hasattr(os, "geteuid") and os.geteuid() == 0:
        pytest.skip("root renames in any folder")
    monkeypatch.setattr(service_host, "LOG_BYTES", 300)
    root = logging.getLogger()
    level = root.level
    service_host.log_to_file()  # in-process: fd 1 and 2 are not the log, so none is moved
    handler = service_host._service_log
    assert handler is not None
    log = service_host.log_path()
    try:
        with log.open("a", encoding="utf-8") as other:
            other.write("y" * 400 + "\n")  # a roll is due
        log.parent.chmod(0o500)
        try:
            service_host.roll_log_if_due()  # raised PermissionError: the service ended
            logging.getLogger("rlx-test-roll").warning("still logging after the failed roll")
            service_host.roll_log_if_due()  # tried no more
        finally:
            log.parent.chmod(0o700)
        text = log.read_text(encoding="utf-8")
        assert "still logging after the failed roll" in text
        assert text.count("could not be rolled over") == 1, "said once"
        assert not log.with_name(log.name + ".1").exists(), "nothing rolled"
        assert handler.stream is not None and not handler.stream.closed
    finally:
        root.removeHandler(handler)
        handler.close()
        root.setLevel(level)
        monkeypatch.setattr(service_host, "_service_log", None)


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
    idle budget): the service runs it as a detached child instead (``rlx service sweep``, which
    is no feature, so no daily ping goes with it: #1842), and starts no second one while the
    first still sweeps."""
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
    sweep = ["-m", "aisquare", *service_host.SWEEP_COMMAND]
    assert [command[-len(sweep) :] for command in started] == [sweep, sweep]
    assert service_host.SWEEP_COMMAND == ("service", "sweep")


def test_the_shipment_is_the_clis_detached_one_and_nothing_while_sharing_is_off(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from aisquare.core import sharing

    calls: list[str] = []

    def ship() -> bool:
        calls.append("ship")
        return True

    monkeypatch.setattr(sharing, "start_background_ship", ship)
    part = service_host.ShipPart()

    monkeypatch.setattr(sharing, "enabled", lambda: False)
    assert part.tick_part().state == "off" and calls == []
    monkeypatch.setattr(sharing, "enabled", lambda: True)
    assert part.tick_part().detail == "started a shipment" and calls == ["ship"]
