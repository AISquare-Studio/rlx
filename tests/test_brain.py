"""0.10.3 §1.6: the resident Python brain, ``rlx brain --stdio``, which rlxd supervises.

The protocol (``services.brain``): a hello naming the brain protocol, then requests, each
``accepted`` before it runs and then answered. Every frame the brain writes is one
``rlxd/schemas/v1/brain-output.json`` accepts, and every frame these tests write is one
``brain-input.json`` accepts. A brain of another protocol refuses, by name; a frame it cannot
read ends it rather than be guessed at; a request that fails is answered and the brain goes
on. As a process, nothing it runs can write into its replies or read its requests. rlxd's own
side, with the real binary, is ``rlxd/crates/rlxd/tests/brain.rs``.
"""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import textwrap
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import jsonschema
import pytest
from typer.testing import CliRunner

from aisquare.cli import app as app_module
from aisquare.core import feature_use
from aisquare.core.version import __version__
from aisquare.services import brain

SCHEMAS = Path(__file__).resolve().parent.parent / "rlxd" / "schemas" / "v1"


def _schema(name: str) -> dict[str, Any]:
    loaded: dict[str, Any] = json.loads((SCHEMAS / name).read_text(encoding="utf-8"))
    return loaded


def _check(frames: list[dict[str, Any]], schema: str) -> None:
    validator = jsonschema.Draft202012Validator(_schema(schema))
    for frame in frames:
        validator.validate(frame)


HELLO = {"type": "hello", "protocol": 1, "rlxd_version": "0.10.3"}


def _request(request_id: int, method: str, **params: Any) -> dict[str, Any]:
    return {"type": "request", "id": request_id, "method": method, "params": params}


def _talk(
    frames: list[dict[str, Any]] | bytes, methods: Mapping[str, brain.Method] | None = None
) -> tuple[int, list[dict[str, Any]]]:
    """Runs a brain over ``frames``: its exit status, and every frame it wrote."""
    if isinstance(frames, bytes):
        raw = frames
    else:
        _check(frames, "brain-input.json")
        raw = b"".join(json.dumps(frame).encode() + b"\n" for frame in frames)
    written = io.BytesIO()
    status = brain.serve(brain.Channel(io.BytesIO(raw), written), methods)
    lines = written.getvalue().decode("ascii").splitlines()
    said = [json.loads(line) for line in lines]
    _check(said, "brain-output.json")
    return status, said


def test_the_brain_says_hello_then_accepts_each_request_before_it_answers() -> None:
    status, said = _talk([HELLO, _request(1, "ping"), _request(2, "ping")])
    assert status == 0, "its stdin's end is rlxd's: the brain exits"
    assert said[0] == {
        "type": "hello",
        "protocol": 1,
        "pid": os.getpid(),
        "rlx_version": __version__,
    }
    assert said[1:] == [
        {"type": "accepted", "id": 1},
        {"type": "reply", "id": 1, "ok": True, "result": {"pid": os.getpid()}},
        {"type": "accepted", "id": 2},
        {"type": "reply", "id": 2, "ok": True, "result": {"pid": os.getpid()}},
    ]


def test_accepted_is_written_before_the_method_runs() -> None:
    seen: list[str] = []
    written = io.BytesIO()

    def probe(params: Mapping[str, Any]) -> dict[str, Any]:
        seen.append(written.getvalue().decode())
        return {}

    raw = b"".join(json.dumps(f).encode() + b"\n" for f in [HELLO, _request(5, "probe")])
    brain.serve(brain.Channel(io.BytesIO(raw), written), {"probe": probe})
    assert seen and seen[0].endswith('{"type":"accepted","id":5}\n'), seen


def test_a_failed_request_is_answered_and_the_brain_goes_on() -> None:
    def boom(params: Mapping[str, Any]) -> dict[str, Any]:
        raise RuntimeError("no store")

    def refuse(params: Mapping[str, Any]) -> dict[str, Any]:
        raise brain.BrainError("bad_event", "no such event")

    methods = {"boom": boom, "refuse": refuse, **brain.METHODS}
    frames = [HELLO, _request(1, "boom"), _request(2, "refuse"), _request(3, "nope")]
    status, said = _talk([*frames, _request(4, "ping")], methods)
    assert status == 0
    replies = {frame["id"]: frame for frame in said if frame["type"] == "reply"}
    assert replies[1]["error"] == {"code": "brain_failed", "message": "RuntimeError: no store"}
    assert replies[2]["error"] == {"code": "bad_event", "message": "no such event"}
    assert replies[3]["error"]["code"] == "unknown_method"
    assert replies[4] == {"type": "reply", "id": 4, "ok": True, "result": {"pid": os.getpid()}}


@pytest.mark.parametrize("protocol", [0, 2, 99, "1", True, None])
def test_a_protocol_the_brain_does_not_speak_is_refused_by_name(protocol: object) -> None:
    hello = {"type": "hello", "protocol": protocol, "rlxd_version": "9.9.9"}
    status, said = _talk(json.dumps(hello).encode() + b"\n" + b"\n")
    assert status == 3
    assert len(said) == 1
    assert said[0]["type"] == "refused"
    assert said[0]["code"] == "incompatible_protocol"
    assert said[0]["supported"] == [1]
    assert __version__ in said[0]["message"]


