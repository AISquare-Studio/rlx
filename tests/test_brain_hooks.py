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
import sysconfig
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

#: Where each environment's transcript is in its home: the Stop reads it for the turn's
#: usage, and its path names the account slot it ran on. A managed slot's folder names its
#: slot itself; the plain home is the one the plain copies name (``plain_vars``), so a hook
#: not sent them records another slot (tester round 3, N2).
TRANSCRIPT = "transcripts/t1.jsonl"
SLOT_TRANSCRIPT = "claude-accounts/2/projects/-proj/t1.jsonl"
PLAIN_HOME = "plain-claude"
PLAIN_TRANSCRIPT = f"{PLAIN_HOME}/projects/-proj/t1.jsonl"

#: Columns whose values are times, durations or fresh ids, the same nowhere twice.
_MASKED = {"id", "trace_id", "wall_ms", "round_trip_ms", "server_ms"}
_TIME = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")
#: A span from now, in every shape ``claude_accounts.format_reset`` and ``format_age`` write
#: one: "12m", "3h", "2h 53m", "2d", "2d 4h", "0m ago". Two runs a minute apart say two: on a
#: whole hour the fresh run said "resets in 3h" and the brain's, a minute later, "2h 59m"
#: (ci.yml run 46066195807). The clock time beside it, "(Fri 00:30)", stays, and with it
#: the zone's parity.
_SPAN = re.compile(r"\b(?:\d+d(?: \d+h)?|\d+h(?: \d+m)?|\d+m)\b")


#: Now, as a time inside a value (a JSON record's ``"at"``): fractional seconds, as a clock
#: reading has and a computed time (a reset's ``"until"``) does not.
_NOW_IN = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+(?:[+-]\d{2}:\d{2}|Z)?")
#: Now, as seconds since the epoch.
_EPOCH = re.compile(r"^\d{10}\.\d+$")


def _no_spans(value: Any) -> Any:
    return _SPAN.sub("<span>", value) if isinstance(value, str) else value


#: Windows' paths are one path whatever the case of their letters (``os.path.normcase``
#: lowers them).
_PATH_CASE = re.IGNORECASE if sys.platform == "win32" else 0


def _home_said(text: str, home: Path) -> str:
    """``text`` with the run's own home said as ``<home>``, however it spells it: as given
    and resolved (Windows: a short ``RUNNER~1`` and its long name), with ``/`` for its
    separator, and with each ``\\`` doubled, as a JSON string (``usage_session.state``) or a
    ``repr`` (an ``OSError``'s message) writes it. On POSIX the three are one."""
    spellings: set[str] = set()
    for path in (home, home.resolve()):
        spellings |= {str(path), path.as_posix(), str(path).replace("\\", "\\\\")}
    for spelling in sorted(spellings, key=len, reverse=True):
        text = re.sub(re.escape(spelling), "<home>", text, flags=_PATH_CASE)
    return text


def _in_home(value: Any, home: Path) -> Any:
    """A stored value with the run's own home said as ``<home>`` (each run has a copy of the
    template's), and the clock's readings in it masked."""
    if not isinstance(value, str):
        return value
    return _NOW_IN.sub("<now>", _no_spans(_home_said(value, home)))


def _program(name: str) -> str:
    """The console script ``name`` of this checkout's environment: in sysconfig's scripts
    folder, ``<prefix>\\Scripts`` on Windows (CI's ``uv pip install --system``), beside the
    interpreter in a POSIX environment, where the two are one."""
    folders = (sysconfig.get_path("scripts"), str(Path(sys.executable).parent))
    found = shutil.which(name, path=os.pathsep.join(folders))
    assert found, f"no {name} in {' or '.join(folders)}"
    return found


def _payload(event: str, cwd: Path, transcript: Path | None = None) -> bytes:
    """``event``'s JSON, with a ``transcript_path`` as Claude Code sends every hook one."""
    payload: dict[str, Any] = {"session_id": "t1", "cwd": str(cwd), **PAYLOADS[event]}
    if transcript is not None:
        payload["transcript_path"] = str(transcript)
    return json.dumps(payload).encode()


