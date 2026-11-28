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
import string
import subprocess
import sys
import tomllib
from collections.abc import Callable, Iterable, Iterator
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


# --- the phone page (web/remote/app.js) --------------------------------------------------------

PHONE_PAGE = ROOT / "src" / "aisquare" / "web" / "remote" / "app.js"
"""The page Remote serves to a phone: a person's, who has no shell there at all."""

_JS_TOKEN = re.compile(
    r"//[^\n]*|/\*.*?\*/"  # a comment, a developer's: skipped
    r"|\"(?:\\.|[^\"\\\n])*\"|'(?:\\.|[^'\\\n])*'|`(?:\\.|[^`\\])*`",  # a string, a person's
    re.S,
)
"""A comment or a string literal of the page's script. The leftmost wins, so a quote inside a
comment opens no string and a ``//`` inside a string (a URL) opens no comment."""


def _js_strings(source: str) -> Iterator[tuple[int, str]]:
    """Each string literal of ``source``, its quotes off, with its line; comments skipped."""
    for match in _JS_TOKEN.finditer(source):
        token = match.group(0)
        if token[0] in "\"'`":
            yield source.count("\n", 0, match.start()) + 1, token[1:-1]


def _command_paths(app: typer.Typer) -> list[tuple[str, ...]]:
    """Every command the CLI runs, as the words typed after the program: ``("fleet", "resume")``,
    hidden ones too, read off the command group Typer builds (as :func:`_commands` is)."""

    def walk(command: object, path: tuple[str, ...]) -> Iterator[tuple[str, ...]]:
        children = getattr(command, "commands", None)
        if not children:
            yield path
            return
        for name, child in children.items():
            yield from walk(child, (*path, name))

    return list(walk(typer.main.get_command(app), ()))


def _alternatives(words: Iterable[str]) -> str:
    return "|".join(sorted(map(re.escape, words), key=lambda word: (-len(word), word)))


def _phone_guard(app: typer.Typer) -> re.Pattern[str]:
    """What a command looks like on the phone page (R13; finding 3 of the 0.10.2 gate, #4295):
    the guard's own, and what slipped past it there. A command without its program ("wait for
    fleet resume"); a subcommand no prose names, any with a hyphen (``regenerate-password``);
    a long flag in a sentence (``--new-link``), never a CSS property's ``--cols`` alone."""
    paths = _command_paths(app)
    pairs = {" ".join(path[-2:]) for path in paths if len(path) >= 2}
    hyphenated = {word for path in paths for word in path if "-" in word}
    return re.compile(
        rf"{_guard(app).pattern}"
        rf"|\b(?:{_alternatives(pairs)})\b"
        rf"|(?<![\w-])(?:{_alternatives(hyphenated)})(?![\w-])"
        r"|(?<=\s)--[a-z][\w-]*"
    )


PHONE_GUARD = _phone_guard(cli_app)


def test_the_phone_page_names_no_command_to_type() -> None:
    """The phone page's strings say where in rlx to do a thing, never a command: a paused agent
    is Resumed in rlx, and a link changes when New link is pressed on rlx's Remote page."""
    found = [
        f"app.js:{line}: {PHONE_GUARD.search(text).group(0)!r} in {text[:70]!r}"  # type: ignore[union-attr]
        for line, text in _js_strings(PHONE_PAGE.read_text(encoding="utf-8"))
        if PHONE_GUARD.search(text)
    ]
    assert found == []


def test_the_phone_guard_catches_what_it_is_for() -> None:
    source = (
        'const S = {paused: "paused: its claims, worktree and session wait for fleet resume"};\n'
        "const OFF = ['the link changes with regenerate-password --new-link.'];\n"
        "const SEE = `run rlx remote status`; // a comment's own: rlx fleet ls, --json\n"
        '/* "fleet resume" in a block comment */ const URL = "https://example.com/a//b";\n'
        'el.style.setProperty("--cols", n); const PROSE = "Open the link the machine shows now.";\n'
    )
    assert [text for _, text in _js_strings(source)] == [
        "paused: its claims, worktree and session wait for fleet resume",
        "the link changes with regenerate-password --new-link.",
        "run rlx remote status",  # the comment after it is no string
        "https://example.com/a//b",  # whole: its // opens no comment
        "--cols",
        "Open the link the machine shows now.",
    ]
    caught = [line for line, text in _js_strings(source) if PHONE_GUARD.search(text)]
    assert caught == [1, 2, 3], "each command, and never a comment's, a URL, a CSS name or prose"
    for prose in (
        "paused: Resume in rlx brings it back",
        "the link changes when ngrok restarts without a static domain, and with New link in rlx.",
        "Turn Remote on again on the machine, then retry.",
        "--cols",
    ):
        assert not PHONE_GUARD.search(prose), prose


