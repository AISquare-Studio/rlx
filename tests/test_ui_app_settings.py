"""App settings, driven headless: the machine-wide form, its writes, and Reset all.

Moved here from the project's Settings tab, which now holds only the project's own
nickname (tests/test_ui_project.py). What each test asserts is the artefact: the
bytes ``config.toml`` holds after Save, the backup Reset all keeps.
"""

from __future__ import annotations

import asyncio
import tomllib
from collections.abc import Callable, Coroutine
from pathlib import Path
from typing import Any, TypeVar, cast

import pytest
from textual.app import App, ComposeResult
from textual.content import Content
from textual.notifications import SeverityLevel
from textual.pilot import Pilot
from textual.widget import Widget
from textual.widgets import (
    Button,
    Checkbox,
    Collapsible,
    Input,
    Label,
    Select,
    Static,
    Switch,
    TabbedContent,
    TabPane,
)
from textual.widgets._tooltip import Tooltip

from aisquare.cli.ui.confirm_button import ConfirmButton
from aisquare.cli.ui.remote_control import RemoteController
from aisquare.cli.ui.views.app_settings import (
    AGENT_ADVANCED,
    AGENT_TAB,
    ASK_AGAIN_BUTTON,
    CROSS_AGENT_TIP,
    DISPLAY_CHOICES,
    DISPLAY_TAB,
    FALLBACK_AGENT_TIP,
    NOT_ASKING_LINE,
    ON_LIMIT_MODES,
    PICK_MODES,
    PREFERENCES_ADVANCED,
    PREFERENCES_TAB,
    REMOTE_TAB,
    UPDATES_CHECK,
    AppSettingsView,
    UiSettingsChanged,
    account_options,
    display_label,
)
from aisquare.core import paths
from aisquare.core.agent_adapters import adapters
from aisquare.core.config import AppConfig, UiSettings, load_config, overrides, save_config
from aisquare.services import fleet as fleet_service
from aisquare.services import settings as settings_service
from tests.settings_fields import leave_fields, reopen
from tests.ui_workers import settle_page

T = TypeVar("T")


class AppHost(App[None]):
    """A bare app around one ``AppSettingsView`` that records every notification."""

    def __init__(self) -> None:
        super().__init__()
        self.remote = RemoteController()
        """What ``FleetApp.remote`` is: the one Remote the *Remote* tab is a view over."""
        self.notices: list[tuple[str, str]] = []
        self.ui_told: list[bool] = []
        """``always_show_project_path`` of each ``UiSettingsChanged`` the form posted."""
        self.stop_told: list[bool] = []
        """``confirm_stop`` of each ``UiSettingsChanged`` the form posted."""
        self.uis: list[UiSettings] = []
        """``[ui]`` of each ``UiSettingsChanged`` the form posted, whole."""

    def compose(self) -> ComposeResult:
        yield AppSettingsView(id="app-settings")

    def on_ui_settings_changed(self, event: UiSettingsChanged) -> None:
        self.ui_told.append(event.ui.always_show_project_path)
        self.stop_told.append(event.ui.confirm_stop)
        self.uis.append(event.ui)

    def notify(
        self,
        message: str,
        *,
        title: str = "",
        severity: SeverityLevel = "information",
        timeout: float | None = None,
        markup: bool = True,
    ) -> None:
        self.notices.append((message, severity))
        super().notify(message, title=title, severity=severity, timeout=timeout, markup=markup)


def drive(scenario: Callable[[Pilot[None], AppHost], Coroutine[Any, Any, T]]) -> T:
    """Run ``scenario`` against a mounted App settings view and return what it observed."""

    async def run() -> T:
        host = AppHost()
        async with host.run_test(size=(120, 50)) as pilot:
            await pilot.pause()
            return await scenario(pilot, host)

    return asyncio.run(run())


async def settle(pilot: Pilot[None]) -> None:
    """Let the page go quiet: every worker of ours done, nothing queued."""
    await settle_page(pilot.app)


def _config_toml() -> dict[str, Any]:
    return tomllib.loads(paths.config_path().read_text(encoding="utf-8"))


def test_the_view_is_titled_app_settings_and_holds_no_project_field() -> None:
    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[str, list[str]]:
        title = str(host.query_one("#settings-title", Static).render())
        missing = [wid for wid in ("#codename", "#nickname") if host.query(wid)]
        return title, missing

    title, project_fields = drive(scenario)
    assert title == "App settings"
    assert project_fields == []


def test_settings_saves_a_changed_permission_mode_to_config_toml() -> None:
    assert not paths.config_path().exists()  # nothing written before the user saves

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[tuple[str, str]]:
        await pilot.pause()
        host.query_one("#role-perm-coder", Select).value = "plan"
        host.query_one("#worktree-dir", Input).value = ".fleet-trees"
        leave_fields(host)
        await settle(pilot)
        return host.notices

    notices = drive(scenario)
    assert any(m.startswith("Saved ") for m, _ in notices), notices
    fleet_on_disk = _config_toml()["fleet"]
    on_disk = fleet_on_disk["roles"]
    assert on_disk["coder"]["permission_mode"] == "plan"  # the bytes, not the widget
    assert on_disk["coder"]["worktree"] is True  # the role's table whole, for older builds
    assert load_config().fleet.roles["coder"].worktree is True  # untouched fields survive
    # The role's table whole, each agent's own arguments (#113) and WS-1's tool policy at
    # its defaults included (an older build ignores a key it does not know).
    assert on_disk["manager"] == {
        "permission_mode": "auto",
        "worktree": False,
        "extra_args": [],
        "agent_args": {},
        "remote_control": False,
        "deny_tools": [],
        "allow_tools": [],
        "skill_listing": "full",
        "board_context": "all",
    }
    assert fleet_on_disk["worktree_dir"] == ".fleet-trees"  # the worktree root (§4.2)
    assert "escape_key" not in fleet_on_disk  # an untouched fleet field keeps its default
    assert load_config().fleet.escape_key == "f12"
    assert load_config().fleet.roles["coder"].permission_mode == "plan"


def test_the_agent_cap_shows_auto_as_the_default_and_takes_it_back_from_a_number() -> None:
    """``auto`` is the default, shown in the field and as its placeholder; a number saves as
    before, and ``auto`` typed over it saves ``auto`` (the default: out of the file)."""
    config = load_config()
    config.fleet.max_agents_per_project = 6
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[str, str, list[str]]:
        await settle(pilot)
        field = host.query_one("#max-agents", Input)
        shown, placeholder = field.value, field.placeholder
        field.value = "auto"
        leave_fields(host)
        await settle(pilot)
        errors = [m for m, severity in host.notices if severity == "error"]
        return shown, placeholder, errors

    shown, placeholder, errors = drive(scenario)
    assert (shown, errors) == ("6", [])
    assert placeholder.startswith("auto (default)")
    assert load_config().fleet.max_agents_per_project == "auto"
    assert "max_agents_per_project" not in overrides(load_config()).get("fleet", {})


