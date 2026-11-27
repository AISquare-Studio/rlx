"""0.10.3 §1.5 and §1.6: the brain runs ``rlx hook <event>`` as Claude Code's own process would.

A hook used to run in Claude Code's process: its directory, its environment (the role, the
config dir, the agent's ids). The brain runs in rlxd's. These pin that, for each of the six
fleet events, the brain (``rlx brain --stdio``, a process of its own, given the hook's bytes,
directory and allowlisted variables) prints the same bytes and writes the same rows as a fresh
``aisquare hook <event>``, under three environments in turn in one brain: nothing of one
hook, nor of rlxd's own environment, reaches the next. And that the allowlist is complete:
every variable rlx's code names is in it or kept out on purpose, every variable the hooks
read is in it, rlxd's copy is the same list, and the module state the hook path writes is the
state ``services.brain`` audits.
"""

from __future__ import annotations

import base64
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import textwrap
import threading
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

from aisquare.services import brain

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src" / "aisquare"

#: Each fleet event's payload, as Claude Code sends it, for session t1 in ``{cwd}``.
PAYLOADS: dict[str, dict[str, Any]] = {
    "session-start": {"hook_event_name": "SessionStart", "source": "startup"},
    "user-prompt-submit": {"hook_event_name": "UserPromptSubmit", "prompt": "hook-parity-probe"},
    "stop": {"hook_event_name": "Stop", "stop_hook_active": False},
    "notification": {
        "hook_event_name": "Notification",
        "message": "Claude needs your permission",
        "notification_type": "permission_prompt",
    },
    "stop-failure": {
        "hook_event_name": "StopFailure",
        "error": "rate_limit",
        "last_assistant_message": "You've hit your session limit · resets 12:30am (UTC)",
    },
    "session-end": {"hook_event_name": "SessionEnd", "reason": "exit"},
}

#: The store's tables the six hooks write.
TABLES = ("prompt", "metric", "team_event", "team_session")

#: Columns whose values are times, durations or fresh ids, the same nowhere twice.
_MASKED = {"id", "trace_id", "wall_ms", "round_trip_ms", "server_ms"}
_TIME = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")
#: A span from now, such as "resets in 2h 53m" or "0m ago": two runs a minute apart say two.
#: The clock time beside it, "(Fri 00:30)", stays, and with it the zone's parity.
_SPAN = re.compile(r"\b(?:\d+h )?\d+m\b")


def _no_spans(value: Any) -> Any:
    return _SPAN.sub("<span>", value) if isinstance(value, str) else value


def _program(name: str) -> str:
    """The console script ``name`` of this checkout's environment."""
    found = shutil.which(name, path=str(Path(sys.executable).parent))
    assert found, f"no {name} beside {sys.executable}"
    return found


def _payload(event: str, cwd: Path) -> bytes:
    return json.dumps({"session_id": "t1", "cwd": str(cwd), **PAYLOADS[event]}).encode()


@pytest.fixture(autouse=True)
def _no_baseline_of_another_test() -> Iterator[None]:
    """Each test starts with no baseline, as a brain does before its hello. One kept in this
    process (by an in-process ``serve`` of another test, in the same xdist worker) would put
    back, before the next hook, what this test patched: #617's CI lost the three hooks'
    threads and exit handler that way."""
    brain.drop_baseline()
    yield
    brain.drop_baseline()


@pytest.fixture
def template(tmp_path: Path) -> tuple[Path, Path]:
    """A home with a project onboarded and a fact remembered, and the project: what each
    run starts from."""
    home = tmp_path / "template-home"
    project = tmp_path / "proj"
    project.mkdir()
    subprocess.run(["git", "init", "-q", str(project)], check=True)
    env = {**os.environ, "AISQUARE_HOME": str(home)}
    rlx = _program("aisquare")
    onboard = [rlx, "project", "onboard", str(project)]
    subprocess.run(onboard, env=env, check=True, capture_output=True)
    subprocess.run(
        [rlx, "context", "add", "--project", "seed fact for the hook check"],
        env=env,
        cwd=project,
        check=True,
        capture_output=True,
    )
    return home, project