def test_what_the_machine_says_on_the_phone_names_no_command() -> None:
    """The server's own words a phone shows: the unlock screen's lockout (a 429) and the alert
    pushed when the unlock budget is spent, which named ``rlx remote regenerate-password
    --new-link``; and a paused row's detail, which names the shell's resume, so the Fleet tab
    says its own words instead (manager #4320 on #71). The guard sees what each said."""
    from aisquare.services import fleet as fleet_service
    from aisquare.services import remote_server

    for said in (remote_server.locked_out(), remote_server.lockout_alert()):
        assert not PHONE_GUARD.search(said), said
        assert remote_server.NEW_LINK_IN_RLX in said, said
    assert PHONE_GUARD.search("On the machine: rlx remote regenerate-password --new-link")
    assert PHONE_GUARD.search(fleet_service.PAUSED_DETAIL), "why the Fleet tab says its own"


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

PLACEHOLDER = "rlx"
"""What a placeholder the sweep cannot trace is read as saying: a program's name, so a command
after it (``f"{PRODUCT} tidy"``) is still one (reviewer #4214)."""

MOST_WAYS = 256
"""The most texts one ``ui_fix`` is read as saying; past them it cannot be read."""

TEXT_METHODS: dict[str, Callable[[str], str]] = {
    "upper": str.upper,
    "lower": str.lower,
    "capitalize": str.capitalize,
    "title": str.title,
    "strip": str.strip,
    "lstrip": str.lstrip,
    "rstrip": str.rstrip,
}
"""A string's own methods a ``ui_fix`` may call on its text: each read as it acts."""

_PERCENT = re.compile(r"%(?:\((\w+)\))?[#0 +-]*(?:\*|\d+)?(?:\.(?:\*|\d+))?([a-zA-Z%])")

Scope = ast.FunctionDef | ast.AsyncFunctionDef | None
"""The function a value is read in, or ``None`` for the module's top level."""


class _Unreadable(Exception):
    """A part of a ``ui_fix`` the sweep cannot trace to text: it fails closed (reviewer #4214)."""

    def __init__(self, node: ast.AST, what: str) -> None:
        super().__init__(f"cannot read {what}")
        self.line: int = getattr(node, "lineno", 0)


def _ui_fix_sites(tree: ast.Module) -> Iterator[tuple[ast.expr, Scope]]:
    """(what a ``ui_fix`` is set to, the function around it) for every place ``tree`` sets one:
    a ``ui_fix=`` argument, a ``{"ui_fix": …}`` update, an ``….ui_fix = …`` assignment, a
    ``setattr(…, "ui_fix", …)``."""

    def visit(node: ast.AST, around: Scope) -> Iterator[tuple[ast.expr, Scope]]:
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
        if (
            isinstance(node, ast.Call)
            and _callee(node) == "setattr"
            and len(node.args) == 3
            and isinstance(node.args[1], ast.Constant)
            and node.args[1].value == "ui_fix"
        ):
            yield node.args[2], around
        for child in ast.iter_child_nodes(node):
            yield from visit(child, around)

    yield from visit(tree, None)


def _assigned(scope: ast.AST, name: str, *, added: bool = True) -> list[ast.expr]:
    """What ``name`` is set to in ``scope``, anywhere in a function or at a module's top
    level; ``added``: what ``+=`` adds to it too."""
    found: list[ast.expr] = []
    for node in scope.body if isinstance(scope, ast.Module) else ast.walk(scope):
        if isinstance(node, ast.Assign):
            targets = node.targets
        elif isinstance(node, ast.AnnAssign) or (isinstance(node, ast.AugAssign) and added):
            targets = [node.target]
        else:
            continue
        if node.value is not None and any(
            isinstance(t, ast.Name) and t.id == name for t in targets
        ):
            found.append(node.value)
    return found


