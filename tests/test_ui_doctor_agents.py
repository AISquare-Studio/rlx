"""The Doctor view's agent rows in asq: Connect for Claude Code, no button for the rest.

The report is the real doctor's, on a machine with Claude Code installed and not
connected and Codex and Cursor on disk. The runner does what ``aisquare --json
agents connect`` would, in process, so the click's effect on ``settings.json`` and
on the re-run report is the real one. Negative controls: Codex and Cursor have
rows and no button, before and after; and the Connect button is gone once the
shared check says the directory is connected.
"""

from __future__ import annotations

import asyncio
import shutil
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pytest
from textual.app import App, ComposeResult
from textual.message import Message
from textual.widgets import Button, Collapsible, Static

from aisquare.cli.ui.views.doctor import DoctorRefreshed, DoctorView, FixApplied
from aisquare.core.selfcli import CliResult
from aisquare.models import CheckStatus, DoctorCheck
from aisquare.services import agents as agents_service
from aisquare.services import diagnostics
from tests.ui_workers import settle_page, settle_until


@dataclass
class InProcessConnect:
    """``aisquare --json agents connect …`` without the subprocess, recording each call."""

    calls: list[list[str]] = field(default_factory=list)

    def __call__(self, args: Sequence[str], *, cwd: Path | None = None) -> CliResult:
        argv = list(args)
        self.calls.append(argv)
        words = [word for word in argv if word != "--json"]
        if words[:3] != ["agents", "connect", "claude-code"] or words[3:4] != ["--config-dir"]:
            return CliResult(argv=argv, returncode=1, stdout='{"error":"unexpected"}', stderr="")
        connection = agents_service.connect("claude-code", Path(words[4]))
        return CliResult(argv=argv, returncode=0, stdout=connection.model_dump_json(), stderr="")


class Host(App[None]):
    def __init__(self, view: DoctorView) -> None:
        super().__init__()
        self.view = view
        self.received: list[Message] = []

    def compose(self) -> ComposeResult:
        yield self.view

    def on_doctor_refreshed(self, message: DoctorRefreshed) -> None:
        self.received.append(message)

    def on_fix_applied(self, message: FixApplied) -> None:
        self.received.append(message)


def _labels(view: DoctorView) -> dict[str, str]:
    """Button id → label, for the fix buttons on screen now."""
    return {
        button.id or "": str(button.label)
        for button in view.query("#doctor-fixes Button").results(Button)
    }


