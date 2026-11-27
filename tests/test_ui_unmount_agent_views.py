"""Agent views the TUI no longer needs are unmounted (#43, the owner's decision of 2026-11-27).

An agent's view was built once and kept for the life of the TUI: the only removal was its
project's, and with a new agent id on every launch the owner's TUI held about 414 of them
after three days. Now a view goes once its agent has stopped (its row exited, lost or
paused) or left its project's list, and the view is off screen; opening the agent again
builds it anew. The one shown stays whatever its agent does, and goes once it is left.

Kept as well: a view of a project the frame did not answer for, a view a worker still runs
on (or whose worker's answer it has not handled yet), and a view holding the keyboard under
a dialog. The keyboard never stays on a removed widget: it goes where showing the side
shown puts it.

Driven through the whole app, the fleet's rows scripted (``tests/test_ui_shell``), each
frame read as the two-second tick reads it.
"""

from __future__ import annotations

import gc
import threading
import weakref
from pathlib import Path

import pytest
from textual.message import Message
from textual.pilot import Pilot
from textual.widget import Widget
from textual.widgets import Button

from aisquare.cli.ui.app import FleetApp, HelpScreen, Panes
from aisquare.cli.ui.sidebar import AgentSelected, ProjectSelected
from aisquare.cli.ui.stop import StopAgentScreen
from aisquare.cli.ui.views.agent import (
    KEPT,
    AgentView,
    InfoLine,
    pane_words,
    shown_pane,
    start_pause,
)
from aisquare.cli.ui.views.project import ProjectView
from aisquare.core.store import store_session
from aisquare.models import FleetAgentStatus, ProjectInfo
from aisquare.services import claude_accounts as accounts_service
from aisquare.services import fleet as fleet_service
from aisquare.services import project_groups as groups_service
from tests import toast_log
from tests.test_ui_shell import (
    Script,
    drive,
    ended,
    fleet_app,
    open_stop_dialog,
    refreshed,
    seed,
    settle,
    shown,
    status,
    stop_buttons_laid_out,
    stopper,
)
from tests.test_ui_shell import no_real_tmux as _no_real_tmux
from tests.test_ui_shell import script as _script
from tests.ui_workers import settle_until, watch_until

#: Re-exported so pytest collects them here (the guard is autouse there, and here).
no_real_tmux = _no_real_tmux
script = _script


# --- helpers ---------------------------------------------------------------------------------


def two_projects(tmp_path: Path, script: Script, *rows: FleetAgentStatus) -> None:
    """``prj_a`` holding ``rows``, and ``prj_b`` with a manager: somewhere to go."""
    seed(tmp_path, ("prj_a", "alpha", None), ("prj_b", "beta", None))
    script["prj_a"] = list(rows)
    script["prj_b"] = [status("prj_b", "manager", "manager", "waiting")]


def coder(n: int, state: str = "working") -> FleetAgentStatus:
    """Coder ``n`` of ``prj_a``: live, or exited as a stop leaves its row (``ended_at`` set)."""
    if state == "exited":
        return status("prj_a", f"coder-{n:02}", "coder", "exited", minute=n, exit_status=0)
    return status("prj_a", f"coder-{n:02}", "coder", state, minute=n)


def mounted(app: FleetApp) -> list[str]:
    """The agent views mounted, by id: what the RAM holds."""
    return sorted(view.id or "" for view in app.content.query_children(AgentView))


def view_of(row: FleetAgentStatus) -> str:
    return f"agent-{row.agent.id}"


async def open_agent(app: FleetApp, row: FleetAgentStatus) -> AgentView:
    """The agent's view, opened as its row opens it, its pane given the keyboard."""
    app.post_message(AgentSelected(row.agent.project_id, row.agent.id))
    await settle_until(app, lambda: app.content.current == view_of(row))
    view = app.current_view()
    assert isinstance(view, AgentView) and view.status.agent.id == row.agent.id
    return view


async def leave_for(app: FleetApp, project_id: str) -> None:
    """Another view shown, as the shell goes back to a project: the keyboard is not moved."""
    app.post_message(ProjectSelected(project_id))
    await settle_until(app, lambda: app.content.current == f"project-{project_id}")


async def tick(app: FleetApp) -> None:
    """A frame read and painted, and what it asked for after it done."""
    await refreshed(app)
    await settle(app)


