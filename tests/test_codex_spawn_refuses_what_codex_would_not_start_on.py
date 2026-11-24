"""A Codex fleet spawn refuses, before its window opens, a list of its arguments Codex would not
start on, naming the list (``AgentAdapter.args_refusal``): the role's binding
(``team.profiles.<role>.args``), its ``agent_args.codex`` or the spawn's own."""

from __future__ import annotations

from pathlib import Path

import pytest

from aisquare.core.agent_adapters import get_adapter
from aisquare.core.config import FleetRoleSettings, RoleLaunchProfile, load_config, save_config
from aisquare.models import ProjectInfo
from aisquare.services import fleet as fleet_service
from aisquare.services import team as team_service
from tests.test_codex_account_switch import codex_on_path  # noqa: F401 - fixture, by name
from tests.test_fleet_service import (  # noqa: F401 - fixtures, by name
    FakeTmux,
    _command,
    project,
    repo,
    tmux,
)

CODEX = get_adapter("codex")


def _configure(bound: list[str], role: list[str], *, first_prompt: str | None = None) -> None:
    """The coder role's binding, made for Claude Code, holding ``bound``, and its Codex
    ``agent_args`` holding ``role``."""
    config = load_config()
    config.fleet.roles["coder"] = FleetRoleSettings(worktree=False, agent_args={"codex": role})
    config.team.profiles["coder"] = RoleLaunchProfile(agent="claude-code", args=bound)
    if first_prompt is not None:
        config.fleet.first_prompt = first_prompt
    save_config(config)


def _holds(command: list[str], run: list[str]) -> bool:
    return any(command[at : at + len(run)] == run for at in range(len(command)))


@pytest.mark.parametrize(
    ("bound", "role", "own", "said"),
    [
        (
            ["--effort", "high"],
            [],
            [],
            "team.profiles.coder.args: --effort is a Claude Code flag",
        ),
        (
            [],
            ["--disallowedTools=Edit"],
            [],
            "fleet.roles.coder.agent_args.codex: --disallowedTools is a Claude Code flag",
        ),
        ([], [], ["--add-dir"], "the spawn's own arguments: --add-dir is given no folder"),
        (
            [],
            ["--add-dir", "/a", "/b"],
            [],
            "fleet.roles.coder.agent_args.codex: --add-dir takes one folder, and Codex would "
            "read '/b' as its prompt: write --add-dir /a --add-dir /b",
        ),
        (
            ["--add-dir", "/a", "/b"],
            [],
            [],
            "team.profiles.coder.args: --add-dir takes one folder",
        ),
    ],
    ids=["binding-effort", "role-disallowed", "own-no-folder", "role-two", "binding-two"],
)
def test_a_codex_spawn_is_refused_naming_the_list_and_opens_no_window(
    tmux: FakeTmux,  # noqa: F811
    codex_on_path: Path,  # noqa: F811
    project: ProjectInfo,  # noqa: F811
    bound: list[str],
    role: list[str],
    own: list[str],
    said: str,
) -> None:
    """Each list holds one argument Codex would not start on; the refusal starts with the
    list's setting and no window is spawned."""
    _configure(bound, role)
    with pytest.raises(fleet_service.FleetError) as refused:
        fleet_service.spawn(project, "coder", agent="codex", agent_args=own)
    assert str(refused.value).startswith(said), refused.value
    assert tmux.spawned == []


@pytest.mark.parametrize("first", ["prompt", "task"])
def test_a_second_folder_in_the_spawns_own_arguments_is_refused_beside_its_first_message(
    tmux: FakeTmux,  # noqa: F811
    codex_on_path: Path,  # noqa: F811
    project: ProjectInfo,  # noqa: F811
    first: str,
) -> None:
    """The fleet puts its first message (``--prompt``, or a task's kickoff) on Codex's command
    line, so a word after an ``--add-dir`` folder in the spawn's own arguments is refused."""
    _configure([], [])
    task, _created = team_service.add_task("a task", role="coder", cwd=project.root)
    with pytest.raises(fleet_service.FleetError) as refused:
        fleet_service.spawn(
            project,
            "coder",
            agent="codex",
            agent_args=["--add-dir", "/a", "/b"],
            prompt="start" if first == "prompt" else None,
            task_id=task.id if first == "task" else None,
        )
    assert str(refused.value) == (
        "the spawn's own arguments: --add-dir takes one folder, and Codex would read '/b' as "
        "its prompt: write --add-dir /a --add-dir /b"
    )
    assert tmux.spawned == []


@pytest.mark.parametrize(
    ("own", "prompt", "first_prompt"),
    [
        (["--add-dir", "/a", "--add-dir", "/b"], "start", None),
        (["--add-dir", "/a", "fix the bug"], None, None),
        (["--add-dir", "/a", "fix the bug"], "start", "type"),
    ],
    ids=["one-each-beside-prompt", "own-prompt", "own-prompt-beside-typed"],
)
def test_a_codex_spawn_opens_its_window_with_its_own_arguments_in_order(
    tmux: FakeTmux,  # noqa: F811
    codex_on_path: Path,  # noqa: F811
    project: ProjectInfo,  # noqa: F811
    own: list[str],
    prompt: str | None,
    first_prompt: str | None,
) -> None:
    """One ``--add-dir`` per folder, and a word after one where the fleet puts no first message
    on the command line (none given, or typed under ``[fleet] first_prompt = "type"``), with
    Codex's effort in the role's ``agent_args``: one window, whose command holds ``own``."""
    _configure([], ["-c", 'model_reasoning_effort="high"'], first_prompt=first_prompt)
    fleet_service.spawn(project, "coder", agent="codex", agent_args=own, prompt=prompt)
    assert len(tmux.spawned) == 1 and _holds(_command(tmux), own), _command(tmux)


@pytest.mark.parametrize(
    ("args", "may_prompt", "said"),
    [
        (["--effort=high"], True, "--effort is a Claude Code flag"),
        (["--add-dir="], True, "--add-dir is given no folder"),
        (["--add-dir", "--search"], True, "--add-dir is given no folder"),
        (["--add-dir=/a", "/b"], False, "write --add-dir /a --add-dir /b"),
        (["--add-dir", "/a", "/b", "/c"], False, "write --add-dir /a --add-dir /b --add-dir /c"),
        (["--add-dir", "/a", "--", "/b"], False, None),
        (["--", "--effort"], False, None),
        (["--add-dir", "/a", "/b"], True, None),
        (["--search", "--add-dir", "/a", "--add-dir", "/b", "-c", "x=1"], False, None),
    ],
)
def test_codex_args_refusal_reads_up_to_its_separator(
    args: list[str], may_prompt: bool, said: str | None
) -> None:
    """``said`` is in the refusal, or ``None`` is answered where ``said`` is ``None``."""
    refusal = CODEX.args_refusal(args, may_prompt=may_prompt)
    assert refusal is None if said is None else refusal is not None and said in refusal, refusal