def test_connect_is_one_click_and_the_planned_agents_have_no_button(
    isolated_agent_home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    claude = isolated_agent_home / ".claude"
    for name in (".claude", ".codex", ".cursor"):
        (isolated_agent_home / name).mkdir(parents=True)
    monkeypatch.chdir(tmp_path)
    run = InProcessConnect()
    home = str(Path.home())
    shown = "~" + str(claude)[len(home) :] if str(claude).startswith(home) else str(claude)
    connect_label = f"Connect Claude Code ({shown})"  # what it does, never its command (R13)

    async def drive() -> tuple[dict[str, str], dict[str, str], str, list[Message]]:
        view = DoctorView(run=run, refresh=lambda cwd: diagnostics.doctor(cwd=cwd))
        host = Host(view)
        async with host.run_test(size=(160, 60)) as pilot:
            view.show(diagnostics.doctor())
            await settle_page(host)
            before = _labels(view)
            button_id = next(key for key, label in before.items() if label == connect_label)
            # Scrolled into view first, as a person would: a report with more rows (another
            # train's doctor rows, Windows' own) put the button below the 60-row screen, and
            # the pilot cannot click what is not shown (OutOfBounds on #296's Windows leg).
            view.query_one(f"#{button_id}").scroll_visible(animate=False)
            await settle_page(host)
            await pilot.click(f"#{button_id}")
            await settle_page(host)
            # The passing rows fold behind one line (Q5): read both, as the page holds them.
            report = (
                str(view.query_one("#doctor-report", Static).render())
                + "\n"
                + str(view.query_one("#doctor-passed-list", Static).render())
            )
            return before, _labels(view), report, host.received

    before, after, report, received = asyncio.run(drive())

    assert connect_label in before.values(), before
    assert not any("codex" in label or "cursor" in label for label in before.values()), before
    assert run.calls == [
        ["--json", "agents", "connect", "claude-code", "--config-dir", str(claude)]
    ]
    assert agents_service.claude_code_connected(claude) is True, "the click connected it"
    assert connect_label not in after.values(), "a connected directory has no Connect left"
    assert not any("codex" in label or "cursor" in label for label in after.values()), after
    # By line, not by sentence: the row names the version of whatever `claude` is on PATH.
    lines = {line.split(":")[0]: line for line in report.splitlines() if ": " in line}
    assert "connected (all lifecycle hooks installed" in lines["✓ claude-code"], report
    # Codex's row is its integration's since #113 (`_check_other_agents`), not a path's.
    assert "Codex detected" in lines["✓ codex"], report  # no mark since 0.10.1 (R21)
    assert f"Cursor detected at {isolated_agent_home / '.cursor'}" in lines["✓ cursor"], report
    refreshed = [m for m in received if isinstance(m, DoctorRefreshed)]
    assert len(refreshed) == 1, received
    rows: dict[str, DoctorCheck] = {check.name: check for check in refreshed[0].checks}
    assert rows["claude-code"].status is CheckStatus.ok, rows["claude-code"]
    assert rows["cursor"].status is CheckStatus.ok and rows["cursor"].fix is None


# --- Q5: problems first, the passing checks folded --------------------------------------


@dataclass
class Spy:
    """Every ``aisquare`` call a fix makes, answered ok."""

    calls: list[list[str]] = field(default_factory=list)

    def __call__(self, args: Sequence[str], *, cwd: Path | None = None) -> CliResult:
        self.calls.append(list(args))
        return CliResult(argv=list(args), returncode=0, stdout="{}", stderr="")


def _row(name: str, status: CheckStatus, fix: str | None = None) -> DoctorCheck:
    return DoctorCheck(name=name, status=status, detail=f"{name} detail", fix=fix)


def test_the_failing_check_comes_first_and_the_passing_ones_fold_behind_one_line() -> None:
    checks = [
        _row("python", CheckStatus.ok),
        _row("git", CheckStatus.ok),
        _row("database", CheckStatus.fail),
        _row("tmux", CheckStatus.ok),
    ]

    async def drive() -> tuple[str, str, bool, bool, str]:
        view = DoctorView(run=Spy(), refresh=lambda cwd: checks)
        host = Host(view)
        async with host.run_test(size=(120, 40)):
            view.show(checks)
            await settle_page(host)
            report = str(view.query_one("#doctor-report", Static).render())
            fold = view.query_one("#doctor-passed", Collapsible)
            folded = fold.collapsed
            title = str(fold.title)
            fold.collapsed = False  # unfold it, as a click on its title does
            await settle_page(host)
            listed = view.query_one("#doctor-passed-list", Static)
            return report, title, folded, listed.region.height > 0, str(listed.render())

    report, title, folded, shown, listed = asyncio.run(drive())

    assert report.splitlines()[0].startswith("✗ database:"), report
    assert "python" not in report and "git" not in report and "tmux" not in report
    assert (title, folded) == ("3 passed", True)
    assert shown, "unfolded, the passing checks show"
    assert [line.split(":")[0] for line in listed.splitlines() if line] == [
        "✓ python",
        "✓ git",
        "✓ tmux",
    ]


def test_with_every_check_passing_the_report_says_so_and_folds_them() -> None:
    checks = [_row("python", CheckStatus.ok), _row("git", CheckStatus.ok)]

    async def drive() -> tuple[str, str]:
        view = DoctorView(run=Spy(), refresh=lambda cwd: checks)
        host = Host(view)
        async with host.run_test(size=(120, 40)):
            view.show(checks)
            await settle_page(host)
            return (
                str(view.query_one("#doctor-report", Static).render()),
                str(view.query_one("#doctor-passed", Collapsible).title),
            )

    assert asyncio.run(drive()) == ("No problems found.", "2 passed")


# --- M3: the new one-click fixes ---------------------------------------------------------


LIMITED = _row(
    "claude-account-limits",
    CheckStatus.warn,
    "Move one to an account with headroom: rlx fleet switch --project amber-otter coder-1; "
    "or wait — Claude Code continues by itself at the reset",
)
STALE_WATCHER = _row("fleet watcher", CheckStatus.warn, "rlx watch --stop; then rlx watch --detach")


@pytest.mark.parametrize(
    ("check", "label", "argv"),
    [
        (
            LIMITED,
            "Move coder-1 to an account with room",
            ["fleet", "switch", "--project", "amber-otter", "coder-1"],
        ),
        (STALE_WATCHER, "Stop the watcher", ["watch", "--stop"]),
        (STALE_WATCHER, "Start the watcher", ["watch", "--detach"]),
    ],
)
def test_each_new_fix_is_one_click_that_runs_its_command(
    check: DoctorCheck, label: str, argv: list[str]
) -> None:
    run = Spy()

    async def drive() -> dict[str, str]:
        view = DoctorView(run=run, refresh=lambda cwd: [check])
        host = Host(view)
        async with host.run_test(size=(160, 50)) as pilot:
            view.show([check])
            await settle_page(host)
            labels = _labels(view)
            button_id = next(key for key, shown in labels.items() if shown == label)
            view.query_one(f"#{button_id}").scroll_visible(animate=False)
            await settle_page(host)
            await pilot.click(f"#{button_id}")
            await settle_page(host)
            return labels

    labels = asyncio.run(drive())

    assert label in labels.values(), labels
    assert ["--json", *argv] in run.calls, run.calls


# --- #15: quiet offers, run in a pane of their own ----------------------------------------


PRIVATE_SOCKET = "asq-test-doctor-pane"
FIX_PANE = "%9"


@dataclass
class PaneWorld:
    """The fleet's server a private socket whose every argv fails; ``open_window`` replaced by
    a recorder, and the pane answered dead at once with ``status``."""

    opened: list[dict[str, Any]] = field(default_factory=list)
    killed: list[str] = field(default_factory=list)
    status: int = 0


@pytest.fixture
def pane_world(monkeypatch: pytest.MonkeyPatch) -> PaneWorld:
    from aisquare.core import tmux as tmux_core
    from aisquare.core.tmux import Completed, PaneFacts, TmuxServer, WindowInfo
    from aisquare.services import fleet as fleet_service

    world = PaneWorld()
    monkeypatch.setattr(
        tmux_core, "_tmux", lambda argv, stdin: Completed(1, "", "no server (a UI test)\n")
    )
    monkeypatch.setattr(fleet_service, "server", lambda config=None: TmuxServer(PRIVATE_SOCKET))

    def open_window(server: Any, session: str, **kwargs: Any) -> WindowInfo:
        world.opened.append({"session": session, **kwargs, "command": list(kwargs["command"])})
        return WindowInfo(
            session=session,
            window_id="@9",
            name=kwargs["name"],
            pane_id=FIX_PANE,
            dead=False,
            dead_status=None,
            current_command="gh",
            activity=False,
        )

    def facts(server: Any, pane_id: str) -> PaneFacts:
        return PaneFacts(
            pane_id=pane_id,
            width=80,
            height=24,
            cursor_x=0,
            cursor_y=0,
            cursor_visible=True,
            alternate_on=False,
            history_size=0,
            dead=True,
            dead_status=world.status,
            in_mode=False,
            current_command="gh",
            title="",
        )

    monkeypatch.setattr(fleet_service, "open_window", open_window)
    monkeypatch.setattr(TmuxServer, "pane_facts_or_raise", facts)
    monkeypatch.setattr(TmuxServer, "kill_window", lambda server, pane: world.killed.append(pane))
    return world


def _offer(name: str, offer: str) -> DoctorCheck:
    return DoctorCheck(name=name, status=CheckStatus.ok, detail=f"{name} detail", offer=offer)


def test_an_ok_rows_offer_is_a_quiet_button_beside_it_in_the_fold_and_no_problem() -> None:
    checks = [_offer("status line", "rlx accounts statusline --install")]

    async def drive() -> tuple[str, str, dict[str, str], str]:
        view = DoctorView(run=Spy(), refresh=lambda cwd: checks)
        host = Host(view)
        async with host.run_test(size=(120, 40)):
            view.show(checks)
            await settle_page(host)
            button = view.query_one("#fix-0", Button)
            return (
                str(view.query_one("#doctor-report", Static).render()),
                str(view.query_one("#doctor-passed-list", Static).render()),
                _labels(view),
                str(button.variant),
            )

    report, passed, labels, variant = asyncio.run(drive())

    assert report == "No problems found.", "an offer is never a problem"
    assert "→ Install status line below, if you want it" in passed
    assert labels == {"fix-0": "Install status line"} and variant == "default"


@pytest.mark.parametrize(
    ("check", "title", "command"),
    [
        (
            _offer("status line", "rlx accounts statusline --install"),
            "Install status line",
            None,  # ours, as this install runs it
        ),
        (_offer("gh", "gh auth login"), "Sign in to GitHub", ["gh", "auth", "login"]),
    ],
)
def test_a_pane_fix_opens_its_command_in_a_pane_and_the_checks_run_again_when_it_ends(
    pane_world: PaneWorld,
    monkeypatch: pytest.MonkeyPatch,
    check: DoctorCheck,
    title: str,
    command: list[str] | None,
) -> None:
    from aisquare.core import selfcli

    monkeypatch.setattr(shutil, "which", lambda name: f"/usr/bin/{name}")
    refreshed: list[Path | None] = []

    def refresh(cwd: Path | None) -> list[DoctorCheck]:
        refreshed.append(cwd)
        return [check]

    async def drive() -> str:
        view = DoctorView(run=Spy(), refresh=refresh)
        host = Host(view)
        async with host.run_test(size=(120, 40)) as pilot:
            view.show([check])
            await settle_page(host)
            await pilot.click("#fix-0")
            await settle_until(host, lambda: bool(refreshed))
            await settle_page(host)
            return str(view.query_one("#doctor-status", Static).render())

    status = asyncio.run(drive())

    (opened,) = pane_world.opened
    assert opened["session"] == "asq-doctor-fix" and opened["cwd"] == Path.home()
    want = command or selfcli.argv_for(["accounts", "statusline", "--install"])
    assert opened["command"] == want
    assert pane_world.killed == [FIX_PANE], "its window closed once it ended"
    assert status == f"✓ {title}" and refreshed, "and the checks ran again"


def test_a_pane_that_cannot_open_says_why_and_runs_nothing(
    pane_world: PaneWorld, monkeypatch: pytest.MonkeyPatch
) -> None:
    from aisquare.services import fleet as fleet_service

    monkeypatch.setattr(shutil, "which", lambda name: f"/usr/bin/{name}")

    def refuse(*args: Any, **kwargs: Any) -> Any:
        raise OSError("no tmux here")

    monkeypatch.setattr(fleet_service, "open_window", refuse)
    check = _offer("gh", "gh auth login")

    async def drive() -> tuple[str, str]:
        view = DoctorView(run=Spy(), refresh=lambda cwd: [check])
        host = Host(view)
        async with host.run_test(size=(120, 40)) as pilot:
            view.show([check])
            await settle_page(host)
            await pilot.click("#fix-0")
            await settle_until(host, lambda: type(host.screen).__name__ != "FixPaneScreen")
            await settle_page(host)
            said = str(view.query_one("#doctor-status", Static).render())
            return type(host.screen).__name__, said

    screen, status = asyncio.run(drive())

    assert screen != "FixPaneScreen"
    assert status.startswith("✗ Sign in to GitHub: no pane could be opened here")
    assert "no tmux here" in status