async def stop_seen(app: FleetApp) -> None:
    """The frames a view off screen takes to go once its agent stops: the first brings the
    stopped row and feeds the view (``refresh_status`` starts its labels' and task's reads),
    the next finds it idle and takes it."""
    await tick(app)
    await tick(app)


def looks(view: AgentView) -> tuple[str, str | None, str, list[str]]:
    """What a view shows of its agent: its header, the pane it shows (and the words in its
    place), and the buttons it offers."""
    return (
        shown(view.query_one("#agent-header", InfoLine)),
        view.pane.pane_id,
        view.pane.placeholder,
        [str(b.id) for b in view.query(Button) if b.display],
    )


def hold(view: AgentView) -> threading.Event:
    """A worker of ``view``'s that runs until the event returned is set: the view cannot go
    meanwhile, so a test reads what the frames before left it, whichever order the view's own
    reads and its removal took. Never ``settle`` while it runs: that waits for it."""
    release = threading.Event()
    view.run_worker(lambda: release.wait(10), thread=True, exit_on_error=False)
    return release


def held_by(app: FleetApp) -> Widget | None:
    """The main screen's keyboard: the widget it is on, under a dialog too."""
    return app.content.screen.focused


# --- the twenty ------------------------------------------------------------------------------


def test_twenty_agents_opened_then_stopped_leave_only_the_live_views_and_the_one_shown(
    tmp_path: Path, script: Script
) -> None:
    """Acceptance 1: open twenty agents, stop fifteen of them, and the views mounted drop back
    to the five live ones and the one on screen; once that one is left, to the five. Ten of
    the stopped are exited (their 💤 rows kept); four have left the list (Stop on a 💤 row
    takes it off); the one shown is exited too."""
    coders = [coder(n) for n in range(20)]
    two_projects(tmp_path, script, *coders)

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        for row in coders:
            await open_agent(app, row)
        seen: dict[str, object] = {"opened": mounted(app)}
        script["prj_a"] = [*coders[:5], *(coder(n, "exited") for n in (*range(5, 15), 19))]
        await stop_seen(app)
        seen["stopped"] = mounted(app)
        seen["shown"] = app.content.current
        await leave_for(app, "prj_b")
        await tick(app)
        seen["left"] = mounted(app)
        focused = held_by(app)
        seen["keyboard on a removed widget"] = focused is not None and not focused.is_attached
        return seen

    seen = drive(go)
    live = sorted(view_of(row) for row in coders[:5])
    assert seen["opened"] == sorted(view_of(row) for row in coders)
    assert seen["stopped"] == sorted([*live, view_of(coders[19])]), "the live, and the one shown"
    assert seen["shown"] == view_of(coders[19]), "the view shown stays while its agent stops"
    assert seen["left"] == live, "once left, it goes"
    assert seen["keyboard on a removed widget"] is False


def test_a_view_stays_while_its_agent_lives_on_screen_or_off(
    tmp_path: Path, script: Script
) -> None:
    """A live agent's view is kept off screen as it always was: its pane is the agent's, and
    building it again on every visit would restart its render loop. Every live state."""
    rows = [coder(n, state) for n, state in enumerate(sorted(fleet_service.PROCESS_STATES))]
    two_projects(tmp_path, script, *rows)

    async def go(pilot: Pilot[None]) -> list[str]:
        app = fleet_app(pilot)
        for row in rows:
            await open_agent(app, row)
        await leave_for(app, "prj_b")
        await tick(app)
        await tick(app)
        return mounted(app)

    assert drive(go) == sorted(view_of(row) for row in rows)


@pytest.mark.parametrize("state", ["exited", "lost", "paused"])
def test_every_state_with_no_process_lets_its_view_go(
    tmp_path: Path, script: Script, state: str
) -> None:
    """Not ``ALIVE_STATES``: a dead window (💤), a lost pane (✗), a pause (⏸). Each is
    opened again from its row, which builds its view anew."""
    live = coder(0)
    two_projects(tmp_path, script, live)

    async def go(pilot: Pilot[None]) -> list[str]:
        app = fleet_app(pilot)
        await open_agent(app, live)
        await leave_for(app, "prj_b")
        script["prj_a"] = [coder(0, state)]
        await stop_seen(app)
        return mounted(app)

    assert drive(go) == []


