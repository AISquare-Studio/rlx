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

import ast
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
    """Three hooks' environments, each its own role, config dir, agent ids and zone; the
    third names none of them, so whatever an earlier one set must be gone."""
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
            import os, sys
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
            try:
                code = brain.serve_stdio()
            finally:
                with open(here, "w", encoding="utf-8") as out:
                    out.write("\\n".join(sorted(reads)))
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
            "env_prefixes": list(brain.ENV_PREFIXES),
            "env_secret_words": list(brain.ENV_SECRET_WORDS),
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
    yield {
        "fresh": fresh,
        "brain": through_brain,
        "reads": set(record.read_text(encoding="utf-8").split()),
        "project": project,
    }


def test_every_event_prints_the_same_bytes_and_writes_the_same_rows_both_ways(
    parity: dict[str, Any],
) -> None:
    for name in ("a", "b", "c"):
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


def test_every_variable_the_hooks_read_is_in_the_allowlist(parity: dict[str, Any]) -> None:
    unlisted = {
        name
        for name in parity["reads"] - brain.ENV_OF_THE_BRAIN
        if not brain.forwarded(name) and not name.startswith(brain.ENV_OF_THE_INTERPRETER)
    }
    assert not unlisted, f"the hooks read variables the brain is not sent: {sorted(unlisted)}"
    assert {"AISQUARE_ROLE", "CLAUDE_CONFIG_DIR", "AISQUARE_FLEET_AGENT"} <= parity["reads"]


def test_every_variable_rlxs_code_names_is_forwarded_or_kept_out_on_purpose() -> None:
    names = (
        r"AISQUARE_[A-Z0-9_]*[A-Z0-9]",
        r"RLX_[A-Z0-9_]*[A-Z0-9]",
        r"CLAUDE[A-Z0-9_]*[A-Z0-9]",
        r"CODEX_[A-Z0-9_]*[A-Z0-9]",
        r"TMUX[A-Z0-9_]*",
    )
    named = re.compile(r"[\"'](" + "|".join(names) + r")[\"']")
    found: set[str] = set()
    for path in SRC.rglob("*.py"):
        found |= set(named.findall(path.read_text(encoding="utf-8")))
    assert len(found) > 50
    missing = {
        name for name in found if not brain.forwarded(name) and not brain.is_credential(name)
    }
    assert not missing, f"name these in services.brain's allowlist, or keep them out: {missing}"
    # A credential is kept out, and the rule says so of the one rlx's code names.
    assert brain.is_credential("CLAUDE_CODE_OAUTH_TOKEN")
    assert not brain.forwarded("CLAUDE_CODE_OAUTH_TOKEN")


def test_rlxd_sends_the_same_list_the_brain_sets() -> None:
    source = (ROOT / "rlxd" / "crates" / "rlxd-proto" / "src" / "hook.rs").read_text(
        encoding="utf-8"
    )

    def listed(constant: str) -> list[str]:
        block = source.split(f"pub const {constant}: &[&str] = &[", 1)[1].split("]", 1)[0]
        return re.findall(r'"([^"]+)"', block)

    assert listed("HOOK_ENV_NAMES") == list(brain.ENV_NAMES)
    assert listed("HOOK_ENV_PREFIXES") == list(brain.ENV_PREFIXES)
    assert listed("HOOK_ENV_SECRET_WORDS") == list(brain.ENV_SECRET_WORDS)
    assert list(brain.ENV_NAMES) == sorted(set(brain.ENV_NAMES))


def test_the_module_state_the_hook_path_writes_is_the_state_the_brain_audits() -> None:
    """Every ``global`` assignment in a module the hook path loads is in ``MODULE_STATE``."""
    loaded = subprocess.run(
        [
            sys.executable,
            "-c",
            "import sys, aisquare.cli.app, aisquare.services.brain;"
            "print('\\n'.join(n for n in sys.modules if n.startswith('aisquare')))",
        ],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split()
    written: set[tuple[str, str]] = set()
    for name in loaded:
        parts = name.split(".")[1:]
        candidates = [SRC.joinpath(*parts, "__init__.py")]
        if parts:
            candidates.append(SRC.joinpath(*parts[:-1], parts[-1] + ".py"))
        path = next((candidate for candidate in candidates if candidate.exists()), None)
        if path is None:
            continue
        tree = ast.parse(path.read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if isinstance(node, ast.Global):
                written |= {(name, target) for target in node.names}
    unaudited = written - set(brain.MODULE_STATE)
    assert not unaudited, f"audit these in services.brain (MODULE_STATE and its doc): {unaudited}"
    assert ("aisquare.core.state", "_state") in written


def test_fresh_caches_empties_every_functools_cache_of_rlx() -> None:
    from aisquare.core import program

    program.word()
    assert program.word.cache_info().currsize == 1
    assert brain.fresh_caches() >= 1
    assert program.word.cache_info().currsize == 0


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