def _copy(template: Path, to: Path) -> Path:
    shutil.copytree(template, to)
    return to


def _rows(home: Path) -> dict[str, list[dict[str, Any]]]:
    """The rows the hooks wrote, their times and fresh ids masked."""
    with sqlite3.connect(home / "context.db") as db:
        db.row_factory = sqlite3.Row
        dumped: dict[str, list[dict[str, Any]]] = {}
        for table in TABLES:
            rows = []
            for row in db.execute(f"SELECT * FROM {table} ORDER BY rowid"):
                rows.append(
                    {
                        key: "<masked>"
                        if key in _MASKED
                        or key.endswith("_at")
                        or (isinstance(row[key], str) and _TIME.match(row[key]))
                        else _no_spans(row[key])
                        for key in row.keys()  # noqa: SIM118 (a Row has no __iter__ of keys)
                    }
                )
            dumped[table] = rows
    return dumped


def _environments(tmp_path: Path) -> dict[str, dict[str, str]]:
    """Five hooks' environments, each its own role, config dir, agent ids and zone; the
    third names none of them, so whatever an earlier one set must be gone; the last two name
    the same relative hub, which every fresh process warns of, so the brain must warn of it
    in both."""
    return {
        "a": {
            "AISQUARE_ROLE": "coder",
            "CLAUDE_CONFIG_DIR": str(tmp_path / "cc-a"),
            "AISQUARE_FLEET_AGENT": "agt_parity_a",
            "AISQUARE_LAUNCH_ID": "lch_parity_a",
            "TZ": "UTC",
        },
        "b": {
            "AISQUARE_ROLE": "manager",
            "CLAUDE_CONFIG_DIR": str(tmp_path / "cc-b"),
            "AISQUARE_FLEET_AGENT": "agt_parity_b",
            "AISQUARE_PERSONA": "persona-b",
            "TZ": "America/New_York",
        },
        "c": {},
        # A relative hub: each ``rlx hook`` warns of it once, as its process starts fresh,
        # so the brain must too (``core.orchestrator._WARNED_HUBS``, put back per hook).
        "d": {"AISQUARE_ROLE": "coder", "AISQUARE_TEAM_HUB": "relative-hub"},
        "e": {"AISQUARE_ROLE": "coder", "AISQUARE_TEAM_HUB": "relative-hub"},
    }


#: What rlxd's own environment holds that no hook of these does: each must stay out.
BRAIN_ONLY = {
    "AISQUARE_ROLE": "reviewer",
    "AISQUARE_FLEET_AGENT": "agt_rlxd",
    "AISQUARE_PERSONA": "rlxd-persona",
    "TZ": "Asia/Kolkata",
}


def _hook_env(base: dict[str, str], home: Path, extra: dict[str, str]) -> dict[str, str]:
    env = {key: value for key, value in base.items() if key not in BRAIN_ONLY}
    env.pop("CLAUDE_CONFIG_DIR", None)
    return {**env, "AISQUARE_HOME": str(home), **extra}


def _normal(text: bytes, home: Path) -> bytes:
    """``text`` with the run's own home, which each run has a copy of, said as ``<home>``,
    and its spans from now masked (:data:`_SPAN`)."""
    for spelling in {str(home.resolve()), str(home)}:
        text = text.replace(spelling.encode(), b"<home>")
    return _SPAN.sub("<span>", text.decode("utf-8", "surrogateescape")).encode(
        "utf-8", "surrogateescape"
    )