# --- opened again ----------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("state", "offered"), [("exited", "agent-restart"), ("paused", "agent-resume")]
)
def test_opening_a_stopped_agent_again_builds_its_view_as_it_was(
    tmp_path: Path, script: Script, state: str, offered: str
) -> None:
    """Acceptance 1: re-opening rebuilds. The view built again is a new one, and shows what
    the one removed showed: the same header, the same pane and the same buttons. An exited
    agent's pane is its dead window, its last screen, with Restart; a paused agent has none
    (its window was killed, and its id is the next agent's to be given: ``shown_pane``), the
    words in its place, with Resume.

    Manager's note on the plan (#43): a paused agent's view goes off screen too, so the view
    built again must still find its pane once there is one: its row live again under the same
    id, the view attaches the pane the row names."""
    two_projects(tmp_path, script, coder(0))
    stopped = coder(0, state)
    back = coder(0)
    back = back.model_copy(update={"agent": back.agent.model_copy(update={"pane_id": "%7"})})

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        view = await open_agent(app, coder(0))
        script["prj_a"] = [stopped]
        await tick(app)
        seen: dict[str, object] = {"before": looks(view)}
        gone = weakref.ref(view)
        del view
        await leave_for(app, "prj_b")
        await tick(app)
        seen["mounted"] = mounted(app)
        again = await open_agent(app, stopped)
        await settle(app)
        seen["new"] = again is not gone()
        seen["after"] = looks(again)
        seen["status"] = again.status
        await tick(app)
        seen["kept while shown"] = mounted(app)
        script["prj_a"] = [back]
        await tick(app)
        seen["live again"] = (again.pane.pane_id, again.is_attached)
        return seen

    seen = drive(go)
    assert seen["mounted"] == []
    assert seen["new"] is True
    assert seen["after"] == seen["before"], "the view built again shows what the removed one did"
    assert seen["status"] == stopped
    after = seen["after"]
    assert isinstance(after, tuple)
    _header, pane, words, buttons = after
    assert pane == shown_pane(stopped) and words == pane_words(stopped)
    assert (pane is None) == (state == "paused")
    assert offered in buttons
    assert seen["kept while shown"] == [view_of(stopped)]
    assert seen["live again"] == ("%7", True), "the view built again finds its agent's pane"


def test_a_removed_view_and_its_pane_are_freed(tmp_path: Path, script: Script) -> None:
    """The point of #43 is the memory: nothing keeps the view or its pane (and the rows its
    render cache holds) alive once removed — no screen selection, no control client, no
    set of mounted panes.

    Looked for over a bounded wait, a collection each round: the pane's timers are stopped
    as it unmounts, and their cancelled tasks still hold it until the loop has run them
    out (measured: one 50 ms pause). A reference that lasts is a leak, and fails here."""
    two_projects(tmp_path, script, coder(0))

    async def go(pilot: Pilot[None]) -> tuple[bool, bool, bool, bool]:
        app = fleet_app(pilot)
        view = await open_agent(app, coder(0))
        refs = weakref.ref(view), weakref.ref(view.pane)
        mounted_before = all(ref() is not None for ref in refs)
        del view
        await leave_for(app, "prj_b")
        script["prj_a"] = [coder(0, "exited")]
        await stop_seen(app)

        def freed() -> bool:
            gc.collect()
            return all(ref() is None for ref in refs)

        await watch_until(freed, timeout=5.0)
        return mounted_before, refs[0]() is None, refs[1]() is None, not mounted(app)

    before, view_freed, pane_freed, unmounted = drive(go)
    assert before and unmounted
    assert view_freed, "the removed AgentView is still referenced"
    assert pane_freed, "the removed TerminalPane is still referenced"


# --- the keyboard ----------------------------------------------------------------------------


