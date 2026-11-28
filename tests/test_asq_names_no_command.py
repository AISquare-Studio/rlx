"""asq never points its person to the command line (R13, #146).

The owner: "the whole purpose of TUI is to make it for users to never use the command line
since their manager is aware of it, so hide all of that". asq's own strings name no command
to type; the Doctor's buttons say what they do, never their command line; a row with no
button is handed to the manager (ruled Q2), which knows the command line, or says plainly
that a manager can fix it. The shell keeps every command: ``doctor``'s text, ``--help``,
errors printed in a terminal.

The guard knows every top-level command from the CLI itself (:data:`GUARD`), not from the
Doctor's hand list (``COMMAND``), which missed ``status``, ``service``, ``tidy`` and more
(reviewer #4085).
"""

from __future__ import annotations

import ast
import asyncio
import re
import subprocess
import sys
import tomllib
from collections.abc import Iterable, Iterator
from pathlib import Path
from typing import Any

import pytest
import typer
import typer.main
from textual.app import App, ComposeResult
from textual.widgets import Button, Static
from typer.core import TyperGroup

from aisquare.cli.app import app as cli_app
from aisquare.cli.ui.views import doctor as doctor_view
from aisquare.cli.ui.views.doctor import (
    COMMAND,
    NO_MANAGER_WAY,
    DoctorView,
    ask_text,
    render_checks,
    ui_way,
)
from aisquare.core import selfcli
from aisquare.models import CheckStatus, DoctorCheck
from aisquare.services.onboarding import FixCommand, FixResult

ROOT = Path(__file__).resolve().parents[1]
UI = ROOT / "src" / "aisquare" / "cli" / "ui"


def _programs() -> list[str]:
    """The names a person types rlx by: every console script that runs the CLI's ``main``."""
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))["project"]
    return sorted(name for name, at in project["scripts"].items() if at == "aisquare.cli.app:main")


def _commands(app: typer.Typer) -> set[str]:
    """Every top-level command and group ``app`` registers, hidden ones too, as the shell types
    it: read off the command group Typer builds from it, so one added is known at once."""
    group = typer.main.get_command(app)
    assert isinstance(group, TyperGroup), group
    return set(group.commands)


def _naming(programs: Iterable[str], commands: Iterable[str]) -> re.Pattern[str]:
    """``<program> <command>``, for each program and command; the longest of each first."""

    def either(words: Iterable[str]) -> str:
        return "|".join(sorted(map(re.escape, words), key=lambda word: (-len(word), word)))

    return re.compile(rf"\b(?:{either(programs)}) (?:{either(commands)})\b")


def _guard(app: typer.Typer) -> re.Pattern[str]:
    """The Doctor's ``COMMAND``, every rule of it (a package manager's, tmux's, a backticked
    flag), with each of ``app``'s commands after each of the CLI's names besides."""
    every = _naming(_programs(), _commands(app))
    return re.compile(f"{every.pattern}|{COMMAND.pattern}")


GUARD = _guard(cli_app)
"""What a command to type looks like in asq's text, as this guard reads it."""

PRINTED_IN_A_TERMINAL = {"instance_gate.py"}
"""asq's files whose text is printed in the terminal, before asq draws or after it quits:
the person is at a shell there, and a command is the way."""

BUILDERS = re.compile(
    r"\b(?:prog\.(?:line|words|command)|install_route\.command_line|fleet_command|"
    r"onboard_command|forget_command|config_dir_command)\("
)
"""The functions that spell a command; asq's views call none (``prog.word()`` names the product)."""

BUILDERS_ALLOWED = {
    ("app.py", "install_route.command_line("),  # run_ui's hand-off, printed after asq quits
}


def _docstrings(tree: ast.AST) -> set[int]:
    """The literals no person reads on screen: docstrings, attributes' doc strings, and the
    sources of patterns (``re.compile``), this guard's own among them."""
    out: set[int] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant):
            out.add(id(node.value))  # a docstring, or an attribute's: read by developers
        if (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and node.func.attr == "compile"
        ):
            out.update(id(inner) for inner in ast.walk(node) if isinstance(inner, ast.Constant))
    return out