class Brain:
    """``rlx brain --stdio``, as rlxd runs it: in rlxd's environment and directory."""

    def __init__(self, env: dict[str, str], cwd: Path, record: Path) -> None:
        driver = textwrap.dedent(
            """
            import json, os, sys
            reads = set()
            here = os.path.normcase(os.path.abspath(sys.argv[1]))

            # A read of one variable, by name. A copy of the whole environment, for a child
            # (`os.environ.copy()`, `.items()`), or the brain's own save and restore, reads
            # every variable and names none.
            WHOLE = ("_collections_abc", "<frozen os>", os.sep + "os.py", "brain.py")

            class Recording(type(os.environ)):
                def _seen(self, key):
                    caller = sys._getframe(2).f_code.co_filename
                    if not caller.endswith(WHOLE) and "_collections_abc" not in caller:
                        reads.add(key)

                def __getitem__(self, key):
                    self._seen(key)
                    return super().__getitem__(key)

                def __contains__(self, key):
                    self._seen(key)
                    return super().__contains__(key)

                def get(self, key, default=None):
                    self._seen(key)
                    return super().get(key, default)

            os.environ.__class__ = Recording
            sys.argv = [sys.argv[2]]
            from aisquare.services import brain

            # The modules the baseline holds, which serve drops as it returns.
            held = set()
            take = brain.take_baseline

            def taking():
                take()
                held.update(brain._BASELINE_MODULES)

            brain.take_baseline = taking
            try:
                code = brain.serve_stdio()
            finally:
                late = sorted(
                    name for name in sys.modules
                    if name.startswith("aisquare") and name not in held
                    and name != "aisquare.services.brain"
                )
                with open(here, "w", encoding="utf-8") as out:
                    out.write(json.dumps({"reads": sorted(reads), "late": late}))
            raise SystemExit(code)
            """
        )
        self.process = subprocess.Popen(
            [sys.executable, "-c", driver, str(record), _program("rlx")],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            env=env,
            cwd=cwd,
        )
        self.next_id = 0
        hello = {
            "type": "hello",
            "protocol": 1,
            "rlxd_version": "test",
            "env_allowlist": list(brain.ENV_NAMES),
        }
        assert self._send(hello)["type"] == "hello"

    def _send(self, frame: dict[str, Any]) -> dict[str, Any]:
        assert self.process.stdin and self.process.stdout
        self.process.stdin.write(json.dumps(frame).encode() + b"\n")
        self.process.stdin.flush()
        line = self.process.stdout.readline()
        assert line, "the brain ended"
        said: dict[str, Any] = json.loads(line)
        return said

    def hook(self, event: str, stdin: bytes, cwd: Path, env: dict[str, str]) -> dict[str, Any]:
        self.next_id += 1
        allowed = {name: value for name, value in env.items() if brain.forwarded(name)}
        params = {
            "event": event,
            "stdin_b64": base64.b64encode(stdin).decode(),
            "cwd": str(cwd),
            "env": allowed,
        }
        accepted = self._send(
            {"type": "request", "id": self.next_id, "method": "hook", "params": params}
        )
        assert accepted == {"type": "accepted", "id": self.next_id}
        assert self.process.stdout
        reply: dict[str, Any] = json.loads(self.process.stdout.readline())
        assert reply["ok"], reply
        result: dict[str, Any] = reply["result"]
        return result

    def close(self) -> int:
        assert self.process.stdin
        self.process.stdin.close()
        return self.process.wait(timeout=60)


