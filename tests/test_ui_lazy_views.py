"""The TUI's views and dialogs load when first opened, not at the start (#45).

app.py imported every view and dialog up front: about 5 MB of a TUI's start before any data.
Each one is now imported where it is first opened, and a look for a view or a worker of one
(the two-second tick's, a doctor run's) is skipped while its module was never imported
(``app._opened``).

Read in a fresh interpreter: this one has imported everything the suite has. The script
lists one scripted agent through ``tests.fleet_fakes`` and refuses every tmux command, as the
in-process UI tests do (``test_ui_shell.no_real_tmux``); the suite's conftest has already
given it a private home and TMUX_TMPDIR and cleared TMUX.
"""

from __future__ import annotations

import json
import subprocess
import sys
import textwrap
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app as cli
from aisquare.core.store import store_session

#: What a start must not load and each opening must (``app.py``'s first-use imports).
DEFERRED = (
    "aisquare.cli.ui.agent_menu",
    "aisquare.cli.ui.spawn",
    "aisquare.cli.ui.stop",
    "aisquare.cli.ui.persona_dialogs",
    "aisquare.cli.ui.views.captain",
    "aisquare.cli.ui.views.onboard",
    "aisquare.cli.ui.views.personas_tab",
    "aisquare.cli.ui.views.resources",
    "aisquare.cli.ui.views.settings_hub",
    "aisquare.cli.ui.views.tmux_page",
    "aisquare.cli.ui.views.usage_sharing",
    "aisquare.cli.ui.views.usage_stats",
    "aisquare.cli.ui.views.welcome",
)

_OPENED = textwrap.dedent(
    """
    import asyncio, json, sys, time
    from datetime import UTC, datetime
    from pathlib import Path

    DEFERRED = {deferred!r}
    SOCKET = "asq-test-lazy-views"  # a socket nobody serves, and every command is refused

    def loaded():
        return sorted(m for m in DEFERRED if m in sys.modules)

    from aisquare.core import claude_accounts as accounts
    accounts.KEYCHAIN_TOOL = "/nonexistent/security"  # no login Keychain is ever asked
    accounts.keychain_platform = lambda: False
    from aisquare.core import tmux as tmux_core
    from aisquare.core.tmux import Completed

    ran = []

    def refused(argv, stdin):
        ran.append(list(argv))
        return Completed(1, "", "no server running (a UI test addresses no real fleet)\\n")

    tmux_core._tmux = refused

    import pytest
    from textual.geometry import Region

    import aisquare.cli.app
    from aisquare.cli.ui.app import FleetApp
    from aisquare.cli.ui.sidebar import (
        AddProject, OpenAgentMenu, PersonasSelected, ResourcesSelected, SettingsSelected,
        SpawnAgent, StopAgent, UsageStatsSelected,
    )
    from aisquare.models import CheckStatus, DoctorCheck, FleetAgent, FleetAgentStatus
    from tests.fleet_fakes import list_agents_as

    PROJECT = sys.argv[1]

    def row(label, role):
        return FleetAgent(
            id=f"agt_lazy_{{label}}", project_id=PROJECT, label=label, role=role, pane_id="%1",
            tmux_socket=SOCKET, cwd=Path("/w"), created_at=datetime(2026, 8, 28, tzinfo=UTC),
        )

    # A fleet that is up (a manager and two coders), so the start asks whether the usage-sharing
    # question is due: the page and its dialog load only when it is.
    agent = row("coder-1", "coder")
    rows = [row("manager", "manager"), agent, row("coder-2", "coder")]
    listed = [FleetAgentStatus(agent=a, state="working") for a in rows]
    list_agents_as(
        pytest.MonkeyPatch(), lambda p, *, live_only=True: listed if p.id == PROJECT else []
    )
    checks = []
    report = {{}}

    # The suite forces sharing off (DO_NOT_TRACK), and the shell then asks nothing. Only the
    # shell's own look is told otherwise: the service it asks, and every other reader, still
    # find sharing forced off, so the question is never due and nothing is sent.
    import types
    from aisquare.cli.ui import app as shell
    from aisquare.core import sharing as sharing_core

    def not_forced():
        report["sharing asked"] = True
        return None

    shell.usage_share_core = types.SimpleNamespace(
        enabled=sharing_core.enabled, forced_off_reason=not_forced
    )

    async def until(pilot, holds, what):
        deadline = time.monotonic() + 20
        while not holds():
            if time.monotonic() > deadline:
                raise AssertionError(f"never: {{what}}")
            await pilot.pause(0.02)

    async def main():
        app = FleetApp(
            refresh_seconds=3600, doctor=lambda: list(checks), accounts=None, headroom=None
        )
        async with app.run_test(headless=True, size=(140, 40)) as pilot:
            await until(pilot, lambda: app.content.current == f"project-{{PROJECT}}", "home")
            app.refresh_data()  # a tick, the doctor's paint: neither may load a view
            # Remote switched off in Settings' Experimental (on in the suite): the switch's
            # look for the Settings hub, never opened, may not load it either.
            from aisquare.cli.ui.views.settings_captain import RemoteSwitched

            was_on = app.remote_on
            app.post_message(RemoteSwitched(not was_on))
            await until(pilot, lambda: app.remote_on is not was_on, "the Remote switch")
            report["remote was on"] = was_on
            doctor = app._doctor_worker
            await until(pilot, lambda: doctor is None or doctor.is_finished, "the doctor")
            await until(pilot, lambda: not any(
                w.name == shell.CONSENT_DUE_WORKER and not w.is_finished for w in app.workers
            ) and report.get("sharing asked"), "the sharing question's look")
            for _ in range(3):
                await pilot.pause()
            report["start"] = loaded()

            async def page(name, message, view_id):
                before = set(loaded())
                app.post_message(message) if message is not None else await app.action_welcome()
                await until(pilot, lambda: app.content.current == view_id, view_id)
                report[name] = sorted(set(loaded()) - before)

            async def dialog(name, message, screen):
                before = set(loaded())
                app.post_message(message)
                await until(pilot, lambda: type(app.screen).__name__ == screen, screen)
                report[name] = sorted(set(loaded()) - before)
                app.pop_screen()
                await until(pilot, lambda: len(app.screen_stack) == 1, "closed")

            await page("hub", SettingsSelected(), "settings-hub")
            hub = app.content.get_child_by_id("settings-hub")
            await page("resources", ResourcesSelected(), "resources")
            await page("usage", UsageStatsSelected(), "usage-stats")
            await page("onboard", AddProject(), "onboard")
            await page("welcome", None, "welcome")  # it imports onboard: after it
            await dialog("spawn", SpawnAgent(PROJECT), "SpawnDialog")
            await dialog("menu", OpenAgentMenu(PROJECT, agent.id, Region(2, 2, 10, 1)), "AgentMenu")
            await dialog("stop", StopAgent(PROJECT, agent.id, ask=True), "StopAgentScreen")
            await page("personas", PersonasSelected(), "personas")  # it imports spawn: after it

            # The hub opened, left, and found again: a doctor run while it is hidden reaches it.
            checks.append(DoctorCheck(name="tmux", status=CheckStatus.fail, detail="absent"))
            app.run_doctor()
            await until(pilot, lambda: app.doctor_checks, "the doctor's second run")
            await page("hub again", SettingsSelected(), "settings-hub")
            about = str(app.query_one("#hub-doctor-about").render())
            report["hub kept"] = app.content.get_child_by_id("settings-hub") is hub
            report["hub found"] = "the last run found" in about
            from tests.pane_harness import asks_a_server, socket_of  # last: it imports views

            report["tmux"] = sorted({{socket_of(a) for a in ran if asks_a_server(a)}})
        app.exit()

    asyncio.run(main())
    print("REPORT " + json.dumps(report))
    """
)