def _transcript_lines(project: Path) -> str:
    """A turn of Claude Code's transcript: the prompt, and two replies with their usage."""
    lines: list[dict[str, Any]] = [
        {
            "type": "user",
            "sessionId": "t1",
            "cwd": str(project),
            "timestamp": "2026-10-23T09:59:58Z",
            "message": {"role": "user", "content": "hook-parity-probe"},
        }
    ]
    for message_id in ("m1", "m2"):
        lines.append(
            {
                "type": "assistant",
                "sessionId": "t1",
                "cwd": str(project),
                "timestamp": "2026-10-23T10:00:00Z",
                "message": {
                    "id": message_id,
                    "model": "claude-sonnet-5-5",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {
                        "input_tokens": 5,
                        "cache_creation_input_tokens": 100,
                        "cache_read_input_tokens": 1000,
                        "output_tokens": 50,
                    },
                },
            }
        )
    return "".join(json.dumps(line) + "\n" for line in lines)


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
    # No symbol index: the onboard builds one in a detached ``code index``, which would
    # still be writing this home as each run copies it (Windows refuses to copy its locked
    # ``index.db-shm``: ci.yml run 46066195807). No hook prints or stores anything of it.
    home.mkdir()
    (home / "config.toml").write_text("[code]\nenabled = false\n", encoding="utf-8")
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
    for where in (TRANSCRIPT, SLOT_TRANSCRIPT, PLAIN_TRANSCRIPT):
        (home / where).parent.mkdir(parents=True, exist_ok=True)
        (home / where).write_text(_transcript_lines(project), encoding="utf-8")
    return home, project


def _copy(template: Path, to: Path) -> Path:
    shutil.copytree(template, to)
    return to


def _rows(home: Path) -> dict[str, list[dict[str, Any]]]:
    """Every table's rows, their times and fresh ids masked: the store's whole state, usage
    included (``session_usage``, ``turn_usage``), and the join log
    (``explainability/joins.jsonl``), whose ids and keys are compared as they are."""
    with sqlite3.connect(home / "context.db") as db:
        db.row_factory = sqlite3.Row
        dumped: dict[str, list[dict[str, Any]]] = {}
        tables = [
            name
            for (name,) in db.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"
                " ORDER BY name"
            )
        ]
        for table in tables:
            rows = []
            try:
                found = db.execute(f"SELECT * FROM {table} ORDER BY rowid").fetchall()
            except sqlite3.OperationalError:  # WITHOUT ROWID: read in its key's order
                found = db.execute(f"SELECT * FROM {table}").fetchall()
            for row in found:
                rows.append(
                    {
                        key: "<masked>"
                        if key in _MASKED
                        or key.endswith(("_at", "_ms"))
                        or (
                            isinstance(row[key], str)
                            and (_TIME.match(row[key]) or _EPOCH.match(row[key]))
                        )
                        else _in_home(row[key], home)
                        for key in row.keys()  # noqa: SIM118 (a Row has no __iter__ of keys)
                    }
                )
            dumped[table] = rows
    joins = home / "explainability" / "joins.jsonl"
    dumped["joins.jsonl"] = [
        {
            key: "<masked>"
            if key.endswith("_at") or key in {"ts", "at", "time"}
            else _in_home(value, home)
            for key, value in json.loads(line).items()
        }
        for line in (joins.read_text(encoding="utf-8").splitlines() if joins.exists() else [])
    ]
    return dumped


#: A column one row has and the other does not.
_ABSENT = "<absent>"