def test_settings_rejects_a_bad_agent_cap_and_writes_nothing() -> None:
    """M5: the agent cap typed as 0 and Enter is refused in a line under the field, which
    keeps what was typed; nothing is written. A good value then clears the line."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, Any]:
        await pilot.pause()
        view = host.query_one(AppSettingsView)
        host.query_one(f"#{AGENT_ADVANCED}", Collapsible).collapsed = False
        await settle(pilot)
        field = host.query_one("#max-agents", Input)
        field.focus()
        await settle(pilot)
        field.value = "0"
        await pilot.press("enter")
        await settle(pilot)
        seen: dict[str, Any] = {
            "said": view.problem(field),
            "typed": field.value,
            "written": paths.config_path().exists(),
            "toasts": [m for m, _ in host.notices],
        }
        field.value = "auto"  # what the file holds, typed back (tester of #24, F1)
        await pilot.press("enter")
        await settle(pilot)
        seen["put back"] = (view.problem(field), paths.config_path().exists())
        field.value = "0"
        await pilot.press("enter")
        await settle(pilot)
        field.value = "auto"
        field.blur()  # and left, this time
        await settle(pilot)
        seen["left"] = view.problem(field)
        field.focus()
        await settle(pilot)
        field.value = "3"
        await pilot.press("enter")
        await settle(pilot)
        seen["after"] = view.problem(field)
        return seen

    seen = drive(scenario)
    assert seen["said"].startswith("not saved: ") and "at least 1" in seen["said"], seen
    assert seen["typed"] == "0", "the field keeps what was typed, to correct"
    assert seen["written"] is False, "a refused value never reaches the writer"
    assert seen["toasts"] == [], "said beside the field, not in a toast"
    assert seen["put back"] == ("", False), "the value on disk typed back: the line goes"
    assert seen["left"] == "", "and on leaving the field"
    assert seen["after"] == "", "a good value clears the line"
    assert load_config().fleet.max_agents_per_project == 3


def test_settings_binds_an_account_per_role_and_a_cleared_one_leaves_no_empty_profile() -> None:
    """The select beside each role is `team bind <role> --account` with a mouse (#145)."""
    from aisquare.core import claude_accounts as accounts_core

    accounts_core.create_account()  # slot 2, under the isolated AISQUARE_HOME

    async def scenario(
        pilot: Pilot[None], host: AppHost
    ) -> tuple[list[str], list[tuple[str, str]]]:
        await settle(pilot)  # the slots arrive from a worker
        select = host.query_one("#role-acct-coder", Select)
        labels = [str(label) for label, _value in select._options]
        select.value = "2"
        leave_fields(host)
        await settle(pilot)
        return labels, host.notices

    labels, notices = drive(scenario)
    assert any(m.startswith("Saved ") for m, _ in notices), notices
    assert labels[0] == "(no preset account)"
    assert "2 · not signed in" in labels, labels  # by who it is (Q1), never "account 2"
    assert load_config().team.profiles["coder"].account == "2"  # the binding's one home
    assert _config_toml()["team"]["profiles"]["coder"]["account"] == "2"  # the bytes
    assert "manager" not in load_config().team.profiles  # an untouched role gains no profile

    async def clear(pilot: Pilot[None], host: AppHost) -> str | None:
        await settle(pilot)
        select = host.query_one("#role-acct-coder", Select)
        shown_value = select.value
        select.value = ""
        leave_fields(host)
        await settle(pilot)
        return str(shown_value)

    shown_value = drive(clear)
    assert shown_value == "2"  # the form opened on what the file held
    assert "coder" not in load_config().team.profiles  # nothing else bound: the table goes


def test_settings_clearing_an_account_keeps_a_binding_that_still_reads_a_file(
    tmp_path: Path,
) -> None:
    """Follow-up of #257: a binding is empty once its account goes only when nothing else
    is bound, and `env_files` (`team bind --env-file`, a reviewer's GH_TOKEN) is something.
    Read as `bin or env or args`, clearing the account dropped the token file with it."""
    from aisquare.core import claude_accounts as accounts_core
    from aisquare.services import settings as settings_service

    accounts_core.create_account()  # slot 2, under the isolated AISQUARE_HOME
    token = tmp_path / "coder-token"
    token.write_text("secret\n", encoding="utf-8")
    settings_service.bind_role("coder", env_files={"GH_TOKEN": str(token)}, account="2")

    async def clear(pilot: Pilot[None], host: AppHost) -> None:
        await settle(pilot)
        host.query_one("#role-acct-coder", Select).value = ""
        leave_fields(host)
        await settle(pilot)

    drive(clear)
    profile = load_config().team.profiles["coder"]
    assert profile.account is None and profile.env_files == {"GH_TOKEN": str(token)}


def test_settings_save_leaves_the_bindings_it_did_not_change_as_the_file_has_them() -> None:
    """Final review of #203, accounts F3: Save wrote every role's account select back, so a
    binding changed since the tab was read was reverted by a save of any other field. The
    sharp case is ``accounts remove``, run from the Accounts page of the same app: it clears
    a binding to a slot that had no login, so the next ``add`` into that slot is not
    inherited by the role, and the kept-alive tab put the number straight back."""
    from aisquare.core import claude_accounts as accounts_core
    from aisquare.services import claude_accounts as accounts_service
    from aisquare.services import settings as settings_service

    removed = accounts_core.create_account()  # slot 2, never signed in
    accounts_core.create_account()  # slot 3
    settings_service.bind_role("coder", account="2")
    settings_service.bind_role("tester", account="2")

    async def scenario(
        pilot: Pilot[None], host: AppHost
    ) -> tuple[list[str], list[tuple[str, str]]]:
        await settle(pilot)  # the form holds coder → 2 and tester → 2
        notes: list[str] = []
        accounts_service.remove(removed, notes=notes)  # the Accounts page's Remove
        settings_service.bind_role("reviewer", account="3")  # `team bind` in another shell
        host.query_one("#role-acct-tester", Select).value = "3"  # the one change made here
        host.query_one("#max-agents", Input).value = "7"
        leave_fields(host)
        await settle(pilot)
        return notes, host.notices

    notes, notices = drive(scenario)
    assert any(m.startswith("Saved ") for m, _ in notices), notices
    assert any("role coder was bound to slot 2" in note for note in notes), notes
    profiles = load_config().team.profiles
    assert "coder" not in profiles or profiles["coder"].account is None  # the clear stands
    assert profiles["reviewer"].account == "3"  # the other shell's binding stands
    assert profiles["tester"].account == "3"  # what the operator changed here lands
    assert load_config().fleet.max_agents_per_project == 7
    assert accounts_core.create_account().slot == 2  # the slot the stale "2" would have named


def test_the_settings_form_reads_its_file_once_and_the_slots_off_the_ui_thread(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Review of #205, fourth round: the constructor read ``config.toml`` three times —
    ``[fleet]``, ``[accounts]``, the bindings — and listed the accounts, a store open, a
    directory scan and maybe a reconcile write, on the event loop. One read of the file;
    the slots come from a thread worker, and the select keeps what it showed meanwhile."""
    import threading

    from aisquare.cli.ui.views import app_settings as settings_view
    from aisquare.core import claude_accounts as accounts_core
    from aisquare.services import claude_accounts as accounts_service
    from aisquare.services import settings as settings_service

    accounts_core.create_account()  # slot 2
    settings_service.bind_role("coder", account="2")
    loads: list[str] = []
    for module in (settings_view, fleet_service, accounts_service, settings_service):
        real_load = module.load_config

        def counted(real: Any = real_load, name: str = module.__name__) -> Any:
            loads.append(name)
            return real()

        monkeypatch.setattr(module, "load_config", counted)
    listed: list[str] = []
    real_list = accounts_service.list_accounts

    def list_accounts() -> Any:
        listed.append(threading.current_thread().name)
        return real_list()

    monkeypatch.setattr(accounts_service, "list_accounts", list_accounts)

    form = AppSettingsView()
    assert len(loads) == 1 and listed == []  # the constructor reads the file once, lists nothing

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[list[str], str]:
        await settle(pilot)
        select = host.query_one("#role-acct-coder", Select)
        return [str(label) for label, _value in select._options], str(select.value)

    labels, value = drive(scenario)
    assert listed and threading.main_thread().name not in listed
    assert "2 · not signed in" in labels, labels  # the slots, named by who (Q1)
    assert value == "2"  # the preset's, shown before the slots arrived and after
    assert form.accounts.pick == "headroom" and form.fleet.escape_key == "f12"


def test_a_slot_removed_between_two_reads_falls_back_to_the_binding_on_the_form() -> None:
    """The slots arrive from a worker, so a select can hold a slot an earlier answer offered
    and the next one does not: set as its value, the select refuses it and the handler
    raises. It falls back to what the file binds instead."""
    import shutil

    from aisquare.core import claude_accounts as accounts_core

    accounts_core.create_account()  # slot 2
    third = accounts_core.create_account()  # slot 3, removed below

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[str, list[str]]:
        await settle(pilot)
        select = host.query_one("#role-acct-coder", Select)
        with select.prevent(Select.Changed):  # held from an earlier read, not a pick (M5)
            select.value = "3"
        shutil.rmtree(third.config_dir)
        host.query_one(AppSettingsView)._load_accounts()
        await settle(pilot)
        return str(select.value), [str(value) for _label, value in select._options]

    value, offered = drive(scenario)
    assert value == ""  # nothing set: "no preset account"
    assert "3" not in offered and "2" in offered


def test_settings_saves_the_accounts_section_and_rejects_a_bad_line() -> None:
    """`[accounts]` (#146) on the same form, through the same one writer."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[tuple[str, str]]:
        await pilot.pause()
        host.query_one("#accounts-pick", Select).value = "default"
        host.query_one("#accounts-switch-at", Input).value = "70"
        host.query_one("#accounts-on-limit", Select).value = "wait"
        host.query_one("#accounts-wait-minutes", Input).value = "5"
        host.query_one("#accounts-prefer", Select).value = "reset-soonest"
        leave_fields(host)
        await settle(pilot)
        return host.notices

    notices = drive(scenario)
    assert any(m.startswith("Saved ") for m, _ in notices), notices
    on_disk = _config_toml()["accounts"]
    assert on_disk == {
        "pick": "default",
        "switch_at": 70,
        "on_limit": "wait",
        "wait_if_reset_within_minutes": 5,
        "prefer": "reset-soonest",
    }
    assert load_config().accounts.pick == "default"

    async def bad(pilot: Pilot[None], host: AppHost) -> tuple[str, str]:
        await pilot.pause()
        shown_pick = str(host.query_one("#accounts-pick", Select).value)
        field = host.query_one("#accounts-switch-at", Input)
        field.value = "250"
        leave_fields(host)
        await settle(pilot)
        return shown_pick, host.query_one(AppSettingsView).problem(field)

    shown_pick, said = drive(bad)
    assert shown_pick == "default"  # the form opened on what the file holds
    assert "between 1 and 100" in said, "said under the field (M5)"
    assert load_config().accounts.switch_at == 70  # a refused value never reaches the writer


def test_settings_switches_what_the_accounts_share() -> None:
    """``share_trust`` (on by default) and ``share_settings`` (off) are two switches in the
    accounts section; Save writes the one changed and nothing else."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, bool]:
        await pilot.pause()
        shown = (
            host.query_one("#accounts-share-trust", Switch).value,
            host.query_one("#accounts-share-settings", Switch).value,
        )
        host.query_one("#accounts-share-trust", Switch).value = False
        leave_fields(host)
        await settle(pilot)
        return shown

    assert drive(scenario) == (True, False)  # the defaults
    assert _config_toml()["accounts"] == {"share_trust": False}
    assert load_config().accounts.share_trust is False

    async def reopened(pilot: Pilot[None], host: AppHost) -> bool:
        await pilot.pause()
        host.query_one("#accounts-share-settings", Switch).value = True
        leave_fields(host)
        await settle(pilot)
        return host.query_one("#accounts-share-trust", Switch).value

    assert drive(reopened) is False  # the form opened on what the file holds
    assert _config_toml()["accounts"] == {"share_trust": False, "share_settings": True}


def test_settings_switches_the_hand_over_across_agents_off_and_on() -> None:
    """``cross_agent_handover`` (on by default, task #15) is a switch in the accounts section:
    off, Save writes it and nothing else; the form reopens on what the file holds."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> bool:
        await pilot.pause()
        shown = host.query_one("#accounts-cross-agent", Switch).value
        host.query_one("#accounts-cross-agent", Switch).value = False
        leave_fields(host)
        await settle(pilot)
        return shown

    assert drive(scenario) is True  # the default
    assert _config_toml()["accounts"] == {"cross_agent_handover": False}
    assert load_config().accounts.cross_agent_handover is False

    async def reopened(pilot: Pilot[None], host: AppHost) -> bool:
        await pilot.pause()
        return host.query_one("#accounts-cross-agent", Switch).value

    assert drive(reopened) is False


def test_the_hand_off_across_agents_switch_says_the_conversation_goes_to_the_other_vendor() -> None:
    """The default is on (the owner's call, r22 of #113), so the switch says what it sends and
    where: on its label and on the switch itself."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[str, str, str]:
        await pilot.pause()
        switch = host.query_one("#accounts-cross-agent", Switch)
        label = switch.parent.query_one(Label) if switch.parent is not None else switch
        return str(switch.tooltip), str(label.tooltip), str(label.render())

    switch_tip, label_tip, text = drive(scenario)

    assert text == "hand off across agents"
    assert switch_tip == label_tip == CROSS_AGENT_TIP
    for said in ("conversation", "that agent's vendor", "scrubbed", "Off:"):
        assert said in CROSS_AGENT_TIP


@pytest.mark.parametrize("tip", [CROSS_AGENT_TIP, FALLBACK_AGENT_TIP], ids=["cross", "fallback"])
def test_a_tooltip_renders_the_words_it_is_written_in(tip: str) -> None:
    """Textual reads a tooltip as markup: ``[redaction]`` rendered as nothing, and the
    sentence "scrubbed out first ( level)" reached the user (tester on #15). What
    ``Tooltip.update`` renders is the sentence, word for word."""

    class Host(App[None]):
        def compose(self) -> ComposeResult:
            yield Tooltip()

    async def render() -> str:
        host = Host()
        async with host.run_test() as pilot:
            shown = host.query_one(Tooltip)
            shown.update(tip)
            await pilot.pause()
            visual = shown.visual
            assert isinstance(visual, Content)
            return visual.plain

    assert asyncio.run(render()) == tip
    assert "[" not in tip, "no square bracket in a tooltip: Textual reads it as markup"


def test_settings_switch_the_daily_release_check_off_and_on() -> None:
    """``[updates] check`` (on by default) is a switch of its own; Save writes it alone."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> bool:
        await pilot.pause()
        shown = host.query_one("#updates-check", Switch).value
        host.query_one("#updates-check", Switch).value = False
        leave_fields(host)
        await settle(pilot)
        return shown

    assert drive(scenario) is True  # the default
    assert _config_toml()["updates"] == {"check": False}
    assert load_config().updates.check is False

    async def reopened(pilot: Pilot[None], host: AppHost) -> bool:
        await pilot.pause()
        shown = host.query_one("#updates-check", Switch).value
        host.query_one("#updates-check", Switch).value = True
        leave_fields(host)
        await settle(pilot)
        return shown

    assert drive(reopened) is False  # the form opened on what the file holds
    assert load_config().updates.check is True


def test_one_save_writes_the_no_role_agents_row_and_the_release_check_together() -> None:
    """Merged in #401 for 0.9.0: #405 made the role-less ``agent`` a default role, which gave
    *Agent settings* its row, and #301 put the release check's switch in *User
    preferences*. Both tabs are one form with one Save: the ``agent`` row is there, and a
    Save writes it and the switch together, neither in place of the other."""
    from aisquare.cli.ui.views.app_settings import role_widget_id
    from aisquare.core.harness import PLAIN_ROLE

    async def scenario(pilot: Pilot[None], host: AppHost) -> None:
        await pilot.pause()
        host.query_one(f"#{role_widget_id('perm', PLAIN_ROLE)}", Select).value = "acceptEdits"
        host.query_one("#updates-check", Switch).value = False
        leave_fields(host)
        await settle(pilot)

    drive(scenario)
    saved = _config_toml()
    assert saved["fleet"]["roles"][PLAIN_ROLE]["permission_mode"] == "acceptEdits"
    assert saved["updates"] == {"check": False}
    assert load_config().fleet.roles[PLAIN_ROLE].permission_mode == "acceptEdits"


def test_reset_all_asks_in_place_then_keeps_a_backup_and_shows_the_defaults() -> None:
    """Reset all asks in place: its first click arms it, changes nothing, and the note line
    under the buttons names the backup it keeps; Esc disarms it and gives the note back; a
    double click on it moves the file aside to that backup, and the form shows the defaults."""
    config = load_config()
    config.fleet.max_agents_per_project = 9
    config.accounts.pick = "default"
    save_config(config)
    before = paths.config_path().read_bytes()

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, object]:
        seen: dict[str, object] = {}
        reset = host.query_one("#reset-settings", ConfirmButton)
        note = host.query_one("#settings-note", Static)
        rest = (str(reset.label), str(note.content))
        await pilot.click(reset)
        await settle(pilot)
        seen["armed"] = (reset.armed, str(reset.label), str(note.content))
        seen["after_one"] = paths.config_path().read_bytes()
        await pilot.press("escape")
        await settle(pilot)
        seen["disarmed"] = (str(reset.label), str(note.content)) == rest
        seen["after_escape"] = paths.config_path().read_bytes()
        seen["pushed"] = len(host.screen_stack)
        await pilot.double_click(reset)
        await settle(pilot)
        seen["form"] = (
            host.query_one("#max-agents", Input).value,
            str(host.query_one("#accounts-pick", Select).value),
        )
        seen["notices"] = host.notices
        return seen

    seen = drive(scenario)
    armed, label, note = cast(tuple[bool, str, str], seen["armed"])
    assert armed is True and label == "Reset all? click again"
    assert "config.toml.bak-" in note, "the note names the backup it keeps, before the 2nd click"
    assert seen["after_one"] == before, "one click changes nothing"
    assert seen["disarmed"] is True and seen["after_escape"] == before
    assert seen["pushed"] == 1, "no dialog"
    assert not paths.config_path().exists(), "every setting is its default now"
    backups = sorted(paths.config_path().parent.glob("config.toml.bak-*"))
    assert len(backups) == 1 and backups[0].read_bytes() == before, "the file kept, unchanged"
    defaults = load_config()
    assert seen["form"] == (str(defaults.fleet.max_agents_per_project), defaults.accounts.pick)
    notices = seen["notices"]
    assert isinstance(notices, list)
    assert any(str(backups[0]) in m for m, _ in notices), notices


def test_reset_all_with_nothing_changed_says_so() -> None:
    assert not paths.config_path().exists()

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[tuple[str, str]]:
        host.query_one("#reset-settings", Button).press()
        await settle(pilot)
        host.query_one("#reset-settings", Button).press()
        await settle(pilot)
        return host.notices

    notices = drive(scenario)
    assert any("at its default already" in m for m, _ in notices), notices
    assert not list(paths.config_path().parent.glob("config.toml.bak-*"))


def test_reset_all_of_a_file_holding_no_setting_keeps_no_backup() -> None:
    """A save of the defaults leaves a file (its version, the role tables): Reset all of it
    backed it up and said "yours are kept" (review of #261, m8)."""
    save_config(load_config())
    assert paths.config_path().exists()

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[tuple[str, str]]:
        host.query_one("#reset-settings", Button).press()
        await settle(pilot)
        host.query_one("#reset-settings", Button).press()
        await settle(pilot)
        return host.notices

    notices = drive(scenario)
    assert any(m.startswith("nothing to reset") for m, _ in notices), notices
    assert not list(paths.config_path().parent.glob("config.toml.bak-*"))


def test_a_form_left_open_saves_only_what_was_changed_on_it() -> None:
    """The reviewer's repro (review of #261, m7): the file holds ``escape_key = "f11"`` and
    ``switch_at = 70`` when App settings opens. From a shell, ``config set accounts.pick
    default``; Save with max agents changed keeps it. Then ``config reset``; Save with max
    agents changed again keeps the reset: the form wrote back every field it showed, and
    undid both."""
    config = load_config()
    config.fleet.escape_key = "f11"
    config.accounts.switch_at = 70
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, object]:
        seen: dict[str, object] = {}
        settings_service.set_value("accounts.pick", "default")  # a shell, while it is open
        host.query_one("#max-agents", Input).value = "5"
        leave_fields(host)
        await settle(pilot)
        seen["set"] = load_config()
        settings_service.reset_settings()  # a shell again: the form still shows f11, 70
        host.query_one("#max-agents", Input).value = "6"
        leave_fields(host)
        await settle(pilot)
        seen["reset"] = load_config()
        seen["form"] = host.query_one("#escape-key", Input).value
        return seen

    seen = drive(scenario)
    kept = seen["set"]
    assert isinstance(kept, AppConfig)
    assert kept.accounts.pick == "default", "the shell's config set stands"
    assert (kept.fleet.escape_key, kept.accounts.switch_at) == ("f11", 70)
    assert kept.fleet.max_agents_per_project == 5
    after = seen["reset"]
    assert isinstance(after, AppConfig)
    assert overrides(after) == {"fleet": {"max_agents_per_project": 6}}, "the reset stands"
    assert seen["form"] == "f12", "after Save the form shows what the file holds"


def test_a_role_added_by_hand_while_the_form_is_open_costs_nothing_on_save() -> None:
    """Review of #259, round 21: ``[fleet.roles.designer]`` written by hand, then Reload,
    put ``designer`` in the form's roles with no row on it, and the next Save looked up
    ``#perm-designer`` unguarded: ``NoMatches`` out of the button handler, and the fleet
    UI exited. The row is shown after a reopen; until then Save leaves the role as the
    file has it."""
    from aisquare.core.config import FleetRoleSettings

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[tuple[str, str]]:
        config = load_config()  # a hand edit, while the form is open
        config.fleet.roles["designer"] = FleetRoleSettings(permission_mode="plan")
        save_config(config)
        await reopen(pilot, host.query_one(AppSettingsView))
        await settle(pilot)
        host.query_one("#role-perm-coder", Select).value = "acceptEdits"
        leave_fields(host)
        await settle(pilot)
        host.query_one("#max-agents", Input).value = "4"
        leave_fields(host)  # and again, from the reloaded form
        await settle(pilot)
        assert not host.query("#role-perm-designer")  # no row yet: a reopen shows it
        return host.notices

    notices = drive(scenario)
    assert [m for m, severity in notices if severity == "error"] == []
    assert sum(m.startswith("Saved ") for m, _ in notices) == 2, notices
    roles = _config_toml()["fleet"]["roles"]
    assert roles["coder"]["permission_mode"] == "acceptEdits"
    assert roles["designer"]["permission_mode"] == "plan"  # the hand edit stands
    assert load_config().fleet.max_agents_per_project == 4

    async def reopened(pilot: Pilot[None], host: AppHost) -> object:
        return host.query_one("#role-perm-designer", Select).value

    assert drive(reopened) == "plan"


def test_two_roles_whose_names_differ_only_in_punctuation_keep_their_own_rows() -> None:
    """Review of #259, round 22, 3/9: a widget id turned every character outside
    ``[A-Za-z0-9_-]`` into ``-``, so ``qa.ui`` and ``qa-ui`` shared one id. Each role's
    widgets sit in their own row, so Textual raised nothing; every lookup found the first
    row's. The second row was shown and never read, and Save wrote the first row's values
    to both roles. Each role has its own id now, and Save changes the row it was asked to."""
    from aisquare.cli.ui.views.app_settings import widget_suffix
    from aisquare.core.config import FleetRoleSettings

    names = ["qa.ui", "qa-ui", "qa_ui", "qa_2e_ui", "测试", "审查", "coder", "ui-tester"]
    assert len({widget_suffix(name) for name in names}) == len(names)
    assert widget_suffix("coder") == "coder" and widget_suffix("ui-tester") == "ui-tester"

    config = load_config()
    config.fleet.roles["qa.ui"] = FleetRoleSettings(permission_mode="plan", worktree=True)
    config.fleet.roles["qa-ui"] = FleetRoleSettings(permission_mode="default", worktree=True)
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[object]:
        dotted, dashed = (f"#role-perm-{widget_suffix(role)}" for role in ("qa.ui", "qa-ui"))
        shown = [host.query_one(dotted, Select).value, host.query_one(dashed, Select).value]
        host.query_one(f"#role-worktree-{widget_suffix('qa-ui')}", Switch).value = False
        leave_fields(host)
        await settle(pilot)
        return shown

    assert drive(scenario) == ["plan", "default"], "each row shows its own role"
    roles = load_config().fleet.roles
    assert (roles["qa.ui"].permission_mode, roles["qa.ui"].worktree) == ("plan", True)
    assert (roles["qa-ui"].permission_mode, roles["qa-ui"].worktree) == ("default", False)


def test_a_role_named_like_a_fixed_field_neither_takes_its_id_nor_takes_the_form_down() -> None:
    """Review of #259, round 23, 2/10: ``[fleet.roles.dir]`` gave its worktree switch the id
    ``worktree-dir``, the worktree root's ``Input``. The two sit in different rows, so
    Textual raised no ``DuplicateIds``, and ``query_one("#worktree-dir", Input)`` found the
    role's ``Switch`` first: ``WrongType`` out of ``on_mount``, and the fleet UI went down
    at App settings. Role names are free TOML keys, so every per-role id sits under a
    prefix no fixed id has, and each field still saves what it shows."""
    from aisquare.cli.ui.views.app_settings import role_order
    from aisquare.core.config import FleetRoleSettings

    config = load_config()
    for role in ("dir", "qa.ui", "qa-ui", "max-agents"):
        config.fleet.roles[role] = FleetRoleSettings(permission_mode="plan", worktree=True)
    save_config(config)
    roles = role_order(load_config().fleet)

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[str]:
        form = host.query_one(AppSettingsView)
        fields = form.query("Input, Select, Switch, Button")
        ids = [str(widget.id) for widget in fields if widget.id]
        host.query_one("#worktree-dir", Input).value = ".trees"
        from aisquare.cli.ui.views.app_settings import role_widget_id  # the fix's own name

        host.query_one(f"#{role_widget_id('worktree', 'dir')}", Switch).value = False
        host.query_one(f"#{role_widget_id('perm', 'qa-ui')}", Select).value = "acceptEdits"
        leave_fields(host)
        await settle(pilot)
        return ids

    ids = drive(scenario)
    from aisquare.cli.ui.views.app_settings import ROLE_ID_PREFIX, ROLE_SELECTS, role_widget_id

    assert len(ids) == len(set(ids)), "one field per id, on the whole form"
    assert {wid for wid in ids if wid.startswith(ROLE_ID_PREFIX)} == {
        role_widget_id(kind, role)
        for kind in ("perm", "worktree", "persona", "acct", *ROLE_SELECTS)
        for role in roles
    }, "the prefix is the roles' alone"
    saved = load_config().fleet
    assert saved.worktree_dir == ".trees"
    assert (saved.roles["dir"].permission_mode, saved.roles["dir"].worktree) == ("plan", False)
    assert saved.roles["qa-ui"].permission_mode == "acceptEdits"
    assert saved.roles["qa.ui"].permission_mode == "plan"


def test_role_names_modes_and_bindings_written_by_hand_are_shown_as_text_not_markup() -> None:
    """Review of #259, round 23, 4/10: ``Label(role)`` read the role's name as Textual
    markup, and role names are free TOML keys. ``[fleet.roles."qa[x]"]`` showed as ``qa``,
    and ``[fleet.roles."a[/b]c"]`` raised ``MarkupError`` when its row rendered, which took
    the fleet UI down at App settings. The same row's selects read a mode and an account
    binding written by hand as markup too. Each is shown as it is written."""
    from textual.widgets import Label

    from aisquare.core.config import FleetRoleSettings, RoleLaunchProfile

    config = load_config()
    config.fleet.roles["a[/b]c"] = FleetRoleSettings(permission_mode="x[/y]")
    config.fleet.roles["qa[x]"] = FleetRoleSettings(permission_mode="plan")
    config.team.profiles["a[/b]c"] = RoleLaunchProfile(account="w[/z]")
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[list[str], list[str]]:
        await settle(pilot)
        form = host.query_one(AppSettingsView)
        names = [str(label.render()) for label in form.query(Label)]
        prompts = [
            str(prompt) for select in form.query(Select) for prompt, _value in select._options
        ]
        host.query_one("#max-agents", Input).value = "5"
        leave_fields(host)
        await settle(pilot)
        return names, prompts

    names, prompts = drive(scenario)

    assert "a[/b]c" in names and "qa[x]" in names, names
    assert "x[/y] (custom)" in prompts and "w[/z] (as written)" in prompts, prompts
    saved = load_config()
    assert saved.fleet.max_agents_per_project == 5
    assert saved.fleet.roles["a[/b]c"].permission_mode == "x[/y]", "the hand edit stands"
    assert saved.team.profiles["a[/b]c"].account == "w[/z]"


def test_the_accounts_choices_say_most_room_left_not_headroom() -> None:
    """Q15 (the UX audit): plain words. *Launches pick* and *on a usage limit* said "the
    account with headroom"; they say "the account with the most room left", and the saved
    values stay what they were (``headroom``, ``switch``)."""
    config = load_config()
    config.accounts.pick = "headroom"
    config.accounts.on_limit = "switch"
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[str]:
        await settle(pilot)
        return [
            str(host.query_one(f"{wid} SelectCurrent #label", Static).render())
            for wid in ("#accounts-pick", "#accounts-on-limit")
        ]

    shown = drive(scenario)
    assert shown == [
        "the account with the most room left",
        "switch to the account with the most room left",
    ]
    labels = [label for label, _value in (*PICK_MODES, *ON_LIMIT_MODES)]
    assert not [label for label in labels if "headroom" in label], labels
    assert [value for _label, value in PICK_MODES] == ["default", "headroom"], "values kept"


def test_the_usage_limit_choices_name_no_agent() -> None:
    """SCOPE #113, 0.8.1 item 19: *wait* read "Claude Code continues by itself", which a
    Codex agent does not (it waits for its next prompt). The choice is every agent's."""
    said = " ".join(label for label, _value in ON_LIMIT_MODES)
    for adapter in adapters():
        assert adapter.label not in said, adapter.label
        assert adapter.binary not in said.split(), adapter.binary
    assert [value for _label, value in ON_LIMIT_MODES] == ["wait", "switch"]


# --- the persona per role (#201) ------------------------------------------------------------


def _persona_prompts(host: AppHost, role: str) -> list[str]:
    from textual.widgets import OptionList

    from aisquare.cli.ui.views.app_settings import role_widget_id

    field = host.query_one(f"#{role_widget_id('persona', role)}", Select)
    overlay = field.query_one(OptionList)
    return [str(overlay.get_option_at_index(i).prompt) for i in range(overlay.option_count)]


def test_a_roles_persona_is_saved_to_config_toml_and_read_back() -> None:
    async def scenario(pilot: Pilot[None], host: AppHost) -> list[tuple[str, str]]:
        await pilot.pause()
        host.query_one("#role-persona-coder", Select).value = "skeptic"
        leave_fields(host)
        await settle(pilot)
        return host.notices

    notices = drive(scenario)
    assert any(m.startswith("Saved ") for m, _ in notices), notices
    roles = _config_toml()["fleet"]["roles"]
    assert roles["coder"]["persona"] == "skeptic"  # the bytes, under the coder's own table
    assert "persona" not in roles.get("tester", {})
    assert load_config().fleet.roles["coder"].persona == "skeptic"


def test_choosing_none_removes_the_roles_persona_rather_than_writing_an_empty_one() -> None:
    from aisquare.core.config import FleetRoleSettings

    config = load_config()
    config.fleet.roles["coder"] = FleetRoleSettings(persona="skeptic")
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> object:
        await pilot.pause()
        field = host.query_one("#role-persona-coder", Select)
        before = field.value
        field.value = ""
        leave_fields(host)
        await settle(pilot)
        return before

    assert drive(scenario) == "skeptic"
    assert "persona" not in _config_toml()["fleet"]["roles"]["coder"]
    assert load_config().fleet.roles["coder"].persona is None


def test_a_project_only_persona_is_never_offered_as_a_machine_wide_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Review of #240, "also confirmed": the form saved a persona only the open project
    holds as ``[fleet.roles.<role>].persona``, a machine-wide default, so every other
    project's spawn of that role refused it. The form lists the personas every project
    can resolve — the user's and the bundled ones — and a configured name it lacks shows
    as ``(custom)``, kept rather than dropped by a save."""
    from aisquare.core.config import FleetRoleSettings

    repo = tmp_path / "repo"
    skill = repo / ".aisquare" / "personas" / "local-only" / "SKILL.md"
    skill.parent.mkdir(parents=True)
    skill.write_text(
        "---\nname: local-only\ndescription: Only this project has it.\n---\nBe local.\n",
        encoding="utf-8",
    )
    (repo / ".git").mkdir()
    monkeypatch.chdir(repo)
    config = load_config()
    config.fleet.roles["tester"] = FleetRoleSettings(persona="ghost")
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[list[str], list[str]]:
        await pilot.pause()
        return _persona_prompts(host, "coder"), _persona_prompts(host, "tester")

    coder, tester = drive(scenario)
    assert coder[0] == "(none)" and "skeptic" in coder
    assert "local-only" not in coder, "a project's own persona is not a machine-wide default"
    assert tester[-1] == "ghost (custom)"


def test_the_form_says_why_it_lists_no_personas_when_they_cannot_be_read(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def unreadable(root: Path | None = None) -> Any:
        raise PermissionError(13, "Permission denied", "/h/.aisquare/personas")

    monkeypatch.setattr("aisquare.core.personas.catalogue", unreadable)

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, str, list[str]]:
        await pilot.pause()
        note = host.query_one("#settings-personas-unavailable", Static)
        return bool(note.display), str(note.render()), _persona_prompts(host, "coder")

    shown, text, prompts = drive(scenario)
    assert shown
    assert text == (
        "personas unavailable — PermissionError: [Errno 13] Permission denied: "
        "'/h/.aisquare/personas'"
    )
    assert prompts == ["(none)"]  # no names, and the form is still up


# --- role bindings (#98) -----------------------------------------------------------------


def _bind_cli(*argv: str) -> None:
    from typer.testing import CliRunner

    from aisquare.cli.app import app

    result = CliRunner().invoke(app, ["team", "bind", *argv])
    assert result.exit_code == 0, result.output


def _after_cli(*argv: str) -> bytes:
    """The bytes ``team bind <argv>`` would leave, the file put back as it was after."""
    before = paths.config_path().read_bytes()
    _bind_cli(*argv)
    after = paths.config_path().read_bytes()
    paths.config_path().write_bytes(before)
    return after


def test_role_bindings_list_each_binding_with_its_claude_dirs_called_out() -> None:
    """#98: nothing in the UI showed a binding. Each bound role is listed with its binary,
    its args, CLAUDE_CONFIG_DIR and CLAUDE_CODE_TMPDIR with their values, and any other
    variable by name only, since its value may be a token."""
    _bind_cli(
        "coder",
        "--bin",
        "claude2",
        "--env",
        "CLAUDE_CONFIG_DIR=/srv/claude-2",
        "--env",
        "CLAUDE_CODE_TMPDIR=/srv/tmp",
        "--env",
        "GH_TOKEN=ghp_secret",
        "--arg",
        "--model",
        "--arg",
        "opus",
    )

    async def scenario(pilot: Pilot[None], host: AppHost) -> str:
        await settle(pilot)
        row = host.query_one("#binding-row-coder")
        return "\n".join(str(widget.render()) for widget in row.query(Static))

    shown = drive(scenario)
    assert "coder" in shown and "claude2" in shown
    assert "CLAUDE_CONFIG_DIR = /srv/claude-2" in shown
    assert "CLAUDE_CODE_TMPDIR = /srv/tmp" in shown
    assert "GH_TOKEN" in shown and "ghp_secret" not in shown
    assert "args --model opus" in shown


def test_a_roles_account_select_names_each_account_by_who_it_is() -> None:
    """Q1 (the test of #676, #4324): the select beside each role listed ``2 · account 2 ·
    two@example.com``. It names an account as the Spawn dialog and the Accounts page do: the
    slot, its alias if it has one, then its email; and its first row and a value written by
    hand in preset words (Q15), never "binding" or "bound"."""
    from aisquare.core import claude_accounts as accounts_core
    from aisquare.services import claude_accounts as accounts_service
    from tests.test_claude_accounts import _sign_in

    two, three, four = (accounts_core.create_account() for _ in range(3))
    _sign_in(two, "two@example.com")
    _sign_in(three, "w@example.com")
    accounts_service.set_alias(str(three.slot), "work")
    listed = [account for account in accounts_service.list_accounts() if account.slot > 1]

    labels = [str(label) for label, _ in account_options(listed, "laptop")]
    assert labels == [
        "(no preset account)",
        f"{two.slot} · two@example.com",
        f"{three.slot} · work · w@example.com",
        f"{four.slot} · not signed in",
        "laptop (as written)",
    ]


def test_no_binding_shows_how_to_make_one() -> None:
    """Q15, finding 6 of the 0.10.2 gate (#4295): a role's binding is a *preset*, as the
    Spawn dialog's Preset row and its picker's *+ New preset* say; here *+ New bind* opened a
    form headed *New preset*. The section, its button, its empty line and the note that
    points to it say preset too."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[str]:
        await settle(pilot)
        return [
            str(host.query_one("#bindings-empty", Static).render()),
            str(host.query_one("#bindings-new", Button).label),
            str(host.query_one("#agent-settings-note", Static).render()),
        ]

    said, button, pointer = drive(scenario)
    assert said == "no presets — + New preset makes one", "the button, never a command (R13)"
    assert button == "+ New preset"
    assert AppSettingsView.bindings_section()[0] == "Presets"
    assert "is under Presets below" in pointer, pointer
    for shown in (said, button, pointer):
        assert "bind" not in shown, shown


def test_edit_opens_the_bind_form_on_the_binding_and_writes_what_team_bind_writes(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Edit is the bind form on the binding as it is; Save replaces it, exactly as
    `team bind --clear` then `team bind` with the new values would."""
    from textual.widgets import TextArea

    from aisquare.cli.ui.attach import MASKED, NewBindScreen

    monkeypatch.setattr(
        "aisquare.cli.ui.attach.shutil.which", lambda name: f"/usr/local/bin/{name}"
    )
    _bind_cli("coder", "--bin", "claude2", "--env", "A=1", "--arg", "--model", "--arg", "opus")
    expected = _after_cli("coder", "--clear")
    paths.config_path().write_bytes(expected)
    expected = _after_cli("coder", "--bin", "claude2", "--env", "B=3", "--arg", "-v")
    paths.config_path().unlink()
    _bind_cli("coder", "--bin", "claude2", "--env", "A=1", "--arg", "--model", "--arg", "opus")

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, object]:
        await settle(pilot)
        host.query_one("#binding-edit-coder", Button).press()
        await settle(pilot)
        screen = host.screen
        seen: dict[str, object] = {"form": isinstance(screen, NewBindScreen)}
        seat = screen.query_one("#bind-seat", Input)
        seen["prefilled"] = (
            seat.value,
            seat.disabled,
            screen.query_one("#bind-binary", Input).value,
            screen.query_one("#bind-env", TextArea).text,
            screen.query_one("#bind-args", Input).value,
        )
        screen.query_one("#bind-env", TextArea).load_text("B=3")
        screen.query_one("#bind-args", Input).value = "-v"
        await settle(pilot)
        screen.query_one("#bind-save", Button).press()
        await settle(pilot)
        seen["shown"] = str(host.query_one("#binding-row-coder").query(Static).first().render())
        return seen

    seen = drive(scenario)
    assert seen["form"] is True
    assert seen["prefilled"] == ("coder", True, "claude2", f"A={MASKED}", "--model opus")
    assert paths.config_path().read_bytes() == expected
    assert "args -v" in str(seen["shown"]), "the section shows what the file holds now"


def test_the_unset_dialogs_button_is_guarded_against_the_opener_s_double_click() -> None:
    """REVIEW-RC092-MI round 3, MAJOR 1: the dialog is centred and does not move with the page,
    so with three bindings the first role's *Unset key…* can be scrolled under its Unset (36
    cells at 140x44). A GuardedButton ignores the second click of a double-click begun on
    something else (``tests/test_click_guard.py``)."""
    from aisquare.cli.ui.click_guard import GuardedButton
    from aisquare.cli.ui.views.app_settings import UnsetKeyScreen

    _bind_cli("tester", "--env", "A=1")

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, object]:
        await settle(pilot)
        host.query_one("#binding-unset-tester", Button).press()
        await settle(pilot)
        dialog = host.screen
        return {
            "asked": isinstance(dialog, UnsetKeyScreen),
            "guarded": isinstance(dialog.query_one("#unset-confirm"), GuardedButton),
        }

    assert drive(scenario) == {"asked": True, "guarded": True}


def test_unset_key_and_clear_write_what_team_bind_writes() -> None:
    """Clear asks in place: one press arms it and the note line under the buttons, always in
    sight, says the whole binding goes, and it writes nothing; the focus leaving disarms it; a
    second press on it clears."""
    from aisquare.cli.ui.views.app_settings import UnsetKeyScreen

    _bind_cli("tester", "--env", "A=1", "--env", "B=2", "--arg", "-v")
    after_unset = _after_cli("tester", "--unset", "B")

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, object]:
        seen: dict[str, object] = {}
        await settle(pilot)
        host.query_one("#binding-unset-tester", Button).press()
        await settle(pilot)
        seen["asked"] = isinstance(host.screen, UnsetKeyScreen)
        host.screen.query_one("#unset-key", Select).value = "B"
        host.screen.query_one("#unset-confirm", Button).press()
        await settle(pilot)
        seen["unset"] = paths.config_path().read_bytes()
        seen["after_clear"] = _after_cli("tester", "--clear")
        host.query_one(f"#{AGENT_ADVANCED}", Collapsible).collapsed = False
        clear = host.query_one("#binding-clear-tester", ConfirmButton)
        await settle(pilot)
        clear.scroll_visible(animate=False)
        await settle(pilot)
        clear.focus()
        clear.press()
        await settle(pilot)
        note = host.query_one("#settings-note", Static)
        seen["armed"] = (clear.armed, str(clear.label), len(host.screen_stack))
        seen["note"] = (note.display, str(note.content))
        seen["after_one"] = paths.config_path().read_bytes()
        host.query_one("#binding-edit-tester", Button).focus()
        await settle(pilot)
        seen["disarmed"] = (clear.armed, str(clear.label), str(note.content).startswith("Saved"))
        clear.press()
        await settle(pilot)
        clear.press()
        await settle(pilot)
        seen["cleared"] = paths.config_path().read_bytes()
        seen["empty"] = bool(host.query("#bindings-empty"))
        return seen

    seen = drive(scenario)
    assert seen["asked"] is True and seen["unset"] == after_unset
    assert seen["armed"] == (True, "Clear? click again", 1), "no dialog"
    shown, said = cast(tuple[bool, str], seen["note"])
    assert shown is True and said.startswith("The whole preset of tester goes"), said
    assert seen["after_one"] == after_unset, "one press writes nothing"
    assert seen["disarmed"] == (False, "Clear", True), "the note line has its own text back"
    assert seen["cleared"] == seen["after_clear"]
    assert seen["empty"] is True


def test_the_role_bindings_section_fits_80_columns() -> None:
    """The fleet UI runs in an 80-column tmux pane as often as not: a binding's row, its
    called-out paths and its three buttons stay inside it, its Clear armed too."""
    _bind_cli("reviewer", "--env", "CLAUDE_CONFIG_DIR=/srv/claude-accounts/2", "--arg", "-v")

    async def run() -> tuple[int, list[int], int]:
        host = AppHost()
        async with host.run_test(size=(80, 40)) as pilot:
            await settle(pilot)
            host.query_one(f"#{AGENT_ADVANCED}", Collapsible).collapsed = False
            await settle(pilot)
            row = host.query_one("#binding-row-reviewer")
            row.scroll_visible(animate=False)
            await settle(pilot)
            buttons = [button.region.right for button in row.query(Button)]
            clear = row.query_one("#binding-clear-reviewer", ConfirmButton)
            clear.focus()
            clear.press()
            await settle(pilot)
            assert clear.armed
            return row.region.right, buttons, clear.region.right

    right, buttons, armed = asyncio.run(run())
    assert 0 < right <= 80 and buttons and max(buttons) <= 80, (right, buttons)
    assert armed <= 80, armed


def _binding_shown(host: App[Any], role: str) -> str | None:
    """What the section shows for ``role``'s binding, or ``None`` when it lists none."""
    rows = host.query(f"#binding-row-{role}")
    if not rows:
        return None
    return "\n".join(str(widget.render()) for widget in rows.first().query(Static))


def test_reload_shows_the_bindings_a_shell_wrote_meanwhile() -> None:
    """Review of #334 (reviewer-a's walk): Reload re-read every field but the bindings, so
    a role bound in a shell never showed, and a key unset there stayed listed."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[str | None, str | None]:
        await settle(pilot)
        settings_service.bind_role("coder", env={"GH_TOKEN": "x"}, args=["-v"])
        await reopen(pilot, host.query_one(AppSettingsView))
        await settle(pilot)
        bound = _binding_shown(host, "coder")
        settings_service.bind_role("coder", unset=["GH_TOKEN"])
        await reopen(pilot, host.query_one(AppSettingsView))
        await settle(pilot)
        return bound, _binding_shown(host, "coder")

    bound, unset = drive(scenario)
    assert bound is not None and "GH_TOKEN" in bound
    assert unset is not None and "GH_TOKEN" not in unset


def test_edit_opens_on_the_binding_the_file_holds_now_not_the_one_last_shown(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Save writes the edited binding whole, so an Edit opened on what the section last
    read wrote a shell's later `team bind` away (review of #334). The page sits open
    beside the shell in a tmux split, shown all along: only Edit itself can read it."""
    monkeypatch.setattr(
        "aisquare.cli.ui.attach.shutil.which", lambda name: f"/usr/local/bin/{name}"
    )
    settings_service.bind_role("coder", args=["-v"])

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[str, list[str]]:
        await settle(pilot)
        settings_service.bind_role("coder", args=["--debug"])  # the shell, meanwhile
        host.query_one("#binding-edit-coder", Button).press()
        await settle(pilot)
        prefilled = host.screen.query_one("#bind-args", Input).value
        host.screen.query_one("#bind-save", Button).press()
        await settle(pilot)
        return prefilled, load_config().team.profiles["coder"].args

    prefilled, saved = drive(scenario)
    assert prefilled == "-v --debug"
    assert saved == ["-v", "--debug"], "the shell's arg survives an Edit and Save"


def test_edit_of_a_binding_cleared_meanwhile_says_so_and_opens_nothing() -> None:
    settings_service.bind_role("coder", args=["-v"])

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, str | None]:
        await settle(pilot)
        settings_service.clear_role_binding("coder")
        host.query_one("#binding-edit-coder", Button).press()
        await settle(pilot)
        return host.screen is not host.query_one(AppSettingsView).screen, _binding_shown(
            host, "coder"
        )

    opened, shown = drive(scenario)
    assert opened is False and shown is None


def test_edit_masks_every_env_value_until_show_values_and_keeps_one_left_masked(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The section names GH_TOKEN and never shows its value, and Edit showed it in clear
    (review of #334). Each value is masked until *Show values*; a masked value saves as
    the one stored, and a value typed over its mask saves as typed."""
    from textual.widgets import TextArea

    from aisquare.cli.ui.attach import MASKED

    monkeypatch.setattr(
        "aisquare.cli.ui.attach.shutil.which", lambda name: f"/usr/local/bin/{name}"
    )
    settings_service.bind_role("coder", env={"GH_TOKEN": "ghp_secret", "A": "1"})

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[str]:
        await settle(pilot)
        host.query_one("#binding-edit-coder", Button).press()
        await settle(pilot)
        area = host.screen.query_one("#bind-env", TextArea)
        reveal = host.screen.query_one("#bind-env-reveal", Switch)
        seen = [area.text]
        reveal.value = True
        await settle(pilot)
        seen.append(area.text)
        reveal.value = False
        await settle(pilot)
        seen.append(area.text)
        area.load_text(f"GH_TOKEN={MASKED}\nA=2")
        await settle(pilot)
        host.screen.query_one("#bind-save", Button).press()
        await settle(pilot)
        return seen

    masked, shown, again = drive(scenario)
    assert masked == again == f"GH_TOKEN={MASKED}\nA={MASKED}"
    assert shown == "GH_TOKEN=ghp_secret\nA=1"
    assert load_config().team.profiles["coder"].env == {"GH_TOKEN": "ghp_secret", "A": "2"}


def test_the_keyboard_reaches_new_bind_opens_the_form_and_escape_closes_it() -> None:
    """Step 6 of the PR's walk, which the tmux walk could not prove end to end: at 80
    columns, Tab reaches *Agent settings*' Advanced and Enter unfolds it, Tab reaches
    *+ New bind*, Enter opens the bind form, and Escape closes it having written nothing."""
    from aisquare.cli.ui.attach import NewBindScreen

    async def run() -> tuple[str | None, bool, bool]:
        host = AppHost()
        async with host.run_test(size=(80, 40)) as pilot:
            await settle(pilot)
            advanced = host.query_one(f"#{AGENT_ADVANCED}", Collapsible)
            for _ in range(120):
                await pilot.press("tab")
                focused = host.focused
                if focused is not None and focused.id == "bindings-new":
                    break
                if advanced.collapsed and focused is not None and focused.parent is advanced:
                    await pilot.press("enter")  # its title: unfold it
                    await settle(pilot)
            reached = host.focused.id if host.focused is not None else None
            await pilot.press("enter")
            await settle(pilot)
            opened = isinstance(host.screen, NewBindScreen)
            await pilot.press("escape")
            await settle(pilot)
            return reached, opened, isinstance(host.screen, NewBindScreen)

    reached, opened, still_open = asyncio.run(run())
    assert reached == "bindings-new"
    assert opened is True and still_open is False
    assert not paths.config_path().exists(), "Escape writes nothing"


# --- the two tabs and User preferences ---------------------------------------------------


def test_the_form_opens_on_agent_settings_beside_preferences_display_and_remote() -> None:
    """The roles, fleet and accounts are *Agent settings*, shown first; *Display* (R17) holds
    *Always show project directory*, off by default, with its one line of why, beside the other
    switches of what asq shows, every one on. Save, Reload and Reset all sit under the tabs on
    the two forms', and hide on *Display*, whose switches save as they are ticked."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, object]:
        tabs = host.query_one("#app-settings-tabs", TabbedContent)
        seen: dict[str, object] = {
            "labels": [
                str(tabs.get_tab(pane).label)
                for pane in (AGENT_TAB, PREFERENCES_TAB, DISPLAY_TAB, REMOTE_TAB)
            ],
            "order": [pane.id for pane in tabs.query(TabPane)],
            "active": tabs.active,
            "agent fields": all(
                host.query_one(f"#{AGENT_TAB}").query(wid)
                for wid in ("#role-perm-coder", "#max-agents", "#accounts-pick")
            ),
            "switch": host.query_one("#ui-always-show-path", Checkbox).value,
            "others": {
                wid: host.query_one(f"#{wid}", Checkbox).value
                for wid in DISPLAY_CHOICES
                if wid != "ui-always-show-path"
            },
            "about": str(host.query_one("#ui-always-show-path-about", Static).render()),
            "from a shell": str(host.query_one("#preferences-from-a-shell", Static).render()),
            "in display": bool(host.query_one(f"#{DISPLAY_TAB}").query("#ui-always-show-path")),
        }
        tabs.active = PREFERENCES_TAB
        await settle(pilot)
        seen["buttons shown"] = all(
            host.query_one(wid, Button).region.area > 0 for wid in ("#reset-settings",)
        )
        tabs.active = DISPLAY_TAB
        await settle(pilot)
        seen["buttons on display"] = host.query_one("#reset-settings", Button).region.area > 0
        return seen

    seen = drive(scenario)
    assert seen["labels"] == ["Agent settings", "User preferences", "Display", "Remote"]
    assert seen["order"] == [AGENT_TAB, PREFERENCES_TAB, DISPLAY_TAB, REMOTE_TAB]
    assert seen["active"] == AGENT_TAB
    assert seen["agent fields"] is True
    assert seen["in display"] is True
    assert seen["switch"] is False, "off by default: the sidebar keeps today's rule"
    others = cast(dict[str, bool], seen["others"])
    assert set(others.values()) == {True}, others
    assert seen["about"] == (
        "Show each project's path under its name in the sidebar. "
        "Off: shown only when two projects share a name."
    )
    assert seen["from a shell"] == (
        "A preference changed outside rlx shows when you next open this page, or when rlx "
        "next starts; not live."
    )
    assert seen["buttons shown"] is True
    assert seen["buttons on display"] is False, "nothing on Display for Reset all"


def test_always_show_project_directory_saves_itself_alone_as_it_is_ticked() -> None:
    """Display's switch is written at its tick, alone, with no Save, and the app is told; a
    Save of the other tab later leaves it as it stands."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[bool]:
        await pilot.pause()
        host.query_one("#ui-always-show-path", Checkbox).value = True
        await settle(pilot)
        return host.ui_told

    assert drive(scenario) == [True], "the tick told the app what [ui] holds now"
    assert _config_toml()["ui"] == {"always_show_project_path": True}  # the bytes
    assert overrides(load_config()) == {"ui": {"always_show_project_path": True}}, (
        "nothing on the other tabs was written"
    )

    async def reopened(pilot: Pilot[None], host: AppHost) -> bool:
        await pilot.pause()
        shown = host.query_one("#ui-always-show-path", Checkbox).value
        host.query_one("#max-agents", Input).value = "6"  # a change on the other tab
        leave_fields(host)
        await settle(pilot)
        return shown

    assert drive(reopened) is True, "the form opened on what the file holds"
    assert load_config().ui.always_show_project_path is True, "an untouched preference stands"
    assert load_config().fleet.max_agents_per_project == 6


def test_each_display_switch_saves_its_own_key_and_tells_the_app() -> None:
    """R17: every Display switch is one ``[ui]`` key, saved alone at its tick (off, as each is
    on by default) and back on by leaving the key out; the app hears each."""

    async def scenario(
        pilot: Pilot[None], host: AppHost
    ) -> dict[str, tuple[dict[str, Any] | None, list[bool]]]:
        await pilot.pause()
        seen: dict[str, tuple[dict[str, Any] | None, list[bool]]] = {}
        for wid, key in DISPLAY_CHOICES.items():
            if key == "always_show_project_path":
                continue
            host.uis.clear()
            host.query_one(f"#{wid}", Checkbox).value = False
            await settle(pilot)
            seen[key] = (_config_toml().get("ui"), [getattr(ui, key) for ui in host.uis])
            host.query_one(f"#{wid}", Checkbox).value = True
            await settle(pilot)
        return seen

    seen = drive(scenario)
    assert len(seen) == len(DISPLAY_CHOICES) - 1
    for key, (written, heard) in seen.items():
        assert written == {key: False}, (key, written)
        assert heard == [False], (key, heard)
    assert not paths.config_path().exists() or "ui" not in _config_toml()


def test_the_marks_switch_names_every_adapters_mark() -> None:
    """Display's *Agent marks* switch, as drawn, names each mark an adapter declares
    (``AgentAdapter.mark``), so a new agent's mark joins it with no edit to the view."""
    glyphs = [adapter.mark.glyph for adapter in adapters() if adapter.mark is not None]

    async def scenario(pilot: Pilot[None], host: AppHost) -> str:
        await pilot.pause()
        return str(host.query_one("#ui-show-agent-marks", Checkbox).label)

    drawn = drive(scenario)
    assert drawn == display_label("ui-show-agent-marks"), drawn
    assert glyphs and all(glyph in drawn for glyph in glyphs), (glyphs, drawn)


def test_a_display_switch_over_an_unreadable_file_goes_back_and_writes_nothing() -> None:
    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, list[tuple[str, str]]]:
        await pilot.pause()
        paths.config_path().parent.mkdir(parents=True, exist_ok=True)
        paths.config_path().write_text("[ui\nnot toml", encoding="utf-8")  # broken meanwhile
        box = host.query_one("#ui-bell", Checkbox)
        box.value = False
        await settle(pilot)
        return box.value, host.notices

    shown, notices = drive(scenario)
    assert shown is True, "the switch goes back to what the file still says"
    assert any(said.startswith("not saved: ") for said, _ in notices), notices
    assert paths.config_path().read_text(encoding="utf-8") == "[ui\nnot toml"


def test_turning_the_preference_off_again_leaves_no_ui_table() -> None:
    settings_service.set_value("ui.always_show_project_path", "true")

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[bool]:
        await pilot.pause()
        host.query_one("#ui-always-show-path", Checkbox).value = False
        await settle(pilot)
        return host.ui_told

    assert drive(scenario) == [False]
    assert "ui" not in _config_toml(), "the default is kept by leaving the key out"
    assert load_config().ui.always_show_project_path is False


def test_reload_reads_the_preference_a_shell_set_meanwhile() -> None:
    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, list[bool]]:
        await pilot.pause()
        settings_service.set_value("ui.always_show_project_path", "true")  # from a shell
        await reopen(pilot, host.query_one(AppSettingsView))
        await settle(pilot)
        return host.query_one("#ui-always-show-path", Checkbox).value, host.ui_told

    # The reload's own post; the switch it set saved nothing (the file held it already).
    assert drive(scenario) == (True, [True])


def test_reload_over_an_unreadable_config_says_so_as_it_shows_the_defaults() -> None:
    """#274 delta review 2, n2: every field, the Captain and ``[ui]`` reset to the defaults
    when the file cannot be read, so Reload says why rather than resetting silently."""
    settings_service.set_value("ui.always_show_project_path", "true")

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, list[tuple[str, str]]]:
        await pilot.pause()
        paths.config_path().write_text("[ui\nnot toml", encoding="utf-8")  # broken meanwhile
        await reopen(pilot, host.query_one(AppSettingsView))
        await settle(pilot)
        return host.query_one("#ui-always-show-path", Checkbox).value, host.notices

    shown, notices = drive(scenario)
    assert shown is False, "the defaults, as the first open shows them"
    [(said, severity)] = [notice for notice in notices if "unreadable" in notice[0]]
    assert severity == "warning"
    assert said.startswith("config unreadable — showing the defaults, nothing saved: ")
    assert paths.config_path().read_text(encoding="utf-8") == "[ui\nnot toml", (
        "the defaults shown are not written over the file"
    )


def test_reset_all_turns_always_show_project_directory_off() -> None:
    settings_service.set_value("ui.always_show_project_path", "true")

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[bool, bool, list[bool]]:
        await pilot.pause()
        before = host.query_one("#ui-always-show-path", Checkbox).value
        host.query_one("#reset-settings", Button).press()
        await settle(pilot)
        host.query_one("#reset-settings", Button).press()
        await settle(pilot)
        return before, host.query_one("#ui-always-show-path", Checkbox).value, host.ui_told

    before, after, told = drive(scenario)
    assert (before, after) == (True, False)
    assert told == [False], "the app is told the preference is off again"
    assert not paths.config_path().exists() and load_config().ui.always_show_project_path is False


# --- Confirmations: [ui] dont_ask and Ask again before every action (R17) -----------------


def _confirmations(host: AppHost) -> tuple[str, bool]:
    line = str(host.query_one(f"#{NOT_ASKING_LINE}", Static).render())
    return line, host.query_one(f"#{ASK_AGAIN_BUTTON}", Button).disabled


def test_confirmations_say_which_actions_no_longer_ask_and_the_reset_clears_every_kind() -> None:
    """The line names each kind ``dont_ask`` holds, a later release's too; *Ask again before
    every action* empties it at one press, with ``confirm_stop`` back on, and the app hears
    it. With every action asking, the line says so and the button has nothing to do."""
    settings_service.set_value("ui.dont_ask", "stop,pause,kind-from-a-later-release")

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, object]:
        await pilot.pause()
        seen: dict[str, object] = {"open": _confirmations(host)}
        host.query_one(f"#{ASK_AGAIN_BUTTON}", Button).press()
        await settle(pilot)
        seen["reset"] = _confirmations(host)
        seen["told"] = [(ui.dont_ask, ui.confirm_stop) for ui in host.uis]
        seen["notices"] = [said for said, _ in host.notices]
        return seen

    seen = drive(scenario)
    assert seen["open"] == ("Not asking before: Stop, Pause, Kind-from-a-later-release", False)
    assert seen["reset"] == ("Every action asks first", True)
    assert seen["told"] == [([], True)]
    assert "Every action asks first again." in seen["notices"]  # type: ignore[operator]
    assert load_config().ui.dont_ask == [] and load_config().ui.confirm_stop is True
    assert not paths.config_path().exists() or "ui" not in _config_toml()


def test_reload_and_reset_all_read_the_confirmations() -> None:
    """A ``config set`` from a shell shows after Reload; Reset all brings every box back."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> list[tuple[str, bool]]:
        await pilot.pause()
        settings_service.set_value("ui.confirm_stop", "false")  # from a shell, 0.10.0's key
        await reopen(pilot, host.query_one(AppSettingsView))
        await settle(pilot)
        reloaded = _confirmations(host)
        host.query_one("#reset-settings", Button).press()  # arms it
        await settle(pilot)
        host.query_one("#reset-settings", Button).press()  # the second press resets
        await settle(pilot)
        return [reloaded, _confirmations(host)]

    assert drive(scenario) == [
        ("Not asking before: Stop", False),
        ("Every action asks first", True),
    ]
    assert load_config().ui.confirm_stop is True


def test_a_save_made_elsewhere_moves_what_it_changed_and_no_other() -> None:
    """A box saves *Don't ask again* while App settings, kept alive once opened, shows every
    action asking: :meth:`AppSettingsView.follow_ui` shows the file's line, as read; an edit
    not yet saved on the form's tab stays, and is all a Save would write."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[tuple[str, bool], set[str]]:
        await pilot.pause()
        view = host.query_one(AppSettingsView)
        host.query_one("#max-agents", Input).value = "7"  # not saved yet
        ui = settings_service.stop_asking_first("pause")  # a box's save
        view.follow_ui(ui)
        return _confirmations(host), view._changed()

    shown, changed = drive(scenario)
    assert shown == ("Not asking before: Pause", False), "the line reads what the box saved"
    assert changed == {"max-agents"}, "the operator's own edit is kept, and only it"


#: Per tab, what shows as it opens (the owner's bare minimum, #41) and what its *Advanced*
#: folds — the list posted to the board before the fold was built.
IN_VIEW = {
    AGENT_TAB: ("#accounts-pick", "#accounts-on-limit", f"#{AGENT_ADVANCED}"),
    PREFERENCES_TAB: ("#updates-check", "#updates-check-about"),
    DISPLAY_TAB: (*(f"#{wid}" for wid in DISPLAY_CHOICES), "#ui-always-show-path-about"),
    REMOTE_TAB: ("#settings-remote-title", "#settings-remote-state", "#settings-remote-open"),
}
FOLDED = {
    AGENT_TAB: (
        "#role-perm-coder",
        "#role-acct-coder",
        "#escape-key",
        "#max-agents",
        "#worktree-dir",
        "#native-teams",
        "#accounts-switch-at",
        "#accounts-wait-minutes",
        "#accounts-cross-agent",
        "#accounts-share-trust",
        "#accounts-share-settings",
        "#agent-settings-note",
        "#bindings-new",
    ),
    PREFERENCES_TAB: (f"#{NOT_ASKING_LINE}", f"#{ASK_AGAIN_BUTTON}", "#preferences-from-a-shell"),
    DISPLAY_TAB: (),
    REMOTE_TAB: (),
}
ADVANCED = {
    AGENT_TAB: f"#{AGENT_ADVANCED}",
    PREFERENCES_TAB: f"#{PREFERENCES_ADVANCED}",
}
TABS_IN_ORDER = (AGENT_TAB, PREFERENCES_TAB, DISPLAY_TAB, REMOTE_TAB)


def test_each_tab_shows_the_bare_minimum_and_folds_the_rest_under_advanced() -> None:
    """The fold (#41): what a new user needs shows, the rest is one click on *Advanced* away;
    the Captain box is the last of *Agent settings*, under its *Advanced*, not atop it (Q18);
    Save, Reload and Reset all hide on *Remote*, whose controls act at once, and come back on
    the form's tabs."""

    def area(host: AppHost, wid: str) -> int:
        return host.query_one(wid).region.area

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, object]:
        tabs = host.query_one("#app-settings-tabs", TabbedContent)
        seen: dict[str, object] = {
            "agent tab": [child.id for child in host.query_one("#agent-settings").children],
        }
        for tab in TABS_IN_ORDER:
            tabs.active = tab
            await settle(pilot)
            seen[f"{tab} in view"] = [wid for wid in IN_VIEW[tab] if area(host, wid) == 0]
            seen[f"{tab} folded"] = [wid for wid in FOLDED[tab] if area(host, wid) > 0]
            seen[f"{tab} buttons"] = area(host, "#reset-settings") > 0
            if tab not in ADVANCED:  # Display and Remote have no fold: all in view
                seen[f"{tab} unfolded"] = []
                continue
            host.query_one(ADVANCED[tab], Collapsible).collapsed = False
            await settle(pilot)
            hidden: list[str] = []
            for wid in FOLDED[tab]:
                host.query_one(wid).scroll_visible(animate=False)
                await settle(pilot)
                if area(host, wid) == 0:
                    hidden.append(wid)
            seen[f"{tab} unfolded"] = hidden
        tabs.active = AGENT_TAB
        await settle(pilot)
        seen["buttons back"] = area(host, "#reset-settings") > 0
        return seen

    seen = drive(scenario)
    agent_tab = cast(list[str], seen["agent tab"])
    assert agent_tab[-1] == "settings-experimental", agent_tab
    assert agent_tab.index(AGENT_ADVANCED) < agent_tab.index("settings-experimental"), "under it"
    for tab in TABS_IN_ORDER:
        assert seen[f"{tab} in view"] == [], f"{tab}: shown as it opens"
        assert seen[f"{tab} folded"] == [], f"{tab}: folded until Advanced is opened"
        assert seen[f"{tab} unfolded"] == [], f"{tab}: shown once Advanced is opened"
    assert (seen[f"{AGENT_TAB} buttons"], seen[f"{PREFERENCES_TAB} buttons"]) == (True, True)
    assert seen[f"{REMOTE_TAB} buttons"] is False, "nothing on Remote for Save to save"
    assert seen[f"{DISPLAY_TAB} buttons"] is False, "nor on Display"
    assert seen["buttons back"] is True


# --- M5: one save model ------------------------------------------------------------------


def test_each_control_is_kept_as_it_changes_and_ctrl_z_puts_the_file_back() -> None:
    """M5: no Save. A pick, a switch, a text field (on Enter) and a *Display* box are each on
    disk as they change, each said with its Undo key; ctrl+z on the page takes them back, the
    last first, on disk and on screen. (The Captain box's: tests/test_ui_settings_captain.py.)"""

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, Any]:
        await settle(pilot)
        tabs = host.query_one("#app-settings-tabs", TabbedContent)
        seen: dict[str, Any] = {}
        pick = host.query_one("#accounts-pick", Select)
        pick.focus()
        pick.value = "default"
        await settle(pilot)
        seen["pick"] = load_config().accounts.pick
        host.query_one(f"#{AGENT_ADVANCED}", Collapsible).collapsed = False
        await settle(pilot)
        escape = host.query_one("#escape-key", Input)
        escape.focus()
        await settle(pilot)
        escape.value = "f11"
        await pilot.press("enter")
        await settle(pilot)
        seen["escape"] = load_config().fleet.escape_key
        tabs.active = PREFERENCES_TAB
        await settle(pilot)
        check = host.query_one(f"#{UPDATES_CHECK}", Switch)
        check.focus()
        check.value = False
        await settle(pilot)
        seen["check"] = load_config().updates.check
        tabs.active = DISPLAY_TAB
        await settle(pilot)
        usage = host.query_one("#ui-show-usage", Checkbox)
        usage.focus()
        usage.value = False
        await settle(pilot)
        seen["usage"] = load_config().ui.show_usage
        seen["saved"] = [m for m, _ in host.notices if m.startswith("Saved ")]
        undone: list[tuple[str, object]] = []
        reads: list[Callable[[AppConfig], object]] = [
            lambda c: c.ui.show_usage,
            lambda c: c.updates.check,
            lambda c: c.fleet.escape_key,
            lambda c: c.accounts.pick,
        ]
        for read in reads:
            await pilot.press("ctrl+z")
            await settle(pilot)
            undone.append((host.notices[-1][0], read(load_config())))
        seen["undone"] = undone
        seen["shown"] = (usage.value, check.value, escape.value, pick.value)
        return seen

    seen = drive(scenario)
    assert (seen["pick"], seen["escape"]) == ("default", "f11")
    assert (seen["check"], seen["usage"]) == (False, False), "each on disk, no Save pressed"
    assert seen["saved"] == [
        "Saved how launches pick · Undo: ctrl+z",
        "Saved the escape key · Undo: ctrl+z",
        "Saved the daily release check: off · Undo: ctrl+z",
        f"Saved {display_label('ui-show-usage')}: off · Undo: ctrl+z",
    ]
    assert [value for _, value in seen["undone"]] == [True, True, "f12", "headroom"]
    assert seen["undone"][0][0] == f"Undone: {display_label('ui-show-usage')}: off"
    assert seen["undone"][-1][0] == "Undone: how launches pick"
    assert seen["shown"] == (True, True, "f12", "headroom"), "and shown as it was"
    assert overrides(load_config()) == {}, "every value at its default again"