@pytest.fixture
def a_project(tmp_path: Path) -> str:
    """A home with one project, as ``rlx init`` leaves it: start opens that project."""
    folder = tmp_path / "project"
    folder.mkdir()
    made = CliRunner().invoke(cli, ["init", "--local", "--yes", "--no-onboard", str(folder)])
    assert made.exit_code == 0, made.output
    with store_session() as store:
        (project,) = store.list_projects()
    return project.id


def _opened_one_by_one(project_id: str) -> dict[str, Any]:
    code = _OPENED.format(deferred=set(DEFERRED))
    result = subprocess.run(
        [sys.executable, "-c", code, project_id],
        capture_output=True,
        text=True,
        timeout=180,
        cwd=Path(__file__).resolve().parent.parent,  # where ``tests.fleet_fakes`` imports from
    )
    assert result.returncode == 0, result.stderr[-3000:]
    line = next(x for x in result.stdout.splitlines() if x.startswith("REPORT "))
    report: dict[str, Any] = json.loads(line.removeprefix("REPORT "))
    return report


@pytest.mark.skipif(sys.platform == "win32", reason="the headless start is POSIX's here")
def test_a_start_loads_no_deferred_view_and_each_loads_when_first_opened(a_project: str) -> None:
    """Acceptance 1: a start to the first screen (the project's page) loads none of them, a
    tick, a doctor run and Remote switched off included; each page and dialog then loads its
    own module as it opens, and opens."""
    report = _opened_one_by_one(a_project)

    assert report["sharing asked"] is True, report
    assert report["remote was on"] is True, report
    assert report["start"] == [], report
    assert report["hub"] == [
        "aisquare.cli.ui.views.settings_hub",
        "aisquare.cli.ui.views.tmux_page",
    ], report
    assert report["resources"] == ["aisquare.cli.ui.views.resources"], report
    assert report["usage"] == [
        "aisquare.cli.ui.views.usage_sharing",
        "aisquare.cli.ui.views.usage_stats",
    ], report
    assert report["welcome"] == ["aisquare.cli.ui.views.welcome"], report
    assert report["onboard"] == ["aisquare.cli.ui.views.onboard"], report
    assert report["spawn"] == ["aisquare.cli.ui.persona_dialogs", "aisquare.cli.ui.spawn"], report
    assert report["menu"] == ["aisquare.cli.ui.agent_menu"], report
    assert report["stop"] == ["aisquare.cli.ui.stop"], report
    assert report["personas"] == ["aisquare.cli.ui.views.personas_tab"], report
    assert set(report["tmux"]) <= {"asq-test-lazy-views"}, report


@pytest.mark.skipif(sys.platform == "win32", reason="the headless start is POSIX's here")
def test_a_view_opened_left_and_opened_again_is_still_found(a_project: str) -> None:
    """The manager's point 3 on #45: the look for a deferred view is skipped only while its
    module was never imported. The Settings hub, opened then left for other pages, is told
    of a doctor run while hidden, and is the same hub when opened again."""
    report = _opened_one_by_one(a_project)

    assert report["hub kept"] is True, report
    assert report["hub found"] is True, report
