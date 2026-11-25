"""typer is capped below 0.30, the first release this CLI has not been checked on.

typer 0.29.0 (PyPI, 2026-11-09) is a rewrite that every command of a fresh install died on,
``--version`` included, before it read its arguments, until both were ported:

- ``cli.common.AgentPath`` subclassed ``typer.models.TyperPath`` and went in as
  ``click_type=``; 0.29.0 has neither, so ``aisquare.cli.common`` failed to import. It is a
  ``parser=`` callable now, which 0.28 and 0.29 both take;
- ``cli.global_flags`` built the five shared flags as ``TyperOption(...)`` by hand, and
  0.29.0's wants a ``type_descriptor`` none of them passed (0.9.2 broke on this one alone).
  They are declared as ``typer.Option`` now, and typer builds them.

0.29 was a rewrite in a minor release, so 0.30 may be another: pyproject caps typer below
0.30 until it is checked. These pin the cap, and what it keeps working on the typer that is
installed (CI resolves the newest one the cap admits): dropping the cap lets an install
resolve 0.30, and the second test fails there."""

from __future__ import annotations

import re
import tomllib
from importlib.metadata import version
from pathlib import Path

import pytest
from typer.testing import CliRunner

from aisquare.core.state import get_state

PYPROJECT = Path(__file__).resolve().parent.parent / "pyproject.toml"


@pytest.fixture(autouse=True)
def work_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    work = tmp_path / "repo"
    work.mkdir()
    monkeypatch.chdir(work)
    return work


def _release(text: str) -> tuple[int, ...]:
    return tuple(int(part) for part in re.findall(r"\d+", text)[:3])


def test_pyproject_caps_typer_below_0_30() -> None:
    dependencies = tomllib.loads(PYPROJECT.read_text(encoding="utf-8"))["project"]["dependencies"]
    typer = [spec for spec in dependencies if re.match(r"typer\s*[<>=!~]", spec)]
    assert typer == ["typer>=0.26.8,<0.30"]


def test_every_command_builds_on_the_typer_installed(runner: CliRunner, work_dir: Path) -> None:
    assert _release(version("typer")) < (0, 30), version("typer")
    from aisquare.cli import common, global_flags
    from aisquare.cli.app import app

    assert common.AgentPath()(str(work_dir)) == work_dir
    assert [option.opts[0] for option in global_flags._shared_options()] == [
        "--verbose",
        "--quiet",
        "--json",
        "--profile",
        "--no-color",
    ]
    result = runner.invoke(app, ["--version"])
    assert result.exit_code == 0, result.output


def test_the_five_shared_flags_work_after_a_command(runner: CliRunner) -> None:
    """The injected ones, not the root callback's: after the command, where only they parse."""
    from aisquare.cli.app import app

    argv = ["config", "list", "--json", "-v", "-q", "--no-color", "--profile", "alt"]
    result = runner.invoke(app, argv)

    assert result.exit_code == 0, result.output
    state = get_state()
    assert (state.json_output, state.verbose, state.quiet, state.no_color) == (True,) * 4
    assert state.profile == "alt"