def _built_up(scope: ast.AST, name: str) -> bool:
    """Whether ``name`` is added to with ``+=`` in ``scope`` (a module: at its top level)."""
    return any(
        isinstance(node, ast.AugAssign)
        and isinstance(node.target, ast.Name)
        and node.target.id == name
        for node in (scope.body if isinstance(scope, ast.Module) else ast.walk(scope))
    )


def _callee(call: ast.Call) -> str | None:
    if isinstance(call.func, ast.Attribute):
        return call.func.attr
    return call.func.id if isinstance(call.func, ast.Name) else None


def _product(parts: list[set[str]], node: ast.AST, glue: str = "") -> set[str]:
    """Each text ``parts`` make, one of each in order, joined by ``glue``."""
    out = {""}
    for index, part in enumerate(parts):
        out = {f"{before}{glue if index else ''}{text}" for before in out for text in part}
        if len(out) > MOST_WAYS:
            raise _Unreadable(node, f"a text that can be more than {MOST_WAYS} ways")
    return out


class _Reader:
    """Every text a ``ui_fix`` in one module can say, read from its source (:meth:`texts`):
    literals, f-strings, ``+``, ``.format``, ``%``, a separator's ``join``, a string's own
    methods, the module's constants, the function's variables and comprehensions, and what the
    module's own helpers return. Anything else cannot be read and fails; a placeholder that
    cannot be traced is read as a program's name (:data:`PLACEHOLDER`)."""

    def __init__(self, tree: ast.Module) -> None:
        self.tree = tree
        self.functions = {
            node.name: node
            for node in ast.walk(tree)
            if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef)
        }
        self.spelt: list[tuple[int, str]] = []
        self.reading: set[str] = set()
        self.naming: set[tuple[int, str]] = set()

    def texts(self, expr: ast.expr, scope: Scope, bound: dict[str, set[str]]) -> set[str]:
        if isinstance(expr, ast.Constant):
            if isinstance(expr.value, str):
                return {expr.value}
            if expr.value is None:
                return set()
            raise _Unreadable(expr, f"the constant {expr.value!r}")
        if isinstance(expr, ast.JoinedStr):
            parts: list[set[str]] = []
            for part in expr.values:
                if isinstance(part, ast.FormattedValue):
                    parts.append(self.hole(part.value, scope, bound))
                else:
                    parts.append(self.texts(part, scope, bound))
            return _product(parts, expr)
        if isinstance(expr, ast.BinOp) and isinstance(expr.op, ast.Add):
            return _product(
                [self.texts(side, scope, bound) for side in (expr.left, expr.right)], expr
            )
        if isinstance(expr, ast.BinOp) and isinstance(expr.op, ast.Mod):
            return self._percent(expr, scope, bound)
        if isinstance(expr, ast.IfExp):
            return self.texts(expr.body, scope, bound) | self.texts(expr.orelse, scope, bound)
        if isinstance(expr, ast.BoolOp):
            return set().union(*(self.texts(value, scope, bound) for value in expr.values))
        if isinstance(expr, ast.Name):
            key = (id(scope), expr.id)
            if key in self.naming:
                raise _Unreadable(expr, f"{expr.id}, which is made from itself")
            self.naming.add(key)
            try:
                values = self._values(expr, scope, bound, text=True)
                return set().union(*(self.texts(v, s, b) for v, s, b in values))
            finally:
                self.naming.discard(key)
        if isinstance(expr, ast.Subscript):
            return self._subscript(expr, scope, bound)
        if isinstance(expr, ast.Call):
            return self._call(expr, scope, bound)
        raise _Unreadable(expr, ast.unparse(expr)[:60])

    def hole(self, expr: ast.expr, scope: Scope, bound: dict[str, set[str]]) -> set[str]:
        """What a placeholder says: its texts, or a program's name where they cannot be read."""
        try:
            return self.texts(expr, scope, bound) or {PLACEHOLDER}
        except _Unreadable:
            return {PLACEHOLDER}

    def _values(
        self, name: ast.Name, scope: Scope, bound: dict[str, set[str]], *, text: bool
    ) -> list[tuple[ast.expr, Scope, dict[str, set[str]]]]:
        """What ``name`` is set to, each with the scope to read it in: a comprehension's
        binding, the function's own variables, else the module's constants. A ``text`` built
        up with ``+=`` cannot be read; a list's ``+=`` adds its items."""
        if name.id in bound:
            return [(ast.Constant(text), scope, {}) for text in sorted(bound[name.id])]
        for where, around in ((scope, scope), (self.tree, None)):
            if where is None or not (values := _assigned(where, name.id, added=not text)):
                if around is not None and name.id in {
                    arg.arg
                    for arg in (
                        *around.args.posonlyargs,
                        *around.args.args,
                        *around.args.kwonlyargs,
                    )
                }:
                    raise _Unreadable(name, f"{name.id}, a parameter of {around.name}")
                continue
            if text and _built_up(where, name.id):
                raise _Unreadable(name, f"{name.id}, built up with +=")
            return [(value, around, bound if around is not None else {}) for value in values]
        raise _Unreadable(name, f"{name.id}, which this module does not set")

    def _subscript(self, expr: ast.Subscript, scope: Scope, bound: dict[str, set[str]]) -> set[str]:
        """A text's character or slice, at whole numbers written in place."""

        def number(end: ast.expr | None) -> int | None:
            if end is None:
                return None
            if isinstance(end, ast.Constant) and isinstance(end.value, int):
                return end.value
            raise _Unreadable(expr, ast.unparse(expr)[:60])

        texts = self.texts(expr.value, scope, bound)
        index = expr.slice
        if isinstance(index, ast.Slice):
            cut = slice(number(index.lower), number(index.upper), number(index.step))
            return {text[cut] for text in texts}
        at = number(index)
        if at is None:
            raise _Unreadable(expr, ast.unparse(expr)[:60])
        return {text[at] for text in texts if -len(text) <= at < len(text)}

    def _call(self, expr: ast.Call, scope: Scope, bound: dict[str, set[str]]) -> set[str]:
        func, callee = expr.func, _callee(expr)
        if callee in SPELLERS:
            self.spelt.append((expr.lineno, f"calls {callee}()"))
            return {PLACEHOLDER}
        if isinstance(func, ast.Attribute) and callee == "format":
            if any(kw.arg is None for kw in expr.keywords) or any(
                isinstance(arg, ast.Starred) for arg in expr.args
            ):
                raise _Unreadable(expr, "a .format given * or ** arguments")
            holes = [self.hole(arg, scope, bound) for arg in expr.args]
            named = {kw.arg: self.hole(kw.value, scope, bound) for kw in expr.keywords if kw.arg}
            return set().union(
                *(
                    self._format(text, holes, named, expr)
                    for text in self.texts(func.value, scope, bound)
                )
            )
        if isinstance(func, ast.Attribute) and callee == "join" and len(expr.args) == 1:
            items = self._items(expr.args[0], scope, bound)
            return set().union(
                *(_product(items, expr, glue) for glue in self.texts(func.value, scope, bound))
            )
        if isinstance(func, ast.Attribute) and callee in TEXT_METHODS and not expr.args:
            act = TEXT_METHODS[callee]
            return {act(text) for text in self.texts(func.value, scope, bound)}
        if isinstance(func, ast.Name) and callee in self.functions:
            if callee in self.reading:
                raise _Unreadable(expr, f"{callee}, which calls itself")
            helper = self.functions[callee]
            self.reading.add(callee)
            try:
                returns = [
                    node.value
                    for node in ast.walk(helper)
                    if isinstance(node, ast.Return) and node.value is not None
                ]
                return set().union(*(self.texts(value, helper, {}) for value in returns))
            finally:
                self.reading.discard(callee)
        raise _Unreadable(expr, f"a call to {ast.unparse(func)[:50]}")

    def _items(self, expr: ast.expr, scope: Scope, bound: dict[str, set[str]]) -> list[set[str]]:
        """The texts of each item ``expr`` holds, for a ``join``: a literal list, tuple or set
        (a starred part spliced in, both of a conditional's), a comprehension (its names bound
        to what it walks), ``sorted``/``list``/``tuple`` of one, and a variable's every
        assignment, ``append``, ``extend`` and ``insert``."""
        if isinstance(expr, ast.List | ast.Tuple | ast.Set):
            items: list[set[str]] = []
            for element in expr.elts:
                if isinstance(element, ast.Starred):
                    items += self._items(element.value, scope, bound)
                else:
                    items.append(self.texts(element, scope, bound))
            return items
        if isinstance(expr, ast.IfExp):
            return self._items(expr.body, scope, bound) + self._items(expr.orelse, scope, bound)
        if isinstance(expr, ast.ListComp | ast.GeneratorExp | ast.SetComp):
            binding = dict(bound)
            for walk in expr.generators:
                if not isinstance(walk.target, ast.Name):
                    raise _Unreadable(walk.target, "a comprehension that unpacks")
                binding[walk.target.id] = set().union(*self._items(walk.iter, scope, binding))
            return [self.texts(expr.elt, scope, binding)]
        if (
            isinstance(expr, ast.Call)
            and isinstance(expr.func, ast.Name)
            and expr.func.id in ("sorted", "list", "tuple")
            and len(expr.args) == 1
        ):
            return self._items(expr.args[0], scope, bound)
        if isinstance(expr, ast.Name):
            items = []
            for value, where, binding in self._values(expr, scope, bound, text=False):
                items += self._items(value, where, binding)
            for node in ast.walk(scope if scope is not None else self.tree):
                if (
                    isinstance(node, ast.Call)
                    and isinstance(node.func, ast.Attribute)
                    and isinstance(node.func.value, ast.Name)
                    and node.func.value.id == expr.id
                    and node.func.attr in ("append", "extend", "insert")
                    and node.args
                ):
                    added = node.args[-1]
                    if node.func.attr == "extend":
                        items += self._items(added, scope, bound)
                    else:
                        items.append(self.texts(added, scope, bound))
            return items
        raise _Unreadable(expr, f"the items of {ast.unparse(expr)[:50]}")

    def _percent(self, expr: ast.BinOp, scope: Scope, bound: dict[str, set[str]]) -> set[str]:
        """``template % args``: each conversion is the argument it takes, as a placeholder."""
        args = expr.right
        named: dict[str, set[str]] = {}
        holes: list[set[str]] = []
        if isinstance(args, ast.Dict):
            for key, value in zip(args.keys, args.values, strict=True):
                if not (isinstance(key, ast.Constant) and isinstance(key.value, str)):
                    raise _Unreadable(args, "a % mapping whose keys are not plain words")
                named[key.value] = self.hole(value, scope, bound)
        else:
            elements = args.elts if isinstance(args, ast.Tuple) else [args]
            holes = [self.hole(element, scope, bound) for element in elements]
        out: set[str] = set()
        for template in self.texts(expr.left, scope, bound):
            parts: list[set[str]] = []
            at, taken = 0, 0
            for spec in _PERCENT.finditer(template):
                parts.append({template[at : spec.start()]})
                if spec.group(2) == "%":
                    parts.append({"%"})
                elif spec.group(1) is not None:
                    parts.append(named.get(spec.group(1), {PLACEHOLDER}))
                else:
                    parts.append(holes[taken] if taken < len(holes) else {PLACEHOLDER})
                    taken += 1
                at = spec.end()
            parts.append({template[at:]})
            out |= _product(parts, expr)
        return out

    def _format(
        self, template: str, holes: list[set[str]], named: dict[str, set[str]], node: ast.AST
    ) -> set[str]:
        """``template.format(…)``: each field is the argument it names, as a placeholder."""
        parts: list[set[str]] = []
        taken = 0
        try:
            fields = list(string.Formatter().parse(template))
        except ValueError as exc:
            raise _Unreadable(node, f"the .format template {template[:40]!r} ({exc})") from exc
        for literal, field, _spec, _conversion in fields:
            parts.append({literal})
            if field is None:
                continue
            key = re.split(r"[.\[]", field, maxsplit=1)[0]
            whole = key == field  # not one of its attributes or items
            if key == "":
                key, taken = str(taken), taken + 1
            if key.isdigit():
                hole = holes[int(key)] if int(key) < len(holes) else {PLACEHOLDER}
            else:
                hole = named.get(key, {PLACEHOLDER})
            parts.append(hole if whole else {PLACEHOLDER})
        return _product(parts, node)