def _differences(
    brain_rows: dict[str, list[dict[str, Any]]], fresh_rows: dict[str, list[dict[str, Any]]]
) -> list[str]:
    """Where the brain's rows (:func:`_rows`) are not a fresh process's, a line each: the table
    (or ``joins.jsonl``) and row that differ, and in a row each column, with both values. None
    when they are equal. A loaded CI run once failed the parity test on a bare ``name``, with
    nothing to say which table it was (tester round 3, N1)."""
    said: list[str] = []
    for table in sorted(brain_rows.keys() | fresh_rows.keys()):
        if table not in fresh_rows or table not in brain_rows:
            whose = "the brain" if table in brain_rows else "the fresh process"
            said.append(f"{table}: only {whose} has the table")
            continue
        through, fresh = brain_rows[table], fresh_rows[table]
        for index in range(max(len(through), len(fresh))):
            if index >= len(fresh):
                said.append(f"{table} row {index}: only the brain wrote it: {through[index]}")
            elif index >= len(through):
                said.append(f"{table} row {index}: only the fresh process wrote it: {fresh[index]}")
            elif through[index] != fresh[index]:
                columns = sorted(through[index].keys() | fresh[index].keys())
                changed = [
                    f"{column}: brain {through[index].get(column, _ABSENT)!r},"
                    f" fresh {fresh[index].get(column, _ABSENT)!r}"
                    for column in columns
                    if through[index].get(column, _ABSENT) != fresh[index].get(column, _ABSENT)
                ]
                said.append(f"{table} row {index}: " + "; ".join(changed))
    return said


def _environments(tmp_path: Path) -> dict[str, dict[str, str]]:
    """Six hooks' environments, each its own role, config dir, agent ids and zone; the
    third names none of them, so whatever an earlier one set must be gone; the fourth and
    fifth name the same relative hub, which every fresh process warns of, so the brain must
    warn of it in both, and the fifth's transcript is a managed slot's; the sixth is the
    fleet's usual agent: traced, and on a managed account slot, whose plain copies
    (``plain_vars``) tell it from the plain home its transcript is in (:func:`_transcript_of`).
    ``{home}`` is each run's own home, ``{tmp}`` the test's folder."""
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
        "f": {
            "AISQUARE_ROLE": "coder",
            "AISQUARE_FLEET_AGENT": "agt_parity_f",
            "AISQUARE_PIPELINE_ID": "pl_parity",
            "AISQUARE_TRACE_AGENT_NAME": "agent-parity",
            "AISQUARE_RUN_TRACE_ID": "tr_parity",
            "CLAUDE_CONFIG_DIR": "{home}/claude-accounts/2",
            "CLAUDE_CODE_TMPDIR": "{home}/cache/claude-accounts/2",
            "AISQUARE_PLAIN_CLAUDE_CONFIG_DIR": "{home}/" + PLAIN_HOME,
            "AISQUARE_PLAIN_CLAUDE_CODE_TMPDIR": "{tmp}/cc-plain-tmp",
            "CODEX_HOME": "{home}/codex-accounts/3",
            "AISQUARE_PLAIN_CODEX_HOME": "{tmp}/codex-plain",
            "TZ": "UTC",
        },
    }


def _transcript_of(name: str, home: Path) -> Path:
    """Environment ``name``'s transcript in ``home``: the slot agent's in the plain home, which
    only its plain copies name; the fifth's in slot 2's folder; the others' in no home."""
    return home / {"e": SLOT_TRANSCRIPT, "f": PLAIN_TRANSCRIPT}.get(name, TRANSCRIPT)


#: What rlxd's own environment holds that no hook of these does: each must stay out.
BRAIN_ONLY = {
    "AISQUARE_ROLE": "reviewer",
    "AISQUARE_FLEET_AGENT": "agt_rlxd",
    "AISQUARE_PERSONA": "rlxd-persona",
    "TZ": "Asia/Kolkata",
    # rlxd started from a traced agent's shell, on a slot of its own: none of it may reach
    # a hook's join or its plain home.
    "AISQUARE_PIPELINE_ID": "pl_rlxd",
    "AISQUARE_TRACE_AGENT_NAME": "rlxd-agent",
    "AISQUARE_RUN_TRACE_ID": "tr_rlxd",
    "AISQUARE_PLAIN_CLAUDE_CONFIG_DIR": "/rlxd/plain-claude",
    "AISQUARE_PLAIN_CLAUDE_CODE_TMPDIR": "/rlxd/plain-tmp",
    "CODEX_HOME": "/rlxd/codex",
}