@pytest.fixture
def parity(tmp_path: Path, template: tuple[Path, Path]) -> Iterator[dict[str, Any]]:
    """Every event run both ways under each environment: what each printed, and the rows."""
    template_home, project = template
    environments = _environments(tmp_path)
    base = dict(os.environ)
    fresh: dict[str, Any] = {}
    for name, extra in environments.items():
        home = _copy(template_home, tmp_path / f"fresh-{name}")
        env = _hook_env(base, home, extra)
        printed = {}
        for event in PAYLOADS:
            done = subprocess.run(
                [_program("aisquare"), "hook", event],
                input=_payload(event, project),
                env=env,
                cwd=project,
                capture_output=True,
                timeout=120,
                check=False,
            )
            printed[event] = (
                done.returncode,
                _normal(done.stdout, home),
                _normal(done.stderr, home),
            )
        fresh[name] = (printed, _rows(home))

    rlxd_dir = tmp_path / "rlxd-cwd"
    rlxd_dir.mkdir()
    record = tmp_path / "env-reads.txt"
    brain_env = {**base, **BRAIN_ONLY, "CLAUDE_CONFIG_DIR": str(tmp_path / "cc-rlxd")}
    running = Brain(brain_env, rlxd_dir, record)
    through_brain: dict[str, Any] = {}
    try:
        for name, extra in environments.items():
            home = _copy(template_home, tmp_path / f"brain-{name}")
            env = _hook_env(base, home, extra)
            printed = {}
            for event in PAYLOADS:
                result = running.hook(event, _payload(event, project), project, env)
                printed[event] = (
                    result["exit_code"],
                    _normal(base64.b64decode(result["stdout_b64"]), home),
                    _normal(base64.b64decode(result["stderr_b64"]), home),
                )
            through_brain[name] = (printed, _rows(home))
    finally:
        assert running.close() == 0
    said = json.loads(record.read_text(encoding="utf-8"))
    yield {
        "fresh": fresh,
        "brain": through_brain,
        "reads": set(said["reads"]),
        "late": said["late"],
        "project": project,
    }


def test_every_event_prints_the_same_bytes_and_writes_the_same_rows_both_ways(
    parity: dict[str, Any],
) -> None:
    for name in ("a", "b", "c", "d", "e"):
        fresh_printed, fresh_rows = parity["fresh"][name]
        brain_printed, brain_rows = parity["brain"][name]
        for event in PAYLOADS:
            assert brain_printed[event] == fresh_printed[event], (name, event)
        assert brain_rows == fresh_rows, name
    # The checks can fail: what each environment says is its own.
    a_start = parity["fresh"]["a"][0]["session-start"][1]
    assert b"seed fact for the hook check" in a_start
    assert b"role: coder" in a_start
    assert b"role: manager" in parity["fresh"]["b"][0]["session-start"][1]
    rows = {name: parity["fresh"][name][1]["team_session"] for name in ("a", "b", "c")}
    assert rows["a"][0]["role"] == "coder" and rows["b"][0]["role"] == "manager"
    assert rows["a"][0]["account"] != rows["b"][0]["account"]
    assert [row["text"] for row in parity["fresh"]["a"][1]["prompt"]] == ["hook-parity-probe"]
    for name in ("d", "e"):
        start_err = parity["brain"][name][0]["session-start"][2]
        assert b"RELATIVE" in start_err, f"{name}: the brain warns of the hub each time"


def test_the_allowlist_is_what_the_hooks_read_and_nothing_else(parity: dict[str, Any]) -> None:
    read = {
        name.upper()
        for name in parity["reads"] - brain.ENV_OF_THE_BRAIN
        if not name.startswith(brain.ENV_OF_THE_INTERPRETER)
    }
    unlisted = {name for name in read if not brain.forwarded(name)}
    assert not unlisted, f"the hooks read variables the brain is not sent: {sorted(unlisted)}"
    unread = set(brain.ENV_NAMES) - read - set(brain.ENV_READ_ELSEWHERE)
    assert not unread, (
        f"sent, and no hook here read them: {sorted(unread)}; drop them, or say in "
        "ENV_READ_ELSEWHERE where they are read"
    )
    assert set(brain.ENV_READ_ELSEWHERE) <= set(brain.ENV_NAMES)
    assert {"AISQUARE_ROLE", "CLAUDE_CONFIG_DIR", "AISQUARE_FLEET_AGENT", "HOME"} <= read


