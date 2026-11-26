"""The anonymous daily active ping (``services.active_ping``), against a stub of its endpoint.

The stub is a real socket, as ``tests/stub_ci_server.py`` is: what is proven here is what goes
over the wire (the body, the headers, the answers) and what the client does with each answer.
The endpoint itself is AISquare-Studio-BE's (``docs/handoffs/rlx-active-installs.md``); its path
and body are this contract.

Commands are run through the CLI. The detached pinger they start is run inline (``_spawn`` is
``send``) so that each test sees its ping before it goes on, except in the one test that starts
the real detached process to prove the command never waits on the server.
"""

from __future__ import annotations

import json
import os
import platform
import re
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli import fleet as fleet_cli
from aisquare.cli.app import app
from aisquare.core import credentials, sharing
from aisquare.services import active_ping, iam, usage_share
from aisquare.services import settings as settings_service

pytestmark = pytest.mark.usefixtures("sharing_allowed")

REAL_SPAWN = active_ping._spawn
"""The real detached start, taken before the suite's autouse fixture replaces it."""

FIVE_KEYS = {"date", "version", "os", "arch", "install_method"}
#: The headers any HTTP client sends with a JSON POST; anything else would be the ping saying
#: something more than its body.
PLAIN_HEADERS = {
    "host",
    "accept",
    "accept-encoding",
    "connection",
    "content-type",
    "content-length",
    "user-agent",
}
ZONE = timezone(timedelta(hours=5, minutes=30))
"""The local zone of the tests' days, whatever the runner's."""
DAY_ONE = datetime(2026, 11, 25, 4, 0, tzinfo=UTC)  # 09:30 local
COMMAND = ["config", "get", "profile"]
"""A command a person runs: a feature, so a day it runs on is a day rlx is used."""


# --- the stub endpoint ----------


@dataclass
class Seen:
    path: str
    headers: dict[str, str]
    raw: bytes

    @property
    def body(self) -> dict[str, Any]:
        loaded: dict[str, Any] = json.loads(self.raw)
        return loaded


@dataclass
class Stub:
    """What the endpoint answers next, and every request it saw."""

    url: str = ""
    status: int = 204
    headers: dict[str, str] = field(default_factory=dict)
    hold: threading.Event | None = None
    """Set: the answer waits until the event is set (a server that does not answer)."""
    seen: list[Seen] = field(default_factory=list)

    def answer(self, status: int, **headers: str) -> None:
        self.status = status
        self.headers = {name.replace("_", "-"): value for name, value in headers.items()}


class _Handler(BaseHTTPRequestHandler):
    server: _Server

    def do_POST(self) -> None:
        stub = self.server.stub
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        headers = {name.lower(): value for name, value in self.headers.items()}
        stub.seen.append(Seen(path=self.path, headers=headers, raw=raw))
        if stub.hold is not None:
            stub.hold.wait(60)
        self.send_response(stub.status)
        for name, value in stub.headers.items():
            self.send_header(name, value)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, format: str, *args: Any) -> None:
        return


class _Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, stub: Stub) -> None:
        super().__init__(("127.0.0.1", 0), _Handler)
        self.stub = stub

    def handle_error(self, request: Any, client_address: Any) -> None:
        # A client that gave up (the timeout test) leaves a broken pipe behind: not an error.
        if isinstance(sys.exc_info()[1], (BrokenPipeError, ConnectionResetError)):
            return
        super().handle_error(request, client_address)


@pytest.fixture
def stub(monkeypatch: pytest.MonkeyPatch) -> Iterator[Stub]:
    found = Stub()
    server = _Server(found)
    thread = threading.Thread(target=server.serve_forever, args=(0.05,), daemon=True)
    thread.start()
    found.url = f"http://127.0.0.1:{server.server_address[1]}"
    monkeypatch.setenv(iam.API_URL_ENV_VAR, found.url)
    try:
        yield found
    finally:
        if found.hold is not None:
            found.hold.set()
        server.shutdown()
        server.server_close()


# --- the clock and the pinger ----------


@dataclass
class Clock:
    now: datetime = DAY_ONE

    def advance(self, by: timedelta) -> None:
        self.now += by


@pytest.fixture
def clock(monkeypatch: pytest.MonkeyPatch) -> Clock:
    held = Clock()
    monkeypatch.setattr(active_ping, "_now", lambda: held.now)
    monkeypatch.setattr(active_ping, "_tz", lambda: ZONE)
    return held