def test_a_change_waiting_its_turn_survives_the_re_read_another_save_makes() -> None:
    """Two changes before either is handled: the first one's save re-reads the form, which
    leaves the second showing what the person chose, and then keeps it too."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[str, str]:
        await settle(pilot)
        host.query_one("#worktree-dir", Input).value = ".trees"
        leave_fields(host)  # its save comes first…
        host.query_one("#accounts-on-limit", Select).value = "wait"  # …and re-reads this
        await settle(pilot)
        config = load_config()
        return config.fleet.worktree_dir, config.accounts.on_limit

    assert drive(scenario) == (".trees", "wait")


def test_opening_the_page_again_shows_what_a_shell_wrote_meanwhile() -> None:
    """There is no Reload (M5): the page re-reads the file each time it is opened again."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[str, str]:
        await settle(pilot)
        field = host.query_one("#escape-key", Input)
        before = field.value
        settings_service.set_value("fleet.escape_key", "f10")  # from a shell
        await reopen(pilot, host.query_one(AppSettingsView))
        return before, field.value

    assert drive(scenario) == ("f12", "f10")


def test_reset_all_is_taken_back_by_ctrl_z() -> None:
    config = load_config()
    config.fleet.escape_key = "f9"
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[str, str]:
        await settle(pilot)
        reset = host.query_one("#reset-settings", Button)
        reset.focus()
        reset.press()  # arms it
        await settle(pilot)
        reset.press()  # resets
        await settle(pilot)
        after_reset = load_config().fleet.escape_key
        await pilot.press("ctrl+z")
        await settle(pilot)
        return after_reset, load_config().fleet.escape_key

    assert drive(scenario) == ("f12", "f9")