def _ui_fix_findings(tree: ast.Module) -> list[tuple[int, str]]:
    """(line, why) for each ``ui_fix`` in ``tree`` that names a command or cannot be read
    (:class:`_Reader`). A function's keyword-only ``ui_fix`` handed on as a ``ui_fix`` (the
    doctor's ``_warn`` and ``_fail``) is read where each caller sets it, by name."""
    reader = _Reader(tree)
    found: set[tuple[int, str]] = set()
    for value, scope in _ui_fix_sites(tree):
        if (
            isinstance(value, ast.Name)
            and value.id == "ui_fix"
            and scope is not None
            and "ui_fix" in {arg.arg for arg in scope.args.kwonlyargs}
            and not _assigned(scope, "ui_fix")
        ):
            continue
        try:
            texts = reader.texts(value, scope, {})
        except _Unreadable as exc:
            found.add((exc.line, str(exc)))
            continue
        for text in texts:
            if match := GUARD.search(text):
                found.add((value.lineno, f"{match.group(0)!r} in {text[:70]!r}"))
    return sorted(found | set(reader.spelt))


def test_no_ui_fix_the_source_can_build_names_a_command() -> None:
    """Tester #4105: the doctor rows a sandbox makes set no ``ui_fix``, so none was read. What
    asq says under a row (``ui_way``) is its ``ui_fix`` as it is: each is read from the source,
    in every module that sets one, and one it cannot read fails (reviewer #4214)."""
    found = []
    for path in sorted(SRC.rglob("*.py")):
        text = path.read_text(encoding="utf-8")
        if "ui_fix" in text:
            rel = path.relative_to(SRC).as_posix()
            found += [f"{rel}:{line}: {why}" for line, why in _ui_fix_findings(ast.parse(text))]
    assert found == []