@pytest.mark.parametrize(
    "raw",
    [
        b"not json\n",
        b"[1, 2]\n",
        json.dumps({"type": "request", "id": 1, "method": "ping"}).encode() + b"\n",
        json.dumps(HELLO).encode() + b"\n" + json.dumps(HELLO).encode() + b"\n",
        json.dumps(HELLO).encode() + b"\n" + b'{"type": "request", "id": "1", "method": "p"}\n',
        json.dumps(HELLO).encode() + b"\n" + b'{"type": "request", "id": 1, "method": "p", '
        b'"params": []}\n',
    ],
)
def test_a_frame_the_brain_cannot_read_ends_it_rather_than_be_guessed_at(raw: bytes) -> None:
    status, said = _talk(raw)
    assert status == 2
    assert all(frame["type"] == "hello" for frame in said)


def test_rlxd_gone_before_its_hello_is_a_quiet_exit() -> None:
    assert _talk(b"") == (0, [])


def test_rlx_brain_runs_only_as_rlxd_starts_it_and_is_no_feature_counted(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    counted: list[str] = []

    def record(name: str, **_: object) -> bool:
        counted.append(name)
        return True

    monkeypatch.setattr(feature_use, "record", record)
    result = CliRunner().invoke(app_module.app, ["brain"])
    assert result.exit_code == 2
    assert "rlxd starts it, with --stdio" in result.output
    assert counted == [], "rlxd starts the brain: no feature use, no active ping"
    assert "brain" in app_module._NOT_FEATURES


def _brain_process(script: str, frames: list[dict[str, Any]]) -> subprocess.CompletedProcess[bytes]:
    """``python -c script`` with ``frames`` on its stdin: a brain started as rlxd starts one."""
    raw = b"".join(json.dumps(frame).encode() + b"\n" for frame in frames)
    return subprocess.run(
        [sys.executable, "-c", textwrap.dedent(script)],
        input=raw,
        capture_output=True,
        timeout=60,
        check=False,
    )


def test_nothing_the_brain_runs_writes_into_its_replies_or_reads_its_requests() -> None:
    noisy = """
        import os, subprocess, sys
        from aisquare.services import brain

        def noisy(params):
            print("a stray print")
            os.write(1, b"a stray write to fd 1\\n")
            subprocess.run([sys.executable, "-c", "print('a child prints')"], check=True)
            read = subprocess.run(
                [sys.executable, "-c", "import sys; print(repr(sys.stdin.read()))"],
                capture_output=True, text=True, check=True,
            ).stdout.strip()
            return {"child_read": read, "stdin": sys.stdin.read()}

        brain.METHODS["noisy"] = noisy
        raise SystemExit(brain.serve_stdio())
    """
    done = _brain_process(noisy, [HELLO, _request(1, "noisy"), _request(2, "ping")])
    assert done.returncode == 0, done.stderr.decode()
    said = [json.loads(line) for line in done.stdout.decode("ascii").splitlines()]
    _check(said, "brain-output.json")
    assert [frame["type"] for frame in said] == ["hello", "accepted", "reply", "accepted", "reply"]
    assert said[2]["result"] == {"child_read": "''", "stdin": ""}, "fd 0 is the null device"
    stderr = done.stderr.decode()
    for noise in ("a stray print", "a stray write to fd 1", "a child prints"):
        assert noise in stderr, f"{noise!r} belongs in the brain's log: {stderr}"


def test_the_protocols_channel_is_inherited_by_no_child() -> None:
    """Its fds (and so, on Windows, their handles) are not inheritable, and fds 0 and 1,
    which a child does inherit, are no longer the protocol's pipes."""
    probe = """
        import json, os, sys
        from aisquare.services import brain

        channel = brain.private_channel()
        fds = [channel._reader.fileno(), channel._writer.fileno()]
        said = {
            "inheritable": [os.get_inheritable(fd) for fd in fds],
            "stdin_is_null": os.read(0, 1) == b"",
        }
        channel.write(said)
    """
    done = _brain_process(probe, [HELLO])
    assert done.returncode == 0, done.stderr.decode()
    said = json.loads(done.stdout.decode("ascii"))
    assert said == {"inheritable": [False, False], "stdin_is_null": True}


def test_rlx_brain_stdio_speaks_the_protocol_as_a_process() -> None:
    done = subprocess.run(
        [sys.executable, "-m", "aisquare", "brain", "--stdio"],
        input=json.dumps(HELLO).encode() + b"\n" + json.dumps(_request(1, "ping")).encode() + b"\n",
        capture_output=True,
        timeout=60,
        check=False,
    )
    assert done.returncode == 0, done.stderr.decode()
    said = [json.loads(line) for line in done.stdout.decode("ascii").splitlines()]
    _check(said, "brain-output.json")
    assert [frame["type"] for frame in said] == ["hello", "accepted", "reply"]
    assert said[0]["pid"] == said[2]["result"]["pid"]