def test_a_stop_from_the_agents_own_view_takes_the_view_and_the_keyboard_to_the_sidebar(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The path ``stop_finished`` takes: Stop in the agent's view, the dialog's Stop, and the
    shell shows the project. The keyboard stayed behind in the hidden view (on its Restart),
    so the view could only go with it moved: to the sidebar, as the click on the project's
    title the stop goes back by gives it. Focus is never left on the removed widget."""
    live = coder(0)
    two_projects(tmp_path, script, live)
    stopper(monkeypatch, live.agent)

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        told = toast_log.record(app)
        dialog = await open_stop_dialog(pilot, app, live.agent.id)
        script["prj_a"] = [coder(0, "exited")]
        await pilot.click(dialog.query_one("#stop-confirm", Button))
        await settle(app)  # stop_finished: the toast, the project shown, a frame asked for
        await stop_seen(app)
        focused = held_by(app)
        return {
            "mounted": mounted(app),
            "shown": app.content.current,
            "keyboard": focused,
            "attached": focused is not None and focused.is_attached,
            "sidebar": app.sidebar,
            "told": told,
        }

    seen = drive(go)
    assert seen["mounted"] == []
    assert seen["shown"] == "project-prj_a"
    assert seen["keyboard"] is seen["sidebar"] and seen["attached"] is True
    assert any(str(m).startswith("✓ stopped coder-00") for m in seen["told"])  # type: ignore[attr-defined]


def test_one_side_at_a_time_the_keyboard_goes_to_the_view_shown(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Below ``Panes.ONE_PANE_BELOW`` columns the content shows alone: the keyboard a stop left
    in its hidden view goes where ``Panes.show_content`` puts it, into the view shown, never
    onto the hidden sidebar's rows."""
    live = coder(0)
    two_projects(tmp_path, script, live)
    stopper(monkeypatch, live.agent)

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        panes = app.query_one(Panes)
        view = await open_agent(app, live)
        await pilot.click(view.query_one("#agent-stop", Button))
        await settle_until(app, lambda: stop_buttons_laid_out(app))
        dialog = app.screen
        assert isinstance(dialog, StopAgentScreen)
        release = hold(view)
        script["prj_a"] = [coder(0, "exited")]
        await pilot.click(dialog.query_one("#stop-confirm", Button))
        await watch_until(lambda: app.content.current == "project-prj_a")
        await refreshed(app)  # the frame the stop asked for: the view held, it stays
        seen: dict[str, object] = {
            "premise": (
                panes.one_pane,
                panes.side,
                app.content.current,
                view in (held_by(app) or app).ancestors_with_self,
            )
        }
        del view
        release.set()
        await settle(app)
        await stop_seen(app)
        focused = held_by(app)
        shown_view = app.current_view()
        seen["mounted"] = mounted(app)
        seen["in the view shown"] = (
            isinstance(shown_view, ProjectView)
            and focused is not None
            and shown_view in focused.ancestors_with_self
        )
        return seen

    seen = drive(go, size=(Panes.ONE_PANE_BELOW - 10, 30))
    assert seen["premise"] == (True, "content", "project-prj_a", True)
    assert seen["mounted"] == []
    assert seen["in the view shown"] is True


def test_under_a_dialog_a_view_holding_the_keyboard_waits_for_it_back(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A dialog gives the keyboard back to where it was as it closes: a hidden view holding
    it stays while the dialog is up, and goes at the first frame after, the keyboard moved
    to the sidebar first. Left in the hidden view as a stop leaves it, the dialog the help."""
    live = coder(0)
    two_projects(tmp_path, script, live)
    stopper(monkeypatch, live.agent)

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        dialog = await open_stop_dialog(pilot, app, live.agent.id)
        view = app.content.get_child_by_id(view_of(live))
        assert isinstance(view, AgentView)
        release = hold(view)
        script["prj_a"] = [coder(0, "exited")]
        await pilot.click(dialog.query_one("#stop-confirm", Button))
        await watch_until(lambda: app.content.current == "project-prj_a")
        await refreshed(app)  # the frame the stop asked for: the view held, it stays
        seen: dict[str, object] = {
            "premise": (app.content.current, view in (held_by(app) or app).ancestors_with_self)
        }
        app.action_help()
        await watch_until(lambda: isinstance(app.screen, HelpScreen))
        release.set()
        await settle(app)
        await stop_seen(app)  # its second frame would have taken it but for the dialog
        seen["under the dialog"] = mounted(app)
        app.pop_screen()
        await settle(app)
        seen["given back"] = view in (held_by(app) or app).ancestors_with_self
        del view
        await tick(app)
        seen["after"] = mounted(app)
        seen["keyboard"] = held_by(app) is app.sidebar
        return seen

    seen = drive(go)
    assert seen["premise"] == ("project-prj_a", True), "a stop leaves the keyboard in its view"
    assert seen["under the dialog"] == [view_of(coder(0))]
    assert seen["given back"] is True
    assert seen["after"] == [] and seen["keyboard"] is True


# --- workers ---------------------------------------------------------------------------------


def test_a_view_whose_pause_still_runs_waits_and_its_answer_is_still_said(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Acceptance 1, the workers: a pause runs on its view, and the row turns ⏸ before the
    pause answers. Removed then, Textual would cancel the worker: its "paused" toast lost,
    and a quit no longer waiting for its write. The view waits for it, its toast is said,
    and the next frame takes it."""
    live = coder(0)
    two_projects(tmp_path, script, live)
    hold = threading.Event()

    def pause_agent(project: ProjectInfo, label: str, **_: object) -> fleet_service.StopReceipt:
        assert hold.wait(10), "the test never let the pause answer"
        return fleet_service.StopReceipt(ended(live.agent), [], None)

    monkeypatch.setattr(fleet_service, "pause_agent", pause_agent)

    async def go(pilot: Pilot[None]) -> dict[str, object]:
        app = fleet_app(pilot)
        told = toast_log.record(app)
        view = await open_agent(app, live)
        worker = start_pause(view, live.agent)
        del view
        # Left with no settle, which would wait for the pause held on purpose.
        app.post_message(ProjectSelected("prj_b"))
        await watch_until(lambda: app.content.current == "project-prj_b")
        script["prj_a"] = [coder(0, "paused")]
        await refreshed(app)  # the fleet's read only: the pause is held on purpose
        await refreshed(app)
        seen: dict[str, object] = {"while it runs": mounted(app)}
        hold.set()
        await worker.wait()
        await stop_seen(app)
        seen["after"] = mounted(app)
        seen["told"] = list(told)
        return seen

    seen = drive(go)
    assert seen["while it runs"] == [view_of(live)]
    assert seen["after"] == []
    assert f"⏸ coder-00 paused: {KEPT}." in seen["told"]  # type: ignore[operator]


class _Unhandled(Message):
    """A message a view has been sent and has not handled yet."""


def test_a_view_with_a_message_not_yet_handled_waits(tmp_path: Path, script: Script) -> None:
    """A worker that has just finished has its answer queued on its view, and a Restart's or
    a Move's answer is the shell's ``AgentRestarted``, which the view posts as it handles it.
    While anything is queued on the view or in it, the view is not left."""
    two_projects(tmp_path, script, coder(0))

    async def go(pilot: Pilot[None]) -> tuple[list[str], list[str], list[str]]:
        app = fleet_app(pilot)
        view = await open_agent(app, coder(0))
        await leave_for(app, "prj_b")
        app.sidebar.focus()
        # A worker of the view's held across the frame that brings the stopped row, so that
        # frame cannot take the view (its own reads alone may end before its removal looks,
        # or not: a race on a loaded machine). Let go, and settled, which reads no frame.
        release = hold(view)
        script["prj_a"] = [coder(0, "exited")]
        await refreshed(app)
        await refreshed(app)
        release.set()
        await settle(app)
        idle = [str(v.id) for v in app._views_left()]
        view.pane.post_message(_Unhandled())  # … and no await: still queued in the view
        queued = [str(v.id) for v in app._views_left()]
        del view
        await tick(app)
        return idle, queued, mounted(app)

    idle, queued, after = drive(go)
    assert idle == [view_of(coder(0))], "the premise: nothing queued, the view is left"
    assert queued == [], "a message still queued in the view keeps it"
    assert after == []


def test_a_worker_finishing_after_its_view_went_raises_nothing(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Acceptance 1: a worker still running when its view goes. This shell removes such a
    view only with its project (``_leave_forgotten``, the same removal): Textual cancels the
    worker, its thread runs on, and when it ends nothing is raised and the shell carries on
    painting."""
    hold = threading.Event()
    started = threading.Event()

    def display_labels() -> dict[int, str]:
        started.set()
        assert hold.wait(10), "the test never let the labels' read end"
        return {1: "someone@example.com"}

    monkeypatch.setattr(accounts_service, "display_labels", display_labels)
    two_projects(tmp_path, script, coder(0))

    async def go(pilot: Pilot[None]) -> tuple[bool, list[str], bool]:
        app = fleet_app(pilot)
        # Opened with no settle, which would wait for the read held on purpose.
        app.post_message(AgentSelected("prj_a", coder(0).agent.id))
        await watch_until(lambda: app.content.current == view_of(coder(0)))
        await watch_until(started.is_set)
        view = app.current_view()
        assert isinstance(view, AgentView)
        workers = [w for w in app.workers if w.node is view and not w.is_finished]
        await app._leave_forgotten("prj_a", ["prj_a", "prj_b"])
        del view
        hold.set()  # the thread ends after its view went, its worker cancelled
        await watch_until(lambda: all(w.is_finished for w in workers))
        await tick(app)
        return bool(workers), mounted(app), app.is_running

    had_one, after, running = drive(go)
    assert had_one, "the premise: a worker of the view's was running"
    assert after == [] and running is True


# --- what the frame did not answer for -------------------------------------------------------


def test_a_project_the_frame_could_not_read_keeps_its_views(
    tmp_path: Path, script: Script, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Not read is not "gone": a project whose read failed (its card's notice) keeps the view
    of its stopped agent, as ``_feed_open_views`` keeps what such a view has. The first frame
    that answers for it takes it."""
    two_projects(tmp_path, script, coder(0))
    failing = {"prj_a": False}
    real = fleet_service.list_agents_many

    def list_agents_many(listings: fleet_service.Listings) -> fleet_service.ManyAgents:
        many = real(listings)
        if failing["prj_a"]:
            many.agents.pop("prj_a", None)
            many.errors["prj_a"] = fleet_service.FleetError("tmux did not answer")
        return many

    monkeypatch.setattr(fleet_service, "list_agents_many", list_agents_many)

    async def go(pilot: Pilot[None]) -> tuple[list[str], list[str]]:
        app = fleet_app(pilot)
        await open_agent(app, coder(0))
        await leave_for(app, "prj_b")
        app.sidebar.focus()
        script["prj_a"] = [coder(0, "exited")]
        failing["prj_a"] = True
        await tick(app)
        unread = mounted(app)
        failing["prj_a"] = False
        await stop_seen(app)
        return unread, mounted(app)

    unread, read = drive(go)
    assert unread == [view_of(coder(0))]
    assert read == []


def test_a_project_the_frame_no_longer_lists_keeps_its_views_live_or_not(
    tmp_path: Path, script: Script
) -> None:
    """A project another shell removed, a captured folder hidden, ``ui --project``: a frame
    that does not list the project says nothing of its agents, not that they are gone. Its
    views stay, the live agent's and the stopped one's, as ``_feed_open_views`` keeps them."""
    two_projects(tmp_path, script, coder(0), coder(1, "exited"))

    async def go(pilot: Pilot[None]) -> tuple[list[str], list[str]]:
        app = fleet_app(pilot)
        await open_agent(app, coder(0))
        await open_agent(app, coder(1, "exited"))
        await leave_for(app, "prj_b")
        app.sidebar.focus()
        opened = mounted(app)
        with store_session() as store:
            store.forget_project("prj_a")  # elsewhere: this shell is not told
        await stop_seen(app)
        return opened, mounted(app)

    opened, after = drive(go)
    assert opened == [view_of(coder(0)), view_of(coder(1, "exited"))]
    assert after == opened


def test_a_folded_projects_view_stays_until_a_frame_reads_its_project(
    tmp_path: Path, script: Script
) -> None:
    """A view left behind another no longer keeps its folded project read (rlx#557): a project
    a collapsed group folds away is not read, and its stopped agent's view stays, as a failed
    read's does. Expanded, the project is read, and the view goes."""
    two_projects(tmp_path, script, coder(0))
    with store_session() as store:
        group, _undo = groups_service.create_group(store, "tools", ["prj_a"])

    def fold(collapsed: bool) -> None:
        with store_session() as store:
            groups_service.set_collapsed(store, group.id, collapsed)

    async def go(pilot: Pilot[None]) -> tuple[list[str], list[str]]:
        app = fleet_app(pilot)
        await open_agent(app, coder(0))
        await leave_for(app, "prj_b")
        app.sidebar.focus()
        fold(True)
        script["prj_a"] = [coder(0, "exited")]
        await stop_seen(app)
        folded = mounted(app)
        fold(False)
        await stop_seen(app)
        return folded, mounted(app)

    folded, expanded = drive(go)
    assert folded == [view_of(coder(0))], "not read is not gone"
    assert expanded == []
