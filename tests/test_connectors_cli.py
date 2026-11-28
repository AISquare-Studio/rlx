"""``rlx connectors list`` asks rlxd (handoff 0.10.3, 2.4 and 7.7).

``rlxd call connectors.list`` is stood in for here, so the suite runs with no rlxd: the
reply it gives, and how it fails. rlxd's own tests (``rlxd/crates/rlxd/tests/
connectors.rs``) run the real one, and check that ``rlx --json connectors list`` prints
exactly its ``data``.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli import connectors as connectors_cli
from aisquare.cli.app import app

runner = CliRunner()

DATA = {
    "connectors": [
        {
            "id": "claude.ai Slack",
            "source": "claude.ai",
            "status": "connected",
            "tools": [
                {"name": "mcp__claude_ai_Slack__slack_search_public", "read_only_hint": None}
            ],
        }
    ],
    "as_of": "2026-11-27T09:00:00.000Z",
    "from": "init",
    "model_turns": 0,
}


@pytest.fixture
def rlxd(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> list[list[str]]:
    """An rlxd at RLX_RLXD_BIN that answers ``connectors.list`` with :data:`DATA`; the
    command lines it was run with."""
    program = tmp_path / "rlxd"
    program.write_text("")
    monkeypatch.setenv(connectors_cli.RLXD_BIN_ENV, str(program))
    calls: list[list[str]] = []

    def run(argv: list[str]) -> subprocess.CompletedProcess[str]:
        calls.append(argv)
        reply = {"schema_version": 1, "ok": True, "data": DATA}
        return subprocess.CompletedProcess(argv, 0, json.dumps(reply), "")

    monkeypatch.setattr(connectors_cli, "run_rlxd", run)
    return calls


def test_json_list_prints_rlxds_data_bare_and_exits_0(rlxd: list[list[str]]) -> None:
    result = runner.invoke(app, ["--json", "connectors", "list"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout) == DATA
    assert rlxd == [[rlxd[0][0], "call", "connectors.list"]]
    assert Path(rlxd[0][0]).is_absolute()


def test_the_human_list_names_each_connector(rlxd: list[list[str]]) -> None:
    result = runner.invoke(app, ["connectors", "list"])
    assert result.exit_code == 0, result.output
    assert "claude.ai Slack  connected  1 tool" in result.stdout


def test_a_refusal_is_rlxds_code(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv(connectors_cli.RLXD_BIN_ENV, str(tmp_path / "rlxd"))
    reply = {
        "schema_version": 1,
        "ok": False,
        "error": {"code": "role_not_allowed", "message": "no"},
    }
    monkeypatch.setattr(
        connectors_cli,
        "run_rlxd",
        lambda argv: subprocess.CompletedProcess(argv, 1, json.dumps(reply), ""),
    )
    result = runner.invoke(app, ["--json", "connectors", "list"])
    assert result.exit_code == 1
    assert json.loads(result.stdout)["error"] == "role_not_allowed"


def test_rlxd_not_running_says_so(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv(connectors_cli.RLXD_BIN_ENV, str(tmp_path / "rlxd"))
    monkeypatch.setattr(
        connectors_cli,
        "run_rlxd",
        lambda argv: subprocess.CompletedProcess(argv, 3, "", "rlxd is not running\n"),
    )
    result = runner.invoke(app, ["--json", "connectors", "list"])
    assert result.exit_code == 1
    payload = json.loads(result.stdout)
    assert payload["error"] == "rlxd_not_running"
    assert payload["detail"] == "rlxd is not running"


def test_no_rlxd_is_rlxd_not_found(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(connectors_cli, "rlxd_program", lambda: None)
    result = runner.invoke(app, ["--json", "connectors", "list"])
    assert result.exit_code == 1
    assert json.loads(result.stdout)["error"] == "rlxd_not_found"


def test_rlxd_is_named_by_an_absolute_path_only(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv(connectors_cli.RLXD_BIN_ENV, str(tmp_path / "rlxd"))
    assert connectors_cli.rlxd_program() == tmp_path / "rlxd"
    monkeypatch.setenv(connectors_cli.RLXD_BIN_ENV, "rlxd")
    assert connectors_cli.rlxd_program() is None


def test_rlxd_beside_rlx_is_found(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.delenv(connectors_cli.RLXD_BIN_ENV, raising=False)
    name = "rlxd.exe" if sys.platform == "win32" else "rlxd"
    (tmp_path / name).write_text("")
    monkeypatch.setattr(sys, "argv", [str(tmp_path / "rlx")])
    assert connectors_cli.rlxd_program() == tmp_path / name


def test_the_group_is_shown_with_only_list() -> None:
    import typer.main

    root: Any = typer.main.get_command(app)
    group: Any = root.commands["connectors"]
    assert not group.hidden
    assert [name for name, command in group.commands.items() if not command.hidden] == ["list"]