def _literals() -> Iterator[tuple[str, int, str]]:
    for path in sorted(UI.rglob("*.py")):
        if path.name in PRINTED_IN_A_TERMINAL:
            continue
        tree = ast.parse(path.read_text(encoding="utf-8"))
        skip = _docstrings(tree)
        for node in ast.walk(tree):
            if (
                isinstance(node, ast.Constant)
                and isinstance(node.value, str)
                and id(node) not in skip
            ):
                yield path.relative_to(UI).as_posix(), node.lineno, node.value


# --- the guard ---------------------------------------------------------------------------------


def test_no_asq_string_names_a_command_to_type() -> None:
    """Every string asq shows — f-strings' literal parts included — is free of a command."""
    found = [
        f"{name}:{line}: {GUARD.search(text).group(0)!r} in {text[:70]!r}"  # type: ignore[union-attr]
        for name, line, text in _literals()
        if GUARD.search(text)
    ]
    assert found == []


def test_the_guard_catches_what_it_is_for() -> None:
    for said in (
        "start one: rlx fleet spawn --project x manager",
        "aisquare doctor --live",
        "use `--project amber`",
        "tmux -L asq attach -t =asq-x",
        "install it: npm install -g @openai/codex",
        "curl -fsSL https://claude.ai/install.sh | bash",
        "see rlx status",
        "asq service install starts it at login",
        "relax tidy --dry-run",
    ):
        assert GUARD.search(said), said
    for prose in (
        "rlx — keys",
        "Restart on its sidebar row",
        "rlx's doctor: its tmux check",
        "rlx statuses",
        "rlx's service",
    ):
        assert not GUARD.search(prose), prose


def test_the_guard_knows_every_command_the_cli_has() -> None:
    """Reviewer #4085: the hand list missed ``status``, ``service`` and more, so asq's text
    could name them unseen. Each command the shell lists, after each of its names, is one."""
    root = typer.main.get_command(cli_app)
    assert isinstance(root, TyperGroup), root
    listed = sorted(root.commands)
    visible = [name for name in listed if not root.commands[name].hidden]

    assert {"status", "service", "tidy", "fleet", "doctor"} <= set(visible), visible
    assert {"rlx", "asq", "aisquare"} <= set(_programs())
    missed = [
        f"{program} {name}"
        for program in _programs()
        for name in listed
        if not GUARD.search(f"say {program} {name} here")
    ]
    assert missed == []