def test_an_agents_variables_come_from_its_adapter_and_no_credential_goes() -> None:
    from aisquare.core.agent_adapters import adapters

    for adapter in adapters():
        assert set(adapter.hook_env) <= set(brain.ENV_NAMES), adapter.id
    own = {name for adapter in adapters() for name in adapter.hook_env}
    assert {"CLAUDE_CONFIG_DIR", "CLAUDE_EFFORT"} <= own
    for name in (
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_AUTH",
        "CLAUDE_AUTH_HEADER",
        "CLAUDE_CODE_SESSION_COOKIE",
        "CODEX_ACCESS_TOKENS",
        "CLAUDE_CODE_SECRETS",
        "CLAUDE_CODE_CREDENTIAL",
        "CLAUDE_CODE_APIKEY",
        "CLAUDE_CODE_PASSPHRASE",
        "CLAUDE_CODE_PASSWD",
        "CLAUDE_CODE_BEARER",
        "CLAUDE_CODE_OAUTHTOKEN",
        "CODEX_AUTH_JSON",
        "CLAUDEAI_SESSION",
    ):
        assert not brain.forwarded(name), name


def test_rlxd_sends_the_same_list_the_brain_sets() -> None:
    source = (ROOT / "rlxd" / "crates" / "rlxd-proto" / "src" / "hook.rs").read_text(
        encoding="utf-8"
    )

    def listed(constant: str) -> list[str]:
        block = source.split(f"pub const {constant}: &[&str] = &[", 1)[1].split("]", 1)[0]
        return re.findall(r'"([^"]+)"', block)

    assert listed("HOOK_ENV_NAMES") == list(brain.ENV_NAMES)
    assert "HOOK_ENV_PREFIXES" not in source, "exact names only"
    assert list(brain.ENV_NAMES) == sorted(set(brain.ENV_NAMES))


def test_the_hooks_import_no_module_of_rlxs_the_brain_did_not_have(parity: dict[str, Any]) -> None:
    """One imported by a hook after the baseline would keep its state across hooks."""
    assert parity["late"] == []


@pytest.fixture
def baseline() -> Iterator[None]:
    """The brain's baseline, taken in this process for one test, and dropped after it."""
    brain.take_baseline()
    try:
        yield
    finally:
        brain.drop_baseline()


@pytest.mark.usefixtures("baseline")
def test_fresh_state_puts_rlxs_modules_back_as_a_fresh_process_has_them() -> None:
    from aisquare.core import launch_seal, orchestrator, program, state

    seen_before = dict(launch_seal._SEEN)
    original_state = state.get_state()
    program.word()
    launch_seal._SEEN["a-session"] = object()  # type: ignore[assignment]
    orchestrator._WARNED_HUBS.add("relative-hub")
    program._in_app = True
    state._state = state.RuntimeState(verbose=True)
    vars(orchestrator)["_ADDED_BY_A_HOOK"] = 1
    try:
        assert brain.fresh_state() >= 5
        assert seen_before == launch_seal._SEEN
        assert "relative-hub" not in orchestrator._WARNED_HUBS
        assert program._in_app is False
        assert state.get_state() is original_state
        assert not hasattr(orchestrator, "_ADDED_BY_A_HOOK")
        assert program.word.cache_info().currsize == 0
        for (name, attribute), (value, kept) in brain._BASELINE.items():
            assert vars(sys.modules[name]).get(attribute) is value, (name, attribute)
            if kept is not brain._PLAIN:
                assert value == kept, (name, attribute)
        assert brain.fresh_state() == 0, "nothing more to put back"
    finally:
        launch_seal._SEEN.clear()
        launch_seal._SEEN.update(seen_before)
        orchestrator._WARNED_HUBS.discard("relative-hub")
        program._in_app = False
        state.set_state(original_state)
        vars(orchestrator).pop("_ADDED_BY_A_HOOK", None)