_ROW = 'row = _warn("x", "d", "f", ui_fix={})\n'

UI_FIX_SHAPES: dict[str, tuple[str, str]] = {
    "a literal": (_ROW.format('"Run rlx tidy"'), "'rlx tidy'"),
    "an f-string": (_ROW.format('f"Run {what}, or rlx tidy"'), "'rlx tidy'"),
    "a constant": ('WAY = "rlx tidy"\n' + _ROW.format("WAY"), "'rlx tidy'"),
    "a variable": (
        "def check():\n    way = 'Run rlx tidy'\n    return "
        + _ROW.format("way").split("= ", 1)[1],
        "'rlx tidy'",
    ),
    "a helper": (
        "def _way(n):\n    if n:\n        return 'Run rlx tidy'\n    return 'Wait'\n"
        + _ROW.format('"; ".join([_way(1)])'),
        "'rlx tidy'",
    ),
    "an update": ('row = old.model_copy(update={"ui_fix": "rlx tidy"})\n', "'rlx tidy'"),
    "an assignment": ('row.ui_fix = "rlx tidy"\n', "'rlx tidy'"),
    "a speller": (_ROW.format('f"{prog.word()} tidy"'), "calls word()"),
    # Reviewer #4214's ten ways past the first sweep, each read or failed now:
    "a constant's placeholder": (
        'PRODUCT = "rlx"\n' + _ROW.format('f"{PRODUCT} tidy frees it"'),
        "'rlx tidy'",
    ),
    "an imported placeholder": (
        "from aisquare.core.program import PRODUCT\n" + _ROW.format('f"{PRODUCT} tidy"'),
        "'rlx tidy'",
    ),
    "an attribute's placeholder": (_ROW.format('f"{prog.NAME} tidy"'), "'rlx tidy'"),
    "a '+'": (_ROW.format('"Run rlx " + "tidy"'), "'rlx tidy'"),
    "a .format": (_ROW.format('"Run {} {how}".format(PRODUCT, how="tidy")'), "'rlx tidy'"),
    "a %": (_ROW.format('"Run %s tidy" % (PRODUCT,)'), "'rlx tidy'"),
    "an imported constant": (
        "from aisquare.services.ways import TIDY\n" + _ROW.format("TIDY"),
        "cannot read TIDY",
    ),
    "an attribute": (_ROW.format("ways.TIDY"), "cannot read ways.TIDY"),
    "another module's helper": (_ROW.format("ways.tidy_way()"), "cannot read a call to"),
    "a helper's parameter": (
        "def _row(way):\n    return " + _ROW.format("way").split("= ", 1)[1],
        "cannot read way, a parameter of _row",
    ),
    "a setattr": ('setattr(row, "ui_fix", "Run rlx tidy")\n', "'rlx tidy'"),
    "a list built up": (
        "def check():\n    parts = []\n    parts.append('rlx tidy')\n    return "
        + _ROW.format('"; ".join(parts)').split("= ", 1)[1],
        "'rlx tidy'",
    ),
    "a text built up": (
        "def check():\n    way = 'rlx '\n    way += 'tidy'\n    return "
        + _ROW.format("way").split("= ", 1)[1],
        "cannot read way, built up with +=",
    ),
    "another parameter handed on": (
        "def _row(name, *, ui_fix=None, way=None):\n"
        "    return DoctorCheck(name=name, ui_fix=way)\n",
        "cannot read way, a parameter of _row",
    ),
    "a positional pass-through": (
        "def _warn(name, ui_fix=None):\n    return DoctorCheck(name=name, ui_fix=ui_fix)\n",
        "cannot read ui_fix, a parameter of _warn",
    ),
}
"""shape → (a module that sets a ``ui_fix`` so, what the sweep says of it)."""