def _hook_env(base: dict[str, str], home: Path, extra: dict[str, str]) -> dict[str, str]:
    env = {key: value for key, value in base.items() if key not in BRAIN_ONLY}
    env.pop("CLAUDE_CONFIG_DIR", None)
    filled = {key: value.format(home=home, tmp=home.parent) for key, value in extra.items()}
    return {**env, "AISQUARE_HOME": str(home), **filled}


def _normal(text: bytes, home: Path) -> bytes:
    """``text`` with the run's own home, which each run has a copy of, said as ``<home>``
    (:func:`_home_said`), and its spans from now masked (:data:`_SPAN`)."""
    said = _home_said(text.decode("utf-8", "surrogateescape"), home)
    return _SPAN.sub("<span>", said).encode("utf-8", "surrogateescape")


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
                input=_payload(event, project, _transcript_of(name, home)),
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
                stdin = _payload(event, project, _transcript_of(name, home))
                result = running.hook(event, stdin, project, env)
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


#: What rlx's own ``hook session-start`` says on stderr when printing its context failed:
#: on Windows its stdout, a pipe, is in the ANSI codepage, which cannot encode the "✓" of a
#: fleet agent's team block, so it fails open and prints nothing (ci.yml run 46081207511).
#: The brain prints the same nothing, as a fresh process does: not the brain's to fix.
_CANNOT_ENCODE = b"UnicodeEncodeError"


def _started(parity: dict[str, Any], name: str) -> tuple[bytes | None, bytes]:
    """What environment ``name``'s session-start printed in a fresh process, and its stderr;
    ``None`` for the printing only where rlx's own hook could not encode it on Windows
    (:data:`_CANNOT_ENCODE`) and the brain printed nothing either. Parity is the bar there;
    the checks of what it says bite again once rlx's hook prints it."""
    printed, said = parity["fresh"][name][0]["session-start"][1:]
    brain_printed = parity["brain"][name][0]["session-start"][1]
    if sys.platform == "win32" and not printed and not brain_printed and _CANNOT_ENCODE in said:
        return None, said
    return printed, said


def test_every_event_prints_the_same_bytes_and_writes_the_same_rows_both_ways(
    parity: dict[str, Any],
) -> None:
    for name in ("a", "b", "c", "d", "e", "f"):
        fresh_printed, fresh_rows = parity["fresh"][name]
        brain_printed, brain_rows = parity["brain"][name]
        for event in PAYLOADS:
            assert brain_printed[event] == fresh_printed[event], (name, event)
        differs = _differences(brain_rows, fresh_rows)
        assert not differs, f"{name}: the brain's rows are not a fresh process's:\n" + "\n".join(
            differs
        )
        assert brain_rows == fresh_rows, name  # whatever _differences could miss
    # The checks can fail: what each environment says is its own.
    a_start, a_said = _started(parity, "a")
    if a_start is not None:
        assert b"seed fact for the hook check" in a_start, a_said
        assert b"role: coder" in a_start, a_said
    b_start, b_said = _started(parity, "b")
    if b_start is not None:
        assert b"role: manager" in b_start, b_said
    rows = {name: parity["fresh"][name][1]["team_session"] for name in ("a", "b", "c")}
    assert rows["a"][0]["role"] == "coder" and rows["b"][0]["role"] == "manager"
    assert rows["a"][0]["account"] != rows["b"][0]["account"]
    assert [row["text"] for row in parity["fresh"]["a"][1]["prompt"]] == ["hook-parity-probe"]
    for name in ("d", "e"):
        start_err = parity["brain"][name][0]["session-start"][2]
        assert b"RELATIVE" in start_err, f"{name}: the brain warns of the hub each time"
    # The traced agent on slot 2: its join names it and its Run (review #2562 round 2, M1),
    # and the Stop recorded its turn's usage on the plain slot its transcript is in, which
    # only its plain copies name: a brain not sent them records no slot (tester round 3, N2).
    f_rows = parity["fresh"]["f"][1]
    assert [
        (join.get("pipeline_id"), join.get("agent_name"), join.get("trace_id"))
        for join in f_rows["joins.jsonl"]
    ] == [("pl_parity", "agent-parity", "tr_parity")]
    assert f_rows["usage_session"], "the Stop read the transcript"
    assert f_rows["usage_session"][0]["account_slot"] == 1
    # A transcript in a managed slot's folder is that slot's.
    assert parity["fresh"]["e"][1]["usage_session"][0]["account_slot"] == 2