@dataclass
class Starts:
    count: int = 0


@pytest.fixture
def inline(monkeypatch: pytest.MonkeyPatch) -> Starts:
    """The detached pinger, run in place: a command's ping has gone when the command returns."""
    starts = Starts()

    def run_it() -> None:
        starts.count += 1
        active_ping.send()

    monkeypatch.setattr(active_ping, "_spawn", run_it)
    return starts


def run(*args: str) -> str:
    result = CliRunner().invoke(app, list(args or COMMAND))
    assert result.exit_code == 0, result.output
    return result.output


def until(condition: Callable[[], bool], *, within: float = 30.0) -> bool:
    """Poll ``condition`` until it holds or ``within`` seconds have passed."""
    deadline = time.monotonic() + within
    while time.monotonic() < deadline:
        if condition():
            return True
        time.sleep(0.02)
    return condition()


# --- what it sends ----------


def test_the_body_is_exactly_the_five_keys_of_the_contract(
    stub: Stub, clock: Clock, inline: Starts
) -> None:
    run()

    assert len(stub.seen) == 1
    seen = stub.seen[0]
    assert seen.path == "/api/v1/rlx/active/"
    body = seen.body
    assert set(body) == FIVE_KEYS
    assert all(isinstance(value, str) for value in body.values())
    assert body["date"] == "2026-11-25"  # the LOCAL day (09:30 in ZONE)
    assert re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+[0-9a-z.+]{0,20}", body["version"])
    assert body["os"] in {"darwin", "linux", "windows", "other"}
    assert body["arch"] in {"x86_64", "arm64", "other"}
    assert body["install_method"] in {"brew", "pipx", "uv", "script", "pip", "unknown"}
    assert seen.headers["content-type"] == "application/json"