@pytest.fixture
def stand_ins(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    """A command, a hidden one and a group, registered on the real CLI: on copies of its lists,
    which the monkeypatch puts back after."""
    monkeypatch.setattr(cli_app, "registered_commands", list(cli_app.registered_commands))
    monkeypatch.setattr(cli_app, "registered_groups", list(cli_app.registered_groups))
    group = typer.Typer()
    group.command("go")(lambda: None)
    cli_app.command("zz-stand-in")(lambda: None)
    cli_app.command("zz-quiet", hidden=True)(lambda: None)
    cli_app.add_typer(group, name="zz-group")
    return ["zz-group", "zz-quiet", "zz-stand-in"]


def test_a_command_the_cli_gains_is_known_with_no_hand_list(stand_ins: list[str]) -> None:
    """A command or group registered on the real CLI is known from then on, hidden or not."""
    before, after = GUARD, _guard(cli_app)

    for said in ("rlx zz-stand-in", "asq zz-quiet --now", "aisquare zz-group go"):
        assert after.search(said) and not before.search(said), said
    assert set(stand_ins) <= _commands(cli_app)


# --- the Doctor's own list (#69) ---------------------------------------------------------------


def _drift(app: typer.Typer) -> tuple[list[str], list[str]]:
    """(``app``'s commands the Doctor's list lacks, the names on it that ``app`` has not)."""
    commands, listed = _commands(app), set(doctor_view.RLX_COMMANDS)
    return sorted(commands - listed), sorted(listed - commands)


def test_the_doctor_s_command_list_is_the_cli_s() -> None:
    """The Doctor's ``COMMAND`` decides what asq says of a fix, in asq, which cannot read the
    CLI's commands without loading all of it (#45): its hand list is held to the CLI's here,
    hidden commands too, and its programs to the console scripts (#69)."""
    assert _drift(cli_app) == ([], [])
    assert len(set(doctor_view.RLX_COMMANDS)) == len(doctor_view.RLX_COMMANDS), "each once"
    assert sorted(doctor_view.PROGRAMS) == _programs()


def test_a_command_the_cli_gains_without_the_doctor_s_list_is_drift(stand_ins: list[str]) -> None:
    assert _drift(cli_app) == (stand_ins, [])


def test_asq_loads_no_cli_for_the_doctor_s_list() -> None:
    """Why the list is by hand: asq's module, read in a fresh interpreter, loads no CLI app."""
    code = "import sys, aisquare.cli.ui.app; print('aisquare.cli.app' in sys.modules)"
    result = subprocess.run(
        [sys.executable, "-c", code], capture_output=True, text=True, timeout=120, cwd=ROOT
    )
    assert result.returncode == 0, result.stderr[-3000:]
    assert result.stdout.split() == ["False"], result.stdout


def _fixes_naming_each_command() -> list[str]:
    """A fix naming each command of the CLI's, after each of its names: read off the CLI."""
    return [
        f"Free it: {program} {name} --now"
        for program in _programs()
        for name in sorted(_commands(cli_app))
    ]


def test_asq_s_doctor_says_its_own_way_for_a_fix_naming_any_command() -> None:
    """``ui_way``: a fix naming a command is never said as it is: the button it makes, the Ask
    beside it, or plain words (#69: one naming ``rlx tidy`` was said as it is)."""
    said = {
        fix: (
            ui_way(row, {}),
            ui_way(row, {}, manager="api's manager"),
            ui_way(row, {"disk": "Free space"}),
        )
        for fix in _fixes_naming_each_command()
        for row in [DoctorCheck(name="disk", status=CheckStatus.warn, detail="full", fix=fix)]
    }
    ways = (NO_MANAGER_WAY, "Ask api's manager below to fix it", "Free space below fixes it")
    assert [f"{fix}: {way}" for fix, way in said.items() if way != ways] == []
    plain = DoctorCheck(name="disk", status=CheckStatus.warn, detail="full", fix="Wait a day")
    assert ui_way(plain, {}) == "Wait a day", "words with no command are said as they are"


def test_asq_s_explainability_page_shows_no_probe_fix_naming_any_command(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The probe's remediation, under the probe, only when it names no command (#69)."""
    from aisquare.cli.ui.views import explainability as view
    from aisquare.services import explainability_ops as ops

    def probe(remediation: str) -> str:
        state = ops.ProxyState(summary="down", severity=CheckStatus.warn, remediation=remediation)
        monkeypatch.setattr(ops, "proxy_state", lambda target, on: state)
        return dict(view.status_report().rows)["probe"]

    assert "Start the SDK's proxy" in probe("Start the SDK's proxy"), "plain words are shown"
    shown = [fix for fix in _fixes_naming_each_command() if fix in probe(fix)]
    assert shown == []


def test_asq_calls_no_function_that_spells_a_command() -> None:
    """f-strings built with a command builder are not literals: asq's views call none, but for
    the shell's own prints before asq draws or after it quits."""
    found = []
    for path in sorted(UI.rglob("*.py")):
        if path.name in PRINTED_IN_A_TERMINAL:
            continue
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            match = BUILDERS.search(line)
            if match and (path.name, match.group(0)) not in BUILDERS_ALLOWED:
                found.append(f"{path.relative_to(UI).as_posix()}:{number}: {stripped[:80]}")
    assert found == []


# --- the Doctor's buttons ----------------------------------------------------------------------


def test_a_fix_button_says_what_it_does_and_which_home_never_its_command() -> None:
    home = str(Path.home())
    connect = FixCommand("claude-code", ("agents", "connect", "claude-code"), "machine", "")
    # As a fix spells it on this platform, and with "/" after the home, as one written by
    # hand on Windows has it: both shown from "~", in this platform's separators (#517).
    native = str(Path.home() / ".rlx" / "accounts" / "codex" / "4")
    slot = FixCommand(
        "claude-code",
        ("agents", "connect", "codex", "--config-dir", f"{home}/.aisquare/codex-accounts/3"),
        "machine",
        "",
    )
    slot_4 = FixCommand(
        "claude-code", ("agents", "connect", "codex", "--config-dir", native), "machine", ""
    )
    elsewhere = FixCommand(
        "c", ("agents", "connect", "claude-code", "--config-dir", "/srv/x"), "machine", ""
    )
    refresh = FixCommand("p", ("project", "onboard", "--refresh"), "project", "")
    fix_all = FixCommand("d", ("doctor", "--fix", "--yes"), "machine", "")
    sign_in = FixCommand("a", ("accounts", "run", "3"), "machine", "", interactive=True)

    assert connect.title == "Connect Claude Code"
    assert slot.title == f"Connect Codex ({Path('~', '.aisquare', 'codex-accounts', '3')})"
    assert slot_4.title == f"Connect Codex ({Path('~', '.rlx', 'accounts', 'codex', '4')})"
    assert elsewhere.title == "Connect Claude Code (/srv/x)"
    assert refresh.title == "Refresh this project"
    assert fix_all.title == "Fix what the checks can"
    assert sign_in.title == "Sign in account 3"
    assert connect.label.endswith("agents connect claude-code"), "the shell's form stays"


def test_a_fix_s_result_says_its_title_never_its_command() -> None:
    fix = FixCommand("c", ("agents", "connect", "claude-code"), "machine", "")
    failed = FixResult(fix, 1, reason=f"{fix.label} failed: not_installed")

    assert FixResult(fix, 0).said() == "✓ Connect Claude Code"
    assert failed.said() == "✗ Connect Claude Code: Connect Claude Code failed: not_installed"
    assert failed.summary().startswith(f"✗ {fix.label}"), "the shell's summary stays"


# --- what a row says, and Ask the manager -----------------------------------------------------

WARN = DoctorCheck(
    name="fleet watcher",
    status=CheckStatus.warn,
    detail="not running",
    fix="rlx watch --stop, then rlx watch --detach",
)
CONNECT = DoctorCheck(
    name="claude-code",
    status=CheckStatus.warn,
    detail="hooks are missing",
    fix="(Re)connect it: rlx agents connect claude-code",
)
OWN_WAY = WARN.model_copy(update={"name": "fleet-manager", "ui_fix": "Restart it from its row"})
OK = DoctorCheck(name="python", status=CheckStatus.ok, detail="3.12", fix="never shown")


def test_a_row_says_rlx_s_own_way_never_the_shell_s_command() -> None:
    buttons = {"claude-code": "Connect Claude Code"}

    assert ui_way(OWN_WAY, buttons) == "Restart it from its row"
    assert ui_way(CONNECT, buttons) == "Connect Claude Code below fixes it"
    assert ui_way(WARN, buttons, manager="api's manager") == "Ask api's manager below to fix it"
    assert ui_way(WARN, buttons) == NO_MANAGER_WAY
    assert ui_way(OK, buttons) is None
    report = render_checks([OK, CONNECT, WARN, OWN_WAY], buttons).plain
    assert "rlx " not in report and "never shown" not in report


def test_the_manager_is_handed_the_shell_s_fix_whole() -> None:
    said = ask_text(WARN)
    assert "fleet watcher check warns: not running" in said
    assert "Its fix: rlx watch --stop, then rlx watch --detach" in said


class Host(App[None]):
    def __init__(self, view: DoctorView) -> None:
        super().__init__()
        self.view = view
        self.asked: list[tuple[str, Path | None]] = []

    def compose(self) -> ComposeResult:
        yield self.view

    def on_doctor_view_ask_manager(self, event: DoctorView.AskManager) -> None:
        self.asked.append((event.check.name, event.cwd))


def _doctor(
    manager: str | None, tmp_path: Path, rows: list[DoctorCheck] | None = None
) -> tuple[list[str], list[tuple[str, Path | None]], str]:
    async def drive() -> tuple[list[str], list[tuple[str, Path | None]], str]:
        view = DoctorView(cwd=tmp_path, run=selfcli.run, manager=lambda cwd: manager)
        host = Host(view)
        async with host.run_test(size=(140, 40)) as pilot:
            view.show([OK, CONNECT, WARN, OWN_WAY] if rows is None else rows)
            await pilot.pause()
            await pilot.pause()
            labels = [str(b.label) for b in view.query("#doctor-fixes Button").results(Button)]
            ask = next(iter(view.asks), None)
            if ask is not None:
                await pilot.click(f"#{ask}")
                await pilot.pause()
            report = str(view.query_one("#doctor-report", Static).render())
            return labels, host.asked, report

    return asyncio.run(drive())


def test_with_a_manager_a_row_with_no_button_is_asked_of_it(tmp_path: Path) -> None:
    labels, asked, report = _doctor("api's manager", tmp_path)

    assert labels == ["Connect Claude Code", "Ask the manager: fleet watcher"]
    assert asked == [("fleet watcher", tmp_path)], "the row, and the report's own project"
    assert "→ Ask api's manager below to fix it" in report
    assert "→ Restart it from its row" in report, "a row with rlx's own way is not asked"


def test_with_no_manager_there_is_no_ask_and_the_row_says_so(tmp_path: Path) -> None:
    labels, asked, report = _doctor(None, tmp_path)

    assert labels == ["Connect Claude Code"] and asked == []
    assert f"→ {NO_MANAGER_WAY}" in report


def test_a_fix_naming_a_command_the_old_list_missed_is_asked_of_the_manager(
    tmp_path: Path,
) -> None:
    """The Ask button is offered for a fix that names a command (#69: ``tidy`` and ``service``
    were not known, and their rows were said as they are, with no Ask)."""
    rows = [
        DoctorCheck(name="disk", status=CheckStatus.warn, detail="full", fix="rlx tidy frees it"),
        DoctorCheck(name="svc", status=CheckStatus.warn, detail="off", fix="asq service install"),
    ]
    labels, asked, report = _doctor("api's manager", tmp_path, rows)

    assert labels == ["Ask the manager: disk", "Ask the manager: svc"]
    assert asked == [("disk", tmp_path)]
    assert report.count("→ Ask api's manager below to fix it") == 2, report
    assert "tidy" not in report and "service" not in report, report


def test_a_fix_with_no_command_is_said_as_it_is_and_is_not_asked() -> None:
    prose = DoctorCheck(
        name="workspace-credits",
        status=CheckStatus.warn,
        detail="low",
        fix="Top up the workspace in the dashboard before spawning a fleet into it",
    )
    assert ui_way(prose, {}, manager="api's manager") == prose.fix


def test_the_tmux_row_links_its_page_in_asq_and_keeps_its_command_for_the_shell() -> None:
    from aisquare.services import diagnostics

    class Missing:
        def available(self) -> bool:
            return False

    row = diagnostics._check_tmux(Missing())  # type: ignore[arg-type]
    assert row.ui_fix is not None and diagnostics.TMUX_INSTALL_PAGE in row.ui_fix
    assert row.fix is not None and row.fix.startswith("Install it: "), "the shell keeps it"


#: (platform, /etc/os-release): macOS, a Debian-family Linux, a Fedora one, one the hint table
#: does not know (it then names every package manager), and Windows (the fleet's WSL2, apt).
_PLATFORMS: dict[str, tuple[str, str | None]] = {
    "macos": ("darwin", None),
    "debian": ("linux", 'ID=ubuntu\nID_LIKE="debian"\n'),
    "fedora": ("linux", "ID=fedora\n"),
    "unknown-linux": ("linux", "ID=arch\n"),
    "windows-wsl": ("win32", None),
}


class _Tmux:
    """A tmux seam at ``version`` (``None``: not installed)."""

    def __init__(self, version: tuple[int, int] | None) -> None:
        self._version = version

    def available(self) -> bool:
        return self._version is not None

    def version(self) -> tuple[int, int] | None:
        return self._version

    def binary(self) -> str:
        return "/usr/bin/tmux"


@pytest.mark.parametrize("where", sorted(_PLATFORMS))
def test_the_install_rows_name_no_command_in_asq_on_any_platform(
    where: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #503, m1: on Linux and WSL ``install_hint`` names apt or dnf, which COMMAND did
    not know, so asq's Doctor showed "Upgrade it: apt install tmux" and "Install GitHub CLI: apt
    install gh, then: gh auth login" as plain words. On every platform each row's shell fix keeps
    its command, which COMMAND knows for one, and what asq shows (:func:`ui_way`) names none."""
    from aisquare.services import diagnostics

    platform, release = _PLATFORMS[where]
    os_release = tmp_path / "os-release"
    if release is not None:
        os_release.write_text(release, encoding="utf-8")
    real = diagnostics.install_hint

    def hint(package: str) -> str:
        return real(package, platform=platform, os_release=os_release)

    def nowhere(name: str, *args: Any, **kwargs: Any) -> None:
        return None

    monkeypatch.setattr(diagnostics, "install_hint", hint)
    monkeypatch.setattr("shutil.which", nowhere)
    rows = [
        diagnostics._check_tmux(_Tmux(None)),  # type: ignore[arg-type]
        diagnostics._check_tmux(_Tmux((3, 0))),  # type: ignore[arg-type]
        diagnostics._check_gh(),
    ]
    assert [row.status for row in rows] == [CheckStatus.warn] * 3
    pages = [
        diagnostics.TMUX_INSTALL_PAGE,
        diagnostics.TMUX_INSTALL_PAGE,
        diagnostics.GH_INSTALL_PAGE,
    ]
    for row, page in zip(rows, pages, strict=True):
        assert row.fix is not None and COMMAND.search(row.fix), (row.name, row.fix)
        way = ui_way(row, {})
        assert way is not None and not GUARD.search(way), (row.name, way)
        assert page in way, f"{row.name}: asq links its install page: {way}"


def test_the_gh_rows_sign_in_note_names_its_command_at_a_shell_and_none_in_asq(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #503, m1: an ok gh row's detail ended "(no login found: gh auth login)" in asq
    too. The shell keeps the command; asq says what is missing (``detail_advice``)."""
    from aisquare.core import program as prog
    from aisquare.services import diagnostics

    def found(name: str, *args: Any, **kwargs: Any) -> str:
        return "/usr/bin/gh"

    for variable in ("GH_TOKEN", "GITHUB_TOKEN", prog.IN_APP_ENV):
        monkeypatch.delenv(variable, raising=False)
    monkeypatch.setattr(diagnostics, "_gh_config_dir", lambda: tmp_path)  # no hosts.yml
    monkeypatch.setattr("shutil.which", found)
    shell = diagnostics._check_gh()
    assert shell.status is CheckStatus.ok and "(no login found: gh auth login)" in shell.detail
    monkeypatch.setenv(prog.IN_APP_ENV, "1")
    app = diagnostics._check_gh()
    assert app.detail.endswith("(not signed in to GitHub CLI)"), app.detail
    assert not GUARD.search(app.detail), app.detail


def test_no_doctor_row_s_asq_wording_names_a_command() -> None:
    """Its ``ui_fix``, and what asq's Doctor says under it (:func:`ui_way`): a ``fix`` naming a
    command the Doctor's own pattern does not know would be said as it is."""
    from aisquare.services import diagnostics

    rows = diagnostics.doctor()
    assert rows, "the doctor ran"
    assert [row.name for row in rows if row.ui_fix and GUARD.search(row.ui_fix)] == []
    said = {row.name: ui_way(row, {}) for row in rows}
    assert [f"{name}: {way}" for name, way in said.items() if way and GUARD.search(way)] == []


# --- every ui_fix the source can build (#69, tester #4105) --------------------------------------

SRC = ROOT / "src" / "aisquare"

SPELLERS = frozenset(
    {
        "word",
        "words",
        "line",
        "command",
        "command_line",
        "fleet_command",
        "onboard_command",
        "forget_command",
        "config_dir_command",
        "trust_command",
        "claude_plugin_command",
    }
)
"""The calls that spell a command (``core.program``'s and the services' builders)."""


def _ui_fix_sites(tree: ast.Module) -> Iterator[tuple[ast.expr, ast.AST | None]]:
    """(what a ``ui_fix`` is set to, the function around it) for every place ``tree`` sets one:
    a ``ui_fix=`` argument, a ``{"ui_fix": …}`` update, an ``….ui_fix = …`` assignment."""

    def visit(node: ast.AST, around: ast.AST | None) -> Iterator[tuple[ast.expr, ast.AST | None]]:
        if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
            around = node
        if isinstance(node, ast.keyword) and node.arg == "ui_fix":
            yield node.value, around
        if isinstance(node, ast.Dict):
            for key, value in zip(node.keys, node.values, strict=True):
                if isinstance(key, ast.Constant) and key.value == "ui_fix":
                    yield value, around
        if isinstance(node, ast.Assign | ast.AnnAssign) and node.value is not None:
            targets = node.targets if isinstance(node, ast.Assign) else [node.target]
            if any(isinstance(t, ast.Attribute) and t.attr == "ui_fix" for t in targets):
                yield node.value, around
        for child in ast.iter_child_nodes(node):
            yield from visit(child, around)

    yield from visit(tree, None)


def _assigned(scope: ast.AST, name: str) -> list[ast.expr]:
    """What ``name`` is set to in ``scope``: anywhere in a function, at a module's top level."""
    found: list[ast.expr] = []
    for node in scope.body if isinstance(scope, ast.Module) else ast.walk(scope):
        if isinstance(node, ast.Assign):
            targets = node.targets
        elif isinstance(node, ast.AnnAssign | ast.AugAssign):
            targets = [node.target]
        else:
            continue
        if node.value is not None and any(
            isinstance(t, ast.Name) and t.id == name for t in targets
        ):
            found.append(node.value)
    return found


def _callee(call: ast.Call) -> str | None:
    if isinstance(call.func, ast.Attribute):
        return call.func.attr
    return call.func.id if isinstance(call.func, ast.Name) else None


def _ui_fix_findings(tree: ast.Module) -> list[tuple[int, str]]:
    """(line, why) for each ``ui_fix`` in ``tree`` that names a command: its literals and
    f-strings (a placeholder read as ``{…}``), the module's constants and the function's own
    variables it names, and what the module's own functions it calls return."""
    functions = {
        node.name: node
        for node in ast.walk(tree)
        if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef)
    }
    found: list[tuple[int, str]] = []

    def read(expr: ast.expr, around: ast.AST | None, seen: set[tuple[int, str]]) -> None:
        for node in ast.walk(expr):
            if isinstance(node, ast.Constant) and isinstance(node.value, str):
                said(node.lineno, node.value)
            elif isinstance(node, ast.JoinedStr):
                parts = [v.value if isinstance(v, ast.Constant) else "{…}" for v in node.values]
                said(node.lineno, "".join(map(str, parts)))
            elif isinstance(node, ast.Call) and (callee := _callee(node)) is not None:
                if callee in SPELLERS:
                    found.append((node.lineno, f"calls {callee}()"))
                elif callee in functions and (id(tree), callee) not in seen:
                    seen.add((id(tree), callee))
                    helper = functions[callee]
                    for inner in ast.walk(helper):
                        if isinstance(inner, ast.Return) and inner.value is not None:
                            read(inner.value, helper, seen)
            elif isinstance(node, ast.Name):
                for scope in (around, tree):
                    if scope is not None and (id(scope), node.id) not in seen:
                        seen.add((id(scope), node.id))
                        for value in _assigned(scope, node.id):
                            read(value, scope if scope is around else None, seen)

    def said(line: int, text: str) -> None:
        match = GUARD.search(text)
        if match:
            found.append((line, f"{match.group(0)!r} in {text[:70]!r}"))

    for expr, around in _ui_fix_sites(tree):
        read(expr, around, set())
    return found


def test_no_ui_fix_the_source_can_build_names_a_command() -> None:
    """Tester #4105: the doctor rows a sandbox makes set no ``ui_fix``, so none was read. What
    asq says under a row (``ui_way``) is its ``ui_fix`` as it is: each is read from the source,
    in every module that sets one."""
    found = []
    for path in sorted(SRC.rglob("*.py")):
        text = path.read_text(encoding="utf-8")
        if "ui_fix" in text:
            rel = path.relative_to(SRC).as_posix()
            found += [f"{rel}:{line}: {why}" for line, why in _ui_fix_findings(ast.parse(text))]
    assert found == []


UI_FIX_SHAPES = {
    "a literal": 'row = _warn("x", "d", "f", ui_fix="Run rlx tidy")\n',
    "an f-string": 'row = _warn("x", "d", "f", ui_fix=f"Run rlx {what} or rlx status")\n',
    "a constant": 'WAY = "rlx service install"\nrow = _warn("x", "d", "f", ui_fix=WAY)\n',
    "a variable": (
        "def check():\n"
        "    way = 'asq doctor --fix'\n"
        '    return _warn("x", "d", "f", ui_fix=way)\n'
    ),
    "a helper": (
        "def _way(n):\n"
        "    if n:\n"
        "        return 'Run aisquare tidy'\n"
        "    return 'Wait'\n"
        'row = _warn("x", "d", "f", ui_fix="; ".join([_way(1)]))\n'
    ),
    "an update": 'row = old.model_copy(update={"ui_fix": "relax status"})\n',
    "an assignment": 'row.ui_fix = "rlx why"\n',
    "a speller": 'row = _warn("x", "d", "f", ui_fix=f"{prog.word()} tidy")\n',
}


@pytest.mark.parametrize("shape", sorted(UI_FIX_SHAPES))
def test_the_ui_fix_sweep_reads_each_way_a_ui_fix_is_built(shape: str) -> None:
    assert _ui_fix_findings(ast.parse(UI_FIX_SHAPES[shape])), shape
    plain = re.sub(
        r"(?:rlx|asq|aisquare|relax) \w+|\{prog\.word\(\)\} tidy", "Restart", UI_FIX_SHAPES[shape]
    )
    assert _ui_fix_findings(ast.parse(plain)) == [], plain


@pytest.mark.parametrize(
    ("watching", "every", "deletes"),
    [(True, 60, True), (True, 15, True), (False, 60, True), (True, 0, True), (True, 60, False)],
)
def test_the_disk_row_s_asq_wording_names_no_command(
    monkeypatch: pytest.MonkeyPatch, watching: bool, every: float, deletes: bool
) -> None:
    """#60, manager #3817: the ``disk`` row's in-app way, with GBs to free and a folder it
    cannot read, in every state: the watcher frees it, isn't running, the job is off, native
    Windows only reports. Never a shell or a command; the shell's fix keeps ``tidy``, which the
    sweep's pattern knows."""
    from aisquare.core.config import load_config, save_config
    from aisquare.services import diagnostics, tidy
    from aisquare.services import watch as watch_service

    use = tidy.DiskUse(
        worktrees=20 * 10**9,
        build_caches=6 * 10**9,
        session_temp=8 * 10**9,
        use_whole=False,
        would_free=3 * 10**9,
        free_whole=False,
        unreadable=(Path("/nowhere/.aisquare-worktrees"),),
    )
    monkeypatch.setattr(tidy, "disk_use", lambda **kwargs: use)
    monkeypatch.setattr(tidy, "DELETES", deletes)
    monkeypatch.setattr(
        watch_service, "watch_status", lambda: watch_service.Status(1 if watching else None, None)
    )
    config = load_config()
    config.tidy.every_minutes = every
    save_config(config)

    rows = [row for row in diagnostics.doctor() if row.name == "disk"]

    assert [row.status for row in rows] == [CheckStatus.warn]
    [row] = rows
    assert row.ui_fix and not GUARD.search(row.ui_fix), row.ui_fix
    assert not GUARD.search(row.detail), row.detail
    said = ui_way(row, {})
    assert said == row.ui_fix and not GUARD.search(said), said
    assert row.fix and GUARD.search(row.fix), "the shell's fix names its command"
    assert COMMAND.search(row.fix), "the Doctor's own pattern knows it, so asq never says it"
