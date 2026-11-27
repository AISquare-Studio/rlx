"""A Codex fleet spawn refuses, before its window opens, a list of its arguments Codex would not
start on, naming the list (``AgentAdapter.args_refusal``): the role's binding
(``team.profiles.<role>.args``), its ``agent_args.codex`` or the spawn's own."""

from __future__ import annotations

from pathlib import Path

import pytest

from aisquare.core.agent_adapters import get_adapter
from aisquare.core.agent_adapters.types import BadEffortError
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


def _configure(bound: list[str], role: list[str], *, typed: bool = False) -> None:
    """The coder role's binding, made for Claude Code, holding ``bound``, and its Codex
    ``agent_args`` holding ``role``; ``typed``: ``[fleet] first_prompt = "type"``."""
    config = load_config()
    config.fleet.roles["coder"] = FleetRoleSettings(worktree=False, agent_args={"codex": role})
    config.team.profiles["coder"] = RoleLaunchProfile(agent="claude-code", args=bound)
    if typed:
        config.fleet.first_prompt = "type"
    save_config(config)


def _holds(command: list[str], run: list[str]) -> bool:
    return any(command[at : at + len(run)] == run for at in range(len(command)))


@pytest.mark.parametrize(
    ("bound", "role", "own", "said"),
    [
        (
            ["--chrome"],
            [],
            [],
            "team.profiles.coder.args: --chrome is a Claude Code flag Codex has no equivalent "
            "for: Codex's browser is its own browser_use feature, not Chrome",
        ),
        ([], ["--effort"], [], "fleet.roles.coder.agent_args.codex: --effort is given no level"),
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
    ids=[
        "binding-chrome",
        "role-no-effort",
        "role-disallowed",
        "own-no-folder",
        "role-two",
        "binding-two",
    ],
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
    ("own", "prompt", "typed"),
    [
        (["--add-dir", "/a", "--add-dir", "/b"], "start", False),
        (["--add-dir", "/a", "fix the bug"], None, False),
        (["--add-dir", "/a", "fix the bug"], "start", True),
    ],
    ids=["one-each-beside-prompt", "own-prompt", "own-prompt-beside-typed"],
)
def test_a_codex_spawn_opens_its_window_with_its_own_arguments_in_order(
    tmux: FakeTmux,  # noqa: F811
    codex_on_path: Path,  # noqa: F811
    project: ProjectInfo,  # noqa: F811
    own: list[str],
    prompt: str | None,
    typed: bool,
) -> None:
    """One ``--add-dir`` per folder, and a word after one where the fleet puts no first message
    on the command line (none given, or typed under ``[fleet] first_prompt = "type"``), with
    Codex's effort in the role's ``agent_args``: one window, whose command holds ``own``."""
    _configure([], ["-c", 'model_reasoning_effort="high"'], typed=typed)
    fleet_service.spawn(project, "coder", agent="codex", agent_args=own, prompt=prompt)
    assert len(tmux.spawned) == 1 and _holds(_command(tmux), own), _command(tmux)


@pytest.mark.parametrize(
    ("args", "may_prompt", "said"),
    [
        (["--effort=high"], True, None),
        (["--effort", "--search"], True, "--effort is given no level"),
        (["--effort", "a b"], True, "Codex reasoning effort must be a level name"),
        (["--disallowedTools=Edit"], True, "--disallowedTools is a Claude Code flag"),
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


#: The three ways a Claude Code flag reaches Codex (``CodexAdapter.native_args`` and
#: ``args_refusal``): in Codex's form, as it is (Codex reads it the same), or refused with why.
MAPPED, PASSED, REFUSED = "mapped", "passed through", "refused"
NO_TOOL_LISTS = "Codex has no tool lists; its sandbox and approvals decide what its tools may do"
BROWSER = "Codex's browser is its own browser_use feature, not Chrome"
SESSIONS = "Codex resumes and forks with its own commands, which rlx's Restart and Fork give it"
SYSTEM_PROMPT = (
    "Codex's developer_instructions replace its instructions, rlx's persona among them, rather "
    "than add to them"
)
MCP_SERVERS = "Codex's MCP servers are its config's mcp_servers table"
SETTINGS = "Codex's settings are its config.toml, a --profile and -c key=value"
PRINT_MODE = "Codex runs without a window only as its exec command, which a fleet window never runs"
CONTINUE = (
    "-c without key=value is Claude Code's --continue, which Codex has no equivalent for: "
    + SESSIONS
)
PRINT = (
    "-p without a profile name is Claude Code's --print, which Codex has no equivalent for: "
    + PRINT_MODE
    + "; Codex's is --profile <name>"
)


def _foreign(flag: str, why: str) -> str:
    return f"{flag} is a Claude Code flag Codex has no equivalent for: {why}"


#: Every Claude Code flag rlx writes for it, or its roles and docs suggest (each one its adapter
#: declares, ``test_the_table_holds_every_flag_claude_codes_adapter_declares``): the flag, a
#: list holding it, what Codex is given, and the list Codex starts on (``MAPPED``) or the one
#: line its refusal says (``REFUSED``).
TABLE: list[tuple[str, list[str], str, object]] = [
    ("--effort", ["--effort", "high"], MAPPED, ["-c", 'model_reasoning_effort="high"']),
    ("--effort", ["--effort=xhigh"], MAPPED, ["-c", 'model_reasoning_effort="xhigh"']),
    ("--effort", ["--effort", "ultracode"], MAPPED, ["-c", 'model_reasoning_effort="max"']),
    (
        "--effort",
        ["--search", "--effort", "Low", "--add-dir", "/a"],
        MAPPED,
        ["--search", "-c", 'model_reasoning_effort="low"', "--add-dir", "/a"],
    ),
    ("--model", ["--model", "gpt-5.5"], PASSED, None),
    ("-m", ["-m", "gpt-5.5"], PASSED, None),
    ("--add-dir", ["--add-dir", "/a", "--add-dir=/b"], PASSED, None),
    ("-c", ["-c", 'model_reasoning_effort="high"'], PASSED, None),
    ("-c", ["-c", "features.hooks=false", "--search"], PASSED, None),
    ("-p", ["-p", "work"], PASSED, None),
    ("-p", ["-p", "team.review-2"], PASSED, None),
    ("-c", ["-c"], REFUSED, CONTINUE),
    ("-c", ["-c", "--search"], REFUSED, CONTINUE),
    ("-c", ["-c", "fix the bug"], REFUSED, CONTINUE),
    ("-p", ["-p"], REFUSED, PRINT),
    ("-p", ["-p", "--search"], REFUSED, PRINT),
    ("-p", ["-p", "fix the bug"], REFUSED, PRINT),
    ("-p", ["-p", '"work"'], REFUSED, PRINT),
    ("--allowedTools", ["--allowedTools", "Bash(git:*)"], REFUSED, NO_TOOL_LISTS),
    ("--allowed-tools", ["--allowed-tools=Read"], REFUSED, NO_TOOL_LISTS),
    ("--disallowedTools", ["--disallowedTools", "Edit"], REFUSED, NO_TOOL_LISTS),
    ("--disallowed-tools", ["--disallowed-tools", "Edit"], REFUSED, NO_TOOL_LISTS),
    ("--tools", ["--tools", "Read,Grep"], REFUSED, NO_TOOL_LISTS),
    (
        "--permission-mode",
        ["--permission-mode", "acceptEdits"],
        REFUSED,
        "Codex's permissions are the role's sandbox and approval_policy settings",
    ),
    (
        "--dangerously-skip-permissions",
        ["--dangerously-skip-permissions"],
        REFUSED,
        "Codex's nearest, --dangerously-bypass-approvals-and-sandbox, drops its sandbox too; "
        "choose that in the role's sandbox and approval_policy settings",
    ),
    ("--chrome", ["--chrome"], REFUSED, BROWSER),
    ("--no-chrome", ["--no-chrome"], REFUSED, BROWSER),
    ("--resume", ["--resume", "0b1c"], REFUSED, SESSIONS),
    ("-r", ["-r", "0b1c"], REFUSED, SESSIONS),
    ("--continue", ["--continue"], REFUSED, SESSIONS),
    ("--session-id", ["--session-id=0b1c"], REFUSED, SESSIONS),
    ("--fork-session", ["--fork-session"], REFUSED, SESSIONS),
    ("--append-system-prompt", ["--append-system-prompt", "Be brief"], REFUSED, SYSTEM_PROMPT),
    (
        "--append-system-prompt-file",
        ["--append-system-prompt-file", "p.md"],
        REFUSED,
        SYSTEM_PROMPT,
    ),
    ("--system-prompt", ["--system-prompt", "Be brief"], REFUSED, SYSTEM_PROMPT),
    ("--mcp-config", ["--mcp-config", "mcp.json"], REFUSED, MCP_SERVERS),
    ("--strict-mcp-config", ["--strict-mcp-config"], REFUSED, MCP_SERVERS),
    ("--settings", ["--settings", "s.json"], REFUSED, SETTINGS),
    ("--setting-sources", ["--setting-sources", "user"], REFUSED, SETTINGS),
    ("--name", ["--name", "coder-1"], REFUSED, "Codex shows no session name"),
    ("--verbose", ["--verbose"], REFUSED, "Codex has no verbose switch"),
    ("--print", ["--print"], REFUSED, PRINT_MODE),
    ("--output-format", ["--output-format", "json"], REFUSED, PRINT_MODE),
    ("--input-format", ["--input-format", "text"], REFUSED, PRINT_MODE),
    ("--json-schema", ["--json-schema", "{}"], REFUSED, PRINT_MODE),
    ("--max-turns", ["--max-turns", "3"], REFUSED, PRINT_MODE),
    ("--no-session-persistence", ["--no-session-persistence"], REFUSED, PRINT_MODE),
    ("--bare", ["--bare"], REFUSED, PRINT_MODE),
]


def _row_id(row: tuple[str, list[str], str, object]) -> str:
    return f"{row[2]}: {' '.join(row[1])}"


@pytest.mark.parametrize(("flag", "args", "verdict", "codex"), TABLE, ids=map(_row_id, TABLE))
def test_codex_maps_passes_or_refuses_each_claude_code_flag_as_the_table_says(
    flag: str, args: list[str], verdict: str, codex: object
) -> None:
    """A mapped flag becomes its Codex form in its place (which Codex's own ``native_args`` keeps
    as it is) and one passed through stays as written, neither refused in any list; a refused
    flag's refusal is its one line, in every list, and ``native_args`` leaves the flag as it is
    (not dropped, not rewritten): the refusal is what stops it."""
    native = CODEX.native_args(list(args))
    if verdict == REFUSED:
        assert isinstance(codex, str)
        said = codex if flag in ("-c", "-p") else _foreign(flag, codex)
        for may_prompt in (False, True):
            assert CODEX.args_refusal(args, may_prompt=may_prompt) == said
        assert "\n" not in said
        assert native == args
        return
    assert native == (codex if verdict == MAPPED else args)
    assert CODEX.native_args(list(native)) == native
    for may_prompt in (False, True):
        assert CODEX.args_refusal(args, may_prompt=may_prompt) is None, args


def test_the_table_holds_every_flag_claude_codes_adapter_declares() -> None:
    """Each flag Claude Code's adapter names (its vocabulary, its options, its session flags)
    and ``-p`` has a row: a flag it declares later fails here until the table says what Codex
    makes of it."""
    claude = get_adapter("claude-code").capabilities
    sessions = claude.session_flags
    assert sessions is not None
    declared = {
        *claude.vocabulary.flags,
        *claude.value_options,
        *claude.switch_options,
        sessions.names,
        *sessions.resumes,
        *sessions.continues,
        *sessions.forks,
        "-p",
    }
    assert declared - {flag for flag, *_rest in TABLE} == set()


@pytest.mark.parametrize(("flag", "args", "verdict", "codex"), TABLE, ids=map(_row_id, TABLE))
def test_claude_code_passes_every_flag_of_the_table_through_as_it_did(
    flag: str, args: list[str], verdict: str, codex: object
) -> None:
    """Claude Code's adapter is unchanged: every list of the table reaches it as written, and
    it refuses none."""
    claude = get_adapter("claude-code")
    assert claude.native_args(list(args)) == args
    for may_prompt in (False, True):
        assert claude.args_refusal(args, may_prompt=may_prompt) is None


def test_codex_reads_rlxs_effort_as_the_setting_it_becomes_the_last_winning() -> None:
    """``native_choice`` reads ``--effort`` where ``native_args`` puts its ``-c``: the last of
    the two is the effort Codex runs, and the one rlx records. After Codex's ``--`` it is
    the prompt's, mapped by neither."""
    low = ["-c", 'model_reasoning_effort="low"']
    assert CODEX.native_choice([*low, "--effort", "high"]).effort == "high"
    assert CODEX.native_choice(["--effort", "high", *low]).effort == "low"
    assert CODEX.native_choice(["--effort=ultracode"]).effort == "ultracode"
    assert CODEX.native_choice(["--", "--effort", "high"]).effort is None
    assert CODEX.native_args(["--", "--effort", "high"]) == ["--", "--effort", "high"]


@pytest.mark.parametrize("args", [["--effort="], ["--effort", "a b"]])
def test_codex_native_args_refuse_an_effort_that_is_no_level(args: list[str]) -> None:
    """A hand ``rlx launch`` reports ``bad_effort`` rather than starting Codex on a value it
    cannot take."""
    with pytest.raises(BadEffortError):
        CODEX.native_args(args)


@pytest.mark.parametrize(
    ("bound", "role", "own", "native"),
    [
        (["--effort", "high"], [], [], 'model_reasoning_effort="high"'),
        ([], ["--effort=ultracode"], [], 'model_reasoning_effort="max"'),
        ([], [], ["--effort", "low"], 'model_reasoning_effort="low"'),
    ],
    ids=["binding", "role", "own"],
)
def test_a_codex_spawn_opens_its_window_with_rlxs_effort_in_codexs_form(
    tmux: FakeTmux,  # noqa: F811
    codex_on_path: Path,  # noqa: F811
    project: ProjectInfo,  # noqa: F811
    bound: list[str],
    role: list[str],
    own: list[str],
    native: str,
) -> None:
    """A Claude Code binding's ``--effort``, the role's, or the spawn's own: one window, whose
    line gives Codex the level as ``-c model_reasoning_effort`` and no ``--effort``."""
    _configure(bound, role)
    fleet_service.spawn(project, "coder", agent="codex", agent_args=own)
    command = _command(tmux)
    line = command[command.index("--") + 1 :]
    assert len(tmux.spawned) == 1 and _holds(line, ["-c", native]), command
    assert not [word for word in line if word.partition("=")[0] == "--effort"], command