@pytest.mark.parametrize("shape", sorted(UI_FIX_SHAPES))
def test_the_ui_fix_sweep_fails_each_way_a_command_could_hide(shape: str) -> None:
    source, said = UI_FIX_SHAPES[shape]
    found = _ui_fix_findings(ast.parse(source))
    assert any(said in why for _, why in found), found


UI_FIX_PLAIN: dict[str, str] = {
    "words": _ROW.format('"Restart it on its row"'),
    "an f-string of a page": (
        'PAGE = "https://example.com/tmux"\n'
        + _ROW.format('f"Install tmux as {PAGE} says, as {label} does"')
    ),
    "an attribute's placeholder": _ROW.format('f"Install it as {adapter.install_page} says"'),
    "a '+', a .format and a %": _ROW.format(
        '"Restart " + "it" + " {} {how}".format(1, how="now") + " %s%%" % (name,)'
    ),
    "a join of a comprehension and a helper": (
        "def _way(every):\n"
        "    if every == 60:\n"
        "        within = 'within the hour'\n"
        "    else:\n"
        "        within = f'within {every:g} minutes'\n"
        "    return f'rlx frees it by itself {within}'\n"
        "def check(named, to_free):\n"
        "    unread = [f'make {named} readable'] if named else []\n"
        "    return "
        + _ROW.format(
            '"; ".join([*(f"{w[0].upper()}{w[1:]}" for w in unread), '
            "*([_way(60)] if to_free else [])])"
        ).split("= ", 1)[1]
    ),
    "a keyword-only pass-through": (
        "def _warn(name, *, ui_fix=None):\n    return DoctorCheck(name=name, ui_fix=ui_fix)\n"
    ),
    "none": _ROW.format("None"),
}
"""shape → a module whose ``ui_fix`` names no command, built each way the sweep can read."""


@pytest.mark.parametrize("shape", sorted(UI_FIX_PLAIN))
def test_the_ui_fix_sweep_passes_plain_words_however_built(shape: str) -> None:
    assert _ui_fix_findings(ast.parse(UI_FIX_PLAIN[shape])) == []


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