def test_rows_that_differ_are_named_by_table_row_and_column() -> None:
    fresh: dict[str, list[dict[str, Any]]] = {
        "usage_session": [{"account_slot": 1, "turns": 1}, {"account_slot": 2, "turns": 1}],
        "prompt": [],
        "team_session": [{"role": "coder"}],
    }
    through_brain: dict[str, list[dict[str, Any]]] = {
        "usage_session": [{"account_slot": 1, "turns": 1}, {"account_slot": None, "turns": 1}],
        "prompt": [{"text": "hook-parity-probe"}],
        "joins.jsonl": [],
    }
    assert _differences(through_brain, fresh) == [
        "joins.jsonl: only the brain has the table",
        "prompt row 0: only the brain wrote it: {'text': 'hook-parity-probe'}",
        "team_session: only the fresh process has the table",
        "usage_session row 1: account_slot: brain None, fresh 2",
    ]
    assert _differences({"t": [{"a": 1}]}, {"t": [{"a": 1}, {"b": 2}]}) == [
        "t row 1: only the fresh process wrote it: {'b': 2}"
    ]
    assert _differences({"t": [{"a": 1}]}, {"t": [{"b": 1}]}) == [
        "t row 0: a: brain 1, fresh '<absent>'; b: brain '<absent>', fresh 1"
    ]
    assert _differences(fresh, fresh) == []


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
    # The user's home, as Path.home() reads it: HOME, and on Windows USERPROFILE (ntpath).
    user_home = "USERPROFILE" if sys.platform == "win32" else "HOME"
    assert {"AISQUARE_ROLE", "CLAUDE_CONFIG_DIR", "AISQUARE_FLEET_AGENT", user_home} <= read
    # The fleet's real paths ran, a traced session's join and a slot agent's Stop: what they
    # read is read here (review #2562 round 2: the list had dropped these five).
    assert {
        "AISQUARE_TRACE_AGENT_NAME",
        "AISQUARE_RUN_TRACE_ID",
        "AISQUARE_PLAIN_CLAUDE_CONFIG_DIR",
        "AISQUARE_PLAIN_CLAUDE_CODE_TMPDIR",
        "CODEX_HOME",
    } <= read


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
        # Exact names, case and all (tester #2645): a variant is not the variable.
        "aisquare_role",
        "Path",
        "Aisquare_Home",
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


def test_a_daemon_thread_the_hook_started_is_not_waited_for_and_named_on_the_brains_log(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """As an exit waits for no daemon thread: the hook's reply does not wait for one, and the
    brain names it on its own log, never in the hook's stderr, which ``rlx hook``'s exit
    leaves empty of it (review #2562 round 2, m1)."""
    import time

    from aisquare.services import agent_events

    handle = agent_events.handle
    release = threading.Event()

    def handle_and_leave_a_daemon(*args: Any, **kwargs: Any) -> str:
        threading.Thread(target=lambda: release.wait(60), name="hook-daemon", daemon=True).start()
        return handle(*args, **kwargs)

    _stop_hook(tmp_path)  # once without, for the time a Stop takes here
    started = time.monotonic()
    plain = _stop_hook(tmp_path)
    took_plain = time.monotonic() - started
    capsys.readouterr()
    monkeypatch.setattr(agent_events, "handle", handle_and_leave_a_daemon)
    try:
        started = time.monotonic()
        result = _stop_hook(tmp_path)
        took = time.monotonic() - started
        assert [t for t in threading.enumerate() if t.name == "hook-daemon"], "it runs on"
        assert took < took_plain + 1.0, f"the hook waited for it: {took:.2f}s"
        assert base64.b64decode(result["stderr_b64"]) == base64.b64decode(plain["stderr_b64"])
        log = capsys.readouterr().err
        assert "daemon threads ['hook-daemon'] still run" in log
    finally:
        release.set()


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