def test_every_exit_handler_rlxs_modules_register_is_run_after_a_hook() -> None:
    registered: set[tuple[str, str]] = set()
    pattern = re.compile(r"^atexit\.register\((\w+)\)|^@atexit\.register\s*\ndef (\w+)", re.M)
    for path in SRC.rglob("*.py"):
        text = path.read_text(encoding="utf-8")
        module = "aisquare." + ".".join(path.relative_to(SRC).with_suffix("").parts)
        for found in pattern.finditer(text):
            registered.add((module, found.group(1) or found.group(2)))
    assert registered == set(brain.EXIT_HANDLERS)


def _children() -> set[int]:
    """This process's children, by pid (POSIX)."""
    listed = subprocess.run(
        ["pgrep", "-P", str(os.getpid())], capture_output=True, text=True, check=False
    )
    return {int(pid) for pid in listed.stdout.split()}


def test_nothing_a_hook_starts_outlives_its_request_but_a_detached_worker(
    tmp_path: Path, template: tuple[Path, Path]
) -> None:
    """Each event, run by the brain's own ``hook`` in this process: no non-daemon thread it
    started is left, and every child still running is in a session of its own, a worker
    ``services.hooks`` detaches on purpose, as it would from ``rlx hook``."""
    template_home, project = template
    home = _copy(template_home, tmp_path / "in-process")
    env = {name: value for name, value in os.environ.items() if brain.forwarded(name)}
    env.update({"AISQUARE_HOME": str(home), "AISQUARE_ROLE": "coder"})
    for event in PAYLOADS:
        threads = {thread for thread in threading.enumerate() if not thread.daemon}
        children = set() if sys.platform == "win32" else _children()
        result = brain.METHODS["hook"](
            {
                "event": event,
                "stdin_b64": base64.b64encode(_payload(event, project)).decode(),
                "cwd": str(project),
                "env": env,
            }
        )
        assert result["exit_code"] == 0, base64.b64decode(result["stderr_b64"])
        left = {thread for thread in threading.enumerate() if not thread.daemon} - threads
        assert not left, f"{event} left threads running: {left}"
        if sys.platform != "win32":  # Windows has no sessions to tell a detached worker by
            for pid in _children() - children:
                assert os.getsid(pid) == pid, f"{event} left child {pid} in the brain's session"
    assert os.environ.get("AISQUARE_ROLE") != "coder", "the hook's environment was put back"


def test_the_brain_waits_for_a_thread_the_hook_started_and_keeps_what_it_printed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """As a process waits for its non-daemon threads before it exits: the hook's reply
    comes once the thread is done, with what it printed."""
    import time

    from aisquare.services import agent_events

    handle = agent_events.handle

    def handle_and_leave_a_thread(*args: Any, **kwargs: Any) -> str:
        def late() -> None:
            time.sleep(0.3)
            print("printed by a thread the hook started", flush=True)

        threading.Thread(target=late, name="hook-leftover").start()
        return handle(*args, **kwargs)

    monkeypatch.setattr(agent_events, "handle", handle_and_leave_a_thread)
    result = brain.METHODS["hook"](
        {
            "event": "stop",
            "stdin_b64": base64.b64encode(_payload("stop", tmp_path)).decode(),
            "cwd": str(tmp_path),
            "env": {"AISQUARE_HOME": str(tmp_path / "home")},
        }
    )
    assert not [thread for thread in threading.enumerate() if thread.name == "hook-leftover"]
    assert b"printed by a thread the hook started" in base64.b64decode(result["stdout_b64"])


def _stop_hook(tmp_path: Path) -> dict[str, Any]:
    """The brain's own ``hook`` for a Stop, in this process."""
    result: dict[str, Any] = brain.METHODS["hook"](
        {
            "event": "stop",
            "stdin_b64": base64.b64encode(_payload("stop", tmp_path)).decode(),
            "cwd": str(tmp_path),
            "env": {"AISQUARE_HOME": str(tmp_path / "home")},
        }
    )
    return result