def test_the_page_holds_no_save_reload_apply_or_cancel_button() -> None:
    async def scenario(pilot: Pilot[None], host: AppHost) -> list[str]:
        await settle(pilot)
        return [
            str(button.label)
            for button in host.query_one(AppSettingsView).query(Button)
            if str(button.label).lower().startswith(("save", "reload", "apply", "cancel"))
        ]

    assert drive(scenario) == []


def test_a_field_whose_config_write_failed_is_written_again_on_enter(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """#53: the config could not be written (the filesystem said no), so the text field keeps
    what was typed to try again (``_show_file_value``), and Enter with it unchanged writes again:
    the value in flight is released."""
    from aisquare.cli.ui.views import app_settings as settings_view

    real_save = save_config  # the one writer, which the view calls by this name
    calls: list[str] = []

    def once_refused(config: AppConfig) -> None:
        calls.append(config.fleet.escape_key)
        if len(calls) == 1:
            raise OSError("read-only file system")
        real_save(config)

    monkeypatch.setattr(settings_view, "save_config", once_refused)

    async def scenario(pilot: Pilot[None], host: AppHost) -> tuple[list[str], str, bool]:
        await settle(pilot)
        host.query_one(f"#{AGENT_ADVANCED}", Collapsible).collapsed = False
        await settle(pilot)
        escape = host.query_one("#escape-key", Input)
        escape.focus()
        await settle(pilot)
        escape.value = "f11"
        await pilot.press("enter")
        await settle(pilot)
        failed = any("could not write the config" in m for m, _ in host.notices)
        await pilot.press("enter")  # the same value, again
        await settle(pilot)
        return list(calls), load_config().fleet.escape_key, failed

    calls_seen, on_disk, failed = drive(scenario)
    assert failed, "the premise: the first write failed"
    assert calls_seen == ["f11", "f11"] and on_disk == "f11"


def test_a_field_typed_and_not_left_is_kept_when_rlx_quits() -> None:
    """Reviewer of #24: on quit, the field's last message (its blur) posted a Committed into
    ancestors whose loops had ended, and the typed value was lost. It is written as rlx goes;
    one the form refuses is not."""

    async def typed_then_quit(pilot: Pilot[None], host: AppHost) -> None:
        await settle(pilot)
        host.query_one(f"#{AGENT_ADVANCED}", Collapsible).collapsed = False
        await settle(pilot)
        host.query_one("#escape-key", Input).value = "f9"
        leave_fields(host)  # kept the usual way first
        await settle(pilot)
        field = host.query_one("#max-agents", Input)
        field.focus()
        await settle(pilot)
        await pilot.press("end", *["backspace"] * 4, "4")
        await pilot.pause()
        host.exit()  # no Enter, the field never left

    drive(typed_then_quit)
    config = load_config()
    assert (config.fleet.max_agents_per_project, config.fleet.escape_key) == (4, "f9")

    async def refused_then_quit(pilot: Pilot[None], host: AppHost) -> None:
        await settle(pilot)
        field = host.query_one("#accounts-switch-at", Input)
        field.focus()
        await settle(pilot)
        field.value = "250"
        host.exit()

    drive(refused_then_quit)
    assert load_config().accounts.switch_at == 85, "a value the form refuses is not written"


def test_reset_all_undone_puts_back_only_what_it_changed() -> None:
    """Reviewer of #24: Reset all's ctrl+z wrote the whole pre-reset file back, which undid a
    + New binding or a shell's config set made after the reset. Each setting it changed comes
    back, unless written since."""
    config = load_config()
    config.fleet.escape_key = "f9"
    config.accounts.switch_at = 70
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> None:
        await settle(pilot)
        reset = host.query_one("#reset-settings", Button)
        reset.focus()
        reset.press()
        await settle(pilot)
        reset.press()
        await settle(pilot)
        settings_service.set_value("accounts.switch_at", "60")  # a shell, after the reset…
        settings_service.bind_role("designer", env={"GH_TOKEN": "x"})  # …and a + New binding
        await pilot.press("ctrl+z")
        await settle(pilot)

    drive(scenario)
    after = load_config()
    assert after.fleet.escape_key == "f9", "what the reset changed, back"
    assert after.accounts.switch_at == 60, "written since the reset: it stands, not 70"
    assert "designer" in after.team.profiles, "the binding made since stands"


def test_an_undo_that_fails_says_so_and_keeps_the_change_to_try_again() -> None:
    """Reviewer of #24: "Undone" was said, and the change dropped, when the undo failed."""

    async def scenario(pilot: Pilot[None], host: AppHost) -> dict[str, Any]:
        await settle(pilot)
        check = host.query_one(f"#{UPDATES_CHECK}", Switch)
        check.focus()
        check.value = False
        await settle(pilot)
        good = paths.config_path().read_text(encoding="utf-8")
        paths.config_path().write_text("[updates\nnot toml", encoding="utf-8")  # broken meanwhile
        await pilot.press("ctrl+z")
        await settle(pilot)
        seen: dict[str, Any] = {"failed": [m for m, _ in host.notices][-2:]}
        paths.config_path().write_text(good, encoding="utf-8")
        await pilot.press("ctrl+z")
        await settle(pilot)
        seen["retried"] = (host.notices[-1][0], load_config().updates.check, check.value)
        return seen

    seen = drive(scenario)
    assert seen["failed"][-1] == "Not undone: the daily release check: off · ctrl+z tries again"
    assert not any(m.startswith("Undone") for m in seen["failed"])
    assert seen["retried"] == ("Undone: the daily release check: off", True, True)


def test_reset_all_undone_puts_a_nested_binding_back_whole() -> None:
    """Reviewer of #24, round 3: the reset's emptied ``team.profiles`` was a setting of its
    own, put back (as nothing) after the bindings in it, in a set's hash order: the coder's
    binding came back with no agent and no environment under some PYTHONHASHSEEDs."""
    from aisquare.core.config import RoleLaunchProfile

    config = load_config()
    config.team.profiles["coder"] = RoleLaunchProfile(agent="codex", env={"A": "1"})
    save_config(config)

    async def scenario(pilot: Pilot[None], host: AppHost) -> bool:
        await settle(pilot)
        reset = host.query_one("#reset-settings", Button)
        reset.focus()
        reset.press()
        await settle(pilot)
        reset.press()
        await settle(pilot)
        emptied = "coder" not in load_config().team.profiles
        await pilot.press("ctrl+z")
        await settle(pilot)
        return emptied

    assert drive(scenario), "the premise: the reset took the binding away"
    coder = load_config().team.profiles.get("coder")
    assert coder is not None, "ctrl+z: the binding back"
    assert (coder.agent, coder.env) == ("codex", {"A": "1"}), "whole: its agent and its env"


def test_a_refusal_line_goes_under_its_row_and_leaves_the_label_whole_at_80x24() -> None:
    """Tester of #24, m1: the line was mounted in the field's row, beside it; at 80x24 the
    label ("max agents per project") was drawn a letter a line and the line cut to two words.
    It goes under the row."""

    async def run() -> dict[str, Any]:
        host = AppHost()
        async with host.run_test(size=(80, 24)) as pilot:
            await settle(pilot)
            host.query_one(f"#{AGENT_ADVANCED}", Collapsible).collapsed = False
            await settle(pilot)
            field = host.query_one("#max-agents", Input)
            row = field.parent
            assert isinstance(row, Widget)
            label = row.query_one(Label)
            field.scroll_visible(animate=False, immediate=True)
            field.focus()
            await settle(pilot)
            before = label.region.height
            field.value = "0"
            await pilot.press("enter")
            await settle(pilot)
            line = host.query_one("#max-agents-problem", Static)
            line.scroll_visible(animate=False, immediate=True)
            await settle(pilot)
            return {
                "in the row": line.parent is row,
                "label rows": (before, label.region.height),
                "line": host.query_one(AppSettingsView).problem(field),
                "line width": line.region.width,
                "row width": row.region.width,
            }

    seen = asyncio.run(run())
    assert seen["in the row"] is False, "under the row, not beside the field"
    before, after = seen["label rows"]
    assert after == before, "the label's rows as they were: not squeezed by the line"
    assert seen["line"].startswith("not saved: ") and "at least 1" in seen["line"], seen
    assert seen["line width"] >= seen["row width"] - 2, "as wide as the row it is under"