def test_the_request_carries_no_token_no_id_and_no_header_beyond_a_plain_post(
    stub: Stub, clock: Clock, inline: Starts, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Signed in to AISquare and sharing on, with an installation id: none of it goes."""
    monkeypatch.setenv(iam.TOKEN_ENV_VAR, "aisq_the-secret-token")
    credentials.store(**{iam.KEY_API_URL: stub.url, iam.KEY_TOKEN: "aisq_the-stored-token"})
    usage_share.enable()
    install_id = sharing.read().install_id
    assert install_id is not None, "the premise: this install has an id to leak"

    run()

    assert len(stub.seen) == 1
    seen = stub.seen[0]
    assert set(seen.headers) <= PLAIN_HEADERS, set(seen.headers) - PLAIN_HEADERS
    assert "authorization" not in seen.headers and "cookie" not in seen.headers
    wire = seen.raw.decode() + json.dumps(seen.headers)
    for secret in ("aisq_the-secret-token", "aisq_the-stored-token", install_id):
        assert secret not in wire


@pytest.mark.parametrize(
    ("system", "machine", "os_", "arch"),
    [
        ("Darwin", "arm64", "darwin", "arm64"),
        ("Linux", "x86_64", "linux", "x86_64"),
        ("Linux", "aarch64", "linux", "arm64"),
        ("Windows", "AMD64", "windows", "x86_64"),
        ("FreeBSD", "riscv64", "other", "other"),
        ("Linux-alice-laptop", "i686", "other", "other"),
    ],
)
def test_the_platform_is_sent_in_the_contracts_closed_sets(
    monkeypatch: pytest.MonkeyPatch, system: str, machine: str, os_: str, arch: str
) -> None:
    monkeypatch.setattr(platform, "system", lambda: system)
    monkeypatch.setattr(platform, "machine", lambda: machine)

    body = active_ping.body(DAY_ONE)

    assert (body["os"], body["arch"]) == (os_, arch)


@pytest.mark.parametrize(
    ("installed", "sent"),
    [
        ("0.10.2", "0.10.2"),
        ("0.11.0rc1", "0.11.0rc1"),
        ("0.0.0+uninstalled", "0.0.0+uninstalled"),
        ("0.10", "unknown"),
        ("0.10.2 alice", "unknown"),
        ("0.10.2+" + "x" * 40, "unknown"),
    ],
)
def test_the_version_is_sent_only_in_its_pattern(
    monkeypatch: pytest.MonkeyPatch, installed: str, sent: str
) -> None:
    from aisquare.core import version

    monkeypatch.setattr(version, "__version__", installed)

    assert active_ping.body(DAY_ONE)["version"] == sent


# --- once a day ----------


def test_three_commands_on_one_local_day_send_one_ping_and_a_new_day_sends_another(
    stub: Stub, clock: Clock, inline: Starts
) -> None:
    for _ in range(3):
        run()
        clock.advance(timedelta(hours=3))  # 09:30, 12:30, 15:30 local

    assert len(stub.seen) == 1
    assert inline.count == 1  # the later commands start no pinger at all

    clock.now = DAY_ONE + timedelta(days=1)
    run()
    run()

    assert [seen.body["date"] for seen in stub.seen] == ["2026-11-25", "2026-11-26"]


def test_with_usage_sharing_off_the_ping_still_goes(
    stub: Stub, clock: Clock, inline: Starts
) -> None:
    usage_share.record_consent("declined")
    assert not sharing.switch_on(), "the premise: sharing is off, and was said no to"

    run()

    assert len(stub.seen) == 1


def test_the_ping_switch_off_sends_nothing(stub: Stub, clock: Clock, inline: Starts) -> None:
    run("config", "set", "sharing.active_ping", "false")

    for _ in range(3):
        run()
        clock.advance(timedelta(days=1))

    assert stub.seen == [] and inline.count == 0
    assert not active_ping.status().enabled


@pytest.mark.parametrize(
    ("variable", "value"),
    [("DO_NOT_TRACK", "1"), ("AISQUARE_NO_TELEMETRY", "1"), ("CI", "true")],
)
def test_each_variable_that_forces_sharing_off_stops_the_ping_too(
    stub: Stub,
    clock: Clock,
    inline: Starts,
    monkeypatch: pytest.MonkeyPatch,
    variable: str,
    value: str,
) -> None:
    monkeypatch.setenv(variable, value)

    run()
    assert active_ping.send().skipped == "the ping is off"  # the pinger itself checks again

    assert stub.seen == [] and inline.count == 0


def test_opening_the_app_is_a_day_of_use(
    stub: Stub, clock: Clock, inline: Starts, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(fleet_cli, "interactive_terminal", lambda: True)
    monkeypatch.setattr(fleet_cli, "ui", lambda: None)

    CliRunner().invoke(app, [])

    assert len(stub.seen) == 1


def test_hooks_and_the_detached_senders_never_start_a_ping(
    stub: Stub, clock: Clock, inline: Starts
) -> None:
    runner = CliRunner()
    runner.invoke(app, ["hook", "session-end"], input="{}")
    runner.invoke(app, ["hook", "user-prompt-submit"], input="not json at all")
    runner.invoke(app, ["metrics", "ship"])

    assert inline.count == 0 and stub.seen == []


def test_the_rlx_service_never_starts_a_ping(
    stub: Stub, clock: Clock, inline: Starts, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The ruling on #32 (Q2): the daily active ping stays out of the rlx service, which is up
    every day whether anyone opened rlx or not. Its sweep ran `metrics stats`, a counted
    command, whose close sent the ping (#1842). Here SweepPart's own argv runs through the CLI
    as its child runs it, beside `service run` and `service status`: no ping, and no feature
    counted. The control is the old argv, which does both.

    The sweep itself runs, over the suite's temp agent home, with the Keychain stubbed on top of
    the suite's own stub: a scratch home does not scope the Keychain, and a sweep read Claude
    Code's real item (tester note #1844). A read that reached it would be recorded here."""
    from types import SimpleNamespace

    from aisquare.cli import app as cli_app
    from aisquare.core import claude_accounts as accounts_core
    from aisquare.core import feature_use
    from aisquare.services import service_host

    counted: list[str] = []
    monkeypatch.setattr(cli_app, "_launch_usage_shared", lambda: True)  # a counted one records
    monkeypatch.setattr(feature_use, "record", counted.append)
    keychain: list[tuple[str, str]] = []

    def no_keychain(service: str, account: str, *, fresh: bool = False) -> Any:
        keychain.append((service, account))
        return accounts_core.KeychainRead(found=False)

    monkeypatch.setattr(accounts_core, "read_keychain", no_keychain)
    monkeypatch.setattr(accounts_core, "keychain_platform", lambda: False)
    monkeypatch.setattr(accounts_core, "KEYCHAIN_TOOL", os.devnull)
    monkeypatch.setattr(service_host, "log_to_file", lambda: None)
    monkeypatch.setattr(service_host, "run_service", lambda **_: None)  # the process, ended
    started: list[list[str]] = []

    def detached(command: list[str]) -> object:
        started.append(list(command))
        return SimpleNamespace(pid=4242, poll=lambda: None)

    monkeypatch.setattr(service_host, "_detached", detached)
    service_host.SweepPart().tick_part()
    (argv,) = started
    words = argv[argv.index("aisquare") + 1 :]  # what follows `python -P -m aisquare`

    runner = CliRunner()
    for command in (words, ["service", "run"], ["service", "status"]):
        ran = runner.invoke(app, command)
        assert ran.exception is None or isinstance(ran.exception, SystemExit), ran.output

    assert words == list(service_host.SWEEP_COMMAND)
    assert inline.count == 0 and stub.seen == [] and counted == []
    assert keychain == [], "the sweep read the Keychain"

    runner.invoke(app, ["--json", "metrics", "stats", "--range", "today"])  # the old argv
    assert len(stub.seen) == 1 and counted == ["cli.metrics.stats"], "the control: it pings"


def test_the_cli_loads_the_ping_module_only_when_it_runs_it() -> None:
    """Every hook imports ``cli/app.py``; the ping costs them no import."""
    result = subprocess.run(
        [sys.executable, "-c", "import sys, aisquare.cli.app; print(' '.join(sys.modules))"],
        capture_output=True,
        text=True,
        check=True,
    )
    assert "aisquare.services.active_ping" not in result.stdout.split()


# --- what each answer does ----------


def test_a_404_sends_nothing_more_that_day_prints_nothing_and_tries_the_next_day(
    stub: Stub, clock: Clock, inline: Starts, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("AISQUARE_NO_TELEMETRY", "1")
    quiet = run()  # what the command prints with no ping at all
    monkeypatch.delenv("AISQUARE_NO_TELEMETRY")
    stub.answer(404)

    outputs = []
    for _ in range(3):
        outputs.append(run())
        clock.advance(timedelta(hours=2))

    assert len(stub.seen) == 1
    assert outputs == [quiet] * 3
    assert active_ping.read().outcome == "absent"

    clock.now = DAY_ONE + timedelta(days=1)
    run()

    assert len(stub.seen) == 2


def test_a_429_sends_nothing_before_its_retry_after(
    stub: Stub, clock: Clock, inline: Starts
) -> None:
    stub.answer(429, Retry_After="3600")
    run()
    stub.answer(204)

    clock.advance(timedelta(minutes=59))
    run()
    assert len(stub.seen) == 1  # the backoff alone (5 min) would have sent it again

    clock.advance(timedelta(minutes=2))
    run()
    assert len(stub.seen) == 2
    assert active_ping.read().outcome == "sent"

    clock.advance(timedelta(hours=1))
    run()
    assert len(stub.seen) == 2


@pytest.mark.parametrize("code", [408, 425])
def test_a_408_or_425_waits_for_its_retry_after_too(
    stub: Stub, clock: Clock, inline: Starts, code: int
) -> None:
    """BE#4004's contract: a timeout (408) and too-early (425) are the server's moment, like a
    throttle, and their ``Retry-After`` is honoured (tester of #22)."""
    stub.answer(code, Retry_After="3600")
    run()
    stub.answer(204)

    clock.advance(timedelta(minutes=59))
    run()
    assert len(stub.seen) == 1

    clock.advance(timedelta(minutes=2))
    run()
    assert len(stub.seen) == 2 and active_ping.read().outcome == "sent"


def test_a_huge_retry_after_waits_the_cap_and_never_overflows(
    stub: Stub, clock: Clock, inline: Starts
) -> None:
    """10^14 s overflowed ``timedelta``: the error was swallowed, no wait was recorded, and the
    next command pinged again 30 minutes later (tester of #22). It waits the 24 h cap."""
    stub.answer(429, Retry_After="100000000000000")
    run()
    stub.answer(204)

    state = active_ping.read()
    assert state.not_before == (DAY_ONE + active_ping.RETRY_AFTER_CAP).isoformat(timespec="seconds")
    clock.advance(timedelta(minutes=31))
    run()
    clock.advance(timedelta(hours=23))
    run()
    assert len(stub.seen) == 1, "nothing before the cap, the next local day included"

    clock.advance(timedelta(minutes=30))  # just past 24 h
    run()
    assert len(stub.seen) == 2
    assert stub.seen[1].body["date"] == "2026-11-26"


def test_a_malformed_retry_after_keeps_the_backoff(
    stub: Stub, clock: Clock, inline: Starts
) -> None:
    stub.answer(429, Retry_After="soon, maybe")
    run()
    stub.answer(204)

    clock.advance(timedelta(minutes=4))
    run()
    assert len(stub.seen) == 1, "inside the 5 minutes' backoff"

    clock.advance(timedelta(minutes=2))
    run()
    assert len(stub.seen) == 2


def test_a_503_tries_again_later_the_same_day(stub: Stub, clock: Clock, inline: Starts) -> None:
    stub.answer(503, Retry_After="600")
    run()
    stub.answer(204)

    clock.advance(timedelta(minutes=9))
    run()
    clock.advance(timedelta(minutes=2))
    run()

    assert [seen.body["date"] for seen in stub.seen] == ["2026-11-25", "2026-11-25"]
    assert active_ping.read().outcome == "sent"


def test_a_400_is_not_tried_again_that_day(stub: Stub, clock: Clock, inline: Starts) -> None:
    stub.answer(400)
    run()
    stub.answer(204)

    for _ in range(3):
        clock.advance(timedelta(hours=2))
        run()
    assert len(stub.seen) == 1
    assert active_ping.read().outcome == "refused"

    clock.now = DAY_ONE + timedelta(days=1)
    run()
    assert len(stub.seen) == 2


def test_a_failure_backs_off_and_a_missed_day_is_never_sent_late(
    stub: Stub, clock: Clock, inline: Starts
) -> None:
    clock.now = datetime(2026, 11, 25, 18, 25, tzinfo=UTC)  # 23:55 local
    stub.answer(500)
    run()
    stub.answer(204)

    clock.advance(timedelta(minutes=3))  # inside the 5 minutes' backoff
    run()
    assert len(stub.seen) == 1

    clock.advance(timedelta(minutes=10))  # 00:08 local, the next day
    run()

    assert [seen.body["date"] for seen in stub.seen] == ["2026-11-25", "2026-11-26"]


def test_a_redirect_is_not_followed(stub: Stub, clock: Clock, inline: Starts) -> None:
    stub.answer(302, Location="http://127.0.0.1:9/elsewhere")

    run()

    assert len(stub.seen) == 1
    state = active_ping.read()
    assert state.day is None and state.failures == 1  # a failure, tried again later today


# --- never in the way ----------


def test_a_server_that_never_answers_does_not_hold_the_command(
    stub: Stub, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The real detached pinger: the command returns at once, and the ping goes all the same,
    from a process of its own."""
    monkeypatch.setattr(active_ping, "_spawn", REAL_SPAWN)
    stub.hold = threading.Event()

    started = time.monotonic()
    result = CliRunner().invoke(app, COMMAND)
    elapsed = time.monotonic() - started

    assert result.exit_code == 0, result.output
    assert elapsed < active_ping.TIMEOUT_S, elapsed  # it never waited on the server
    assert until(lambda: len(stub.seen) == 1), "the premise: the ping went, held by the stub"
    stub.hold.set()
    assert until(lambda: active_ping.read().day is not None), "the pinger finished"


def test_the_pinger_gives_up_at_its_timeout(
    stub: Stub, clock: Clock, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(active_ping, "TIMEOUT_S", 0.5)
    stub.hold = threading.Event()

    started = time.monotonic()
    result = active_ping.send()
    elapsed = time.monotonic() - started

    assert elapsed < 4.0, elapsed
    assert not result.sent and result.retry_at is not None
    assert len(stub.seen) == 1


def test_a_settings_write_turns_it_off_and_on(clock: Clock) -> None:
    assert active_ping.status().enabled  # on unless turned off

    assert not active_ping.set_enabled(False).switch
    assert settings_service.get_value("sharing.active_ping") in (False, "false")
    assert active_ping.set_enabled(True).enabled