def test_an_exit_handler_a_hook_registers_runs_at_its_end_as_its_own(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """As at a process's exit, and not at the brain's: what it prints is the hook's, and
    the brain's own exit handlers stay as they were."""
    import atexit

    from aisquare.services import agent_events

    handle = agent_events.handle

    def handle_and_register(*args: Any, **kwargs: Any) -> str:
        atexit.register(print, "said by the hook's exit handler")
        return handle(*args, **kwargs)

    monkeypatch.setattr(agent_events, "handle", handle_and_register)
    before = atexit._ncallbacks()
    result = _stop_hook(tmp_path)
    assert b"said by the hook's exit handler" in base64.b64decode(result["stdout_b64"])
    assert atexit._ncallbacks() == before


def test_a_daemon_thread_the_hook_started_is_given_time_to_end(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import time

    from aisquare.services import agent_events

    handle = agent_events.handle

    def handle_and_leave_a_daemon(*args: Any, **kwargs: Any) -> str:
        def late() -> None:
            time.sleep(0.3)
            print("printed by a daemon thread of the hook's", flush=True)

        threading.Thread(target=late, name="hook-daemon", daemon=True).start()
        return handle(*args, **kwargs)

    monkeypatch.setattr(agent_events, "handle", handle_and_leave_a_daemon)
    result = _stop_hook(tmp_path)
    assert not [thread for thread in threading.enumerate() if thread.name == "hook-daemon"]
    assert b"printed by a daemon thread" in base64.b64decode(result["stdout_b64"])


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX: Windows keeps no zombies")
def test_a_hook_collects_no_child_another_still_waits_on(tmp_path: Path) -> None:
    import time

    held = subprocess.Popen([sys.executable, "-c", "raise SystemExit(7)"])
    # It ends, and stays a zombie for its holder to reap.
    deadline = time.monotonic() + 10
    while (
        "Z"
        not in subprocess.run(
            ["ps", "-o", "stat=", "-p", str(held.pid)], capture_output=True, text=True, check=False
        ).stdout
    ):
        assert time.monotonic() < deadline, "the child never ended"
        time.sleep(0.05)
    _stop_hook(tmp_path)
    assert held.wait(timeout=10) == 7, "the hook took its status"


@pytest.mark.skipif(sys.platform == "win32", reason="lists its fds in /dev/fd")
def test_a_hook_that_cannot_capture_its_output_leaks_no_fd(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import tempfile

    made = tempfile.TemporaryFile
    calls: list[int] = []

    def second_fails(*args: Any, **kwargs: Any) -> Any:
        calls.append(1)
        if len(calls) == 2:
            raise OSError("no room")
        return made(*args, **kwargs)

    def open_fds() -> set[str]:
        return set(os.listdir("/dev/fd"))

    monkeypatch.setattr(tempfile, "TemporaryFile", second_fails)
    before = open_fds()
    with pytest.raises(OSError, match="no room"), brain._as_the_hook({}, None, ["x"], b""):
        pass
    assert open_fds() <= before


def test_on_windows_a_detached_start_asks_to_leave_the_brains_job() -> None:
    breakaway = 0x01000000
    flags: list[int] = []
    refuse_first = [True]

    def popen(*args: Any, **kwargs: Any) -> str:
        flags.append(kwargs.get("creationflags") or 0)
        if refuse_first[0] and flags[-1] & breakaway:
            refuse_first[0] = False
            raise PermissionError("a job above allows no breakaway")
        return "started"

    start = brain.breaking_away(popen)
    refuse_first[0] = False
    assert start(["worker"], start_new_session=True, creationflags=0x200) == "started"
    assert flags[-1] == 0x200 | breakaway
    assert start(["git", "status"]) == "started"
    assert flags[-1] == 0, "only a detached start asks"
    refuse_first[0] = True
    assert start(["worker"], start_new_session=True) == "started"
    assert flags[-2:] == [breakaway, 0], "refused, it is started in the brain's job"
