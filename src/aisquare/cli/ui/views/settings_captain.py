"""Settings' *Experimental* (#46): the captain and Remote, each switched on and off while rlx
runs, both off as they ship (:class:`ExperimentalSection`, last in App settings' *Agent
settings*). It replaced the *Captain* section (#33), which follows as it was, under the one
heading.

*Remote*: a box that saves ``[experimental] remote`` (:class:`RemoteToggle`) and posts
:class:`RemoteSwitched`; the shell brings Remote up or down at once
(``FleetApp.on_remote_switched``). Off, nothing of Remote loads; what it loaded already stays
until rlx's next start, which its line says. ``AISQUARE_EXPERIMENTAL_REMOTE`` wins, as the
captain's variable does.

The captain's part, as #33 made it:

One box, *Captain (experimental)*. A tick saves ``[experimental] captain`` through
``core.config.save_config``, the one writer, which is what ``aisquare config set
experimental.captain true`` writes, and posts :class:`CaptainSwitched`. The shell then
shows or hides *Start captain* (the captain's row while it runs) and starts or stops the
captain's part of its fleet read and its ui receiver, at once
(``FleetApp.on_captain_switched``). It took ``config set`` and a restart, which the owner
could not find.

``AISQUARE_EXPERIMENTAL_CAPTAIN`` wins over the config (``core.experimental``). While it is
set, the box shows the state it forces, greyed, and one line says why: a tick would save
a setting the variable overrides.

It ends App settings' *Agent settings* tab, under its *Advanced* (Q18: it topped the tab,
#41), in that form's format: a section heading, the box as a row, a dim help line, styled by
the form's own ``.section``/``.row``/``.about`` rules. It saves at once, as it did, not with
the form's *Save*; the form's Reload and Reset all re-read the file under it
(:meth:`CaptainToggle.reread`), so the box and the shell follow what the file holds.

Under the box, *captain's agent*: the coding agent the captain runs on, ``[team.profiles.
captain] agent`` (what ``aisquare team bind captain --agent`` writes), every agent the
role rows offer, *Use default* clearing it (SCOPE #113 274, L3: the captain on Codex was
a TOML edit). It saves at once too, and reaches the captain at its next start or restart.
"""

from __future__ import annotations

from rich.text import Text
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.message import Message
from textual.widgets import Checkbox, Label, Select, Static

from aisquare.cli.ui.save_model import page_of
from aisquare.core import experimental
from aisquare.core.config import AppConfig, RoleLaunchProfile, load_config, save_config

ABOUT = (
    "the home-level agent that unblocks your agents: Start captain at the top of the sidebar, "
    "and its row there while it runs"
)
"""The line under the box while the config decides."""
TITLE = ("Experimental", "features still being tried: each is off until you turn it on")
"""The section heading: its name, and what it is, dim (``app_settings.section_title``)."""
REMOTE_ABOUT = (
    "the fleet on your phone: a link, its QR code and a password. Off frees its memory at "
    "rlx's next start"
)
"""The line under Remote's box while the config (or the home, undecided) decides."""
CAPTAIN = "captain"
"""The captain's role, whose binding names its agent (``services.fleet.CAPTAIN_ROLE``)."""


def captain_agent(config: AppConfig) -> str:
    """The agent ``config`` binds the captain to, ``""`` for none (the user's coding agent)."""
    return config.team.profiles.get(CAPTAIN, RoleLaunchProfile()).agent or ""


def forced_text(
    on: bool, variable: str = experimental.CAPTAIN_ENV, what: str = "the captain"
) -> str:
    """The line under a box while its variable (the captain's by default) decides."""
    state = "on" if on else "off"
    return f"{variable} is set where rlx runs and keeps {what} {state}: it wins over this setting"


class CaptainSwitched(Message):
    """The *Captain* box saved a new state: the shell shows it at once (``FleetApp``)."""

    def __init__(self, on: bool) -> None:
        self.on = on
        super().__init__()


class RemoteSwitched(Message):
    """The *Remote* box saved a new state: the shell brings Remote up or down at once."""

    def __init__(self, on: bool) -> None:
        self.on = on
        super().__init__()


class ExperimentalSection(Vertical):
    """*Experimental*: one heading, then the captain's box with its agent under it, then
    Remote's box, each with its dim line, in the form's own format (no border of its own)."""

    DEFAULT_CSS = """
    ExperimentalSection { height: auto; }
    """

    def compose(self) -> ComposeResult:
        from aisquare.cli.ui.views.app_settings import section_title

        yield Static(section_title(*TITLE), id="experimental-title", classes="section")
        yield CaptainToggle(id="settings-captain")
        yield RemoteToggle(id="settings-remote-switch")

    def reread(self, config: AppConfig) -> None:
        """The form re-read the file: each box shows what it holds."""
        self.query_one(CaptainToggle).reread(config)
        self.query_one(RemoteToggle).reread(config)


class CaptainToggle(Vertical):
    """*Captain (experimental)*: a box that saves the setting and posts :class:`CaptainSwitched`,
    and the select of the agent it runs on. Each is kept as it changes, and its page's ctrl+z
    takes it back (M5, ``cli.ui.save_model``)."""

    _agent = ""
    """The captain's agent as the file holds it: what ctrl+z puts back after a pick."""

    DEFAULT_CSS = """
    CaptainToggle { height: auto; }
    CaptainToggle #captain-agent-label { padding-top: 0; }
    """

    def compose(self) -> ComposeResult:
        forced = experimental.captain_forced()
        # What the shell shows now, which a config set from a shell meanwhile has not moved.
        shown = bool(getattr(self.app, "captain_on", experimental.captain_enabled()))
        with Horizontal(classes="row preference"):
            yield Checkbox(
                "Captain",
                value=shown if forced is None else forced,
                disabled=forced is not None,
                id="captain-toggle",
                compact=True,
            )
        about = ABOUT if forced is None else forced_text(forced)
        yield Static(Text(about), id="captain-toggle-about", classes="about")
        yield from self._compose_agent()

    def _compose_agent(self) -> ComposeResult:
        from aisquare.cli.ui.views.app_settings import agent_options

        try:
            current = captain_agent(load_config())
        except Exception:  # a broken config.toml: the form says so; the select offers default
            current = ""
        self._agent = current
        # One line, a compact select under the box: App settings opens on the box and the first
        # of how launches pick, whole, at 80x24 (#41).
        with Horizontal(classes="row preference"):
            yield Label("captain's agent", id="captain-agent-label")
            yield Select(
                agent_options(current),
                value=current,
                allow_blank=False,
                id="captain-agent",
                compact=True,
            )

    def reread(self, config: AppConfig) -> None:
        """The form re-read the file (Reload, Save, Reset all): the box shows what it holds,
        and the shell follows when that moved the switch — Reset all turned a ticked captain
        off in the file while Start captain and the captain's row stayed until a restart. While
        the variable decides, the box keeps the state it forces."""
        from aisquare.cli.ui.views.app_settings import agent_options

        agent = self.query_one("#captain-agent", Select)
        self._agent = captain_agent(config)
        with self.prevent(Select.Changed):
            agent.set_options(agent_options(self._agent))
            agent.value = self._agent
        if experimental.captain_forced() is not None:
            return
        box = self.query_one("#captain-toggle", Checkbox)
        with self.prevent(Checkbox.Changed):
            box.value = config.experimental.captain
        if config.experimental.captain != bool(getattr(self.app, "captain_on", box.value)):
            # To the app itself, not bubbled: Reset all re-reads from its dialog's callback,
            # which runs as the app, and Textual never bubbles a message up to its sender.
            self.app.post_message(CaptainSwitched(config.experimental.captain))

    def on_checkbox_changed(self, event: Checkbox.Changed) -> None:
        if event.checkbox.id != "captain-toggle":
            return
        event.stop()
        if not self._switch(event.value):
            return
        page = page_of(self)
        if page is not None:
            back = not event.value
            page.kept(f"the captain: {'on' if event.value else 'off'}", lambda: self._switch(back))

    def _switch(self, on: bool) -> bool:
        """Save the captain ``on`` (or off), shown on the box, and tell the shell; said, and the
        box put back, when the config would not take it."""
        box = self.query_one("#captain-toggle", Checkbox)
        problem = self._save(on)
        if problem is not None:
            self.notify(problem, severity="error", timeout=8, markup=False)
            with self.prevent(Checkbox.Changed):
                box.value = not on
            return False
        if box.value != on:
            with self.prevent(Checkbox.Changed):
                box.value = on
        self.post_message(CaptainSwitched(experimental.captain_enabled()))
        return True

    def on_select_changed(self, event: Select.Changed) -> None:
        if event.select.id != "captain-agent":
            return
        event.stop()
        value = event.value if isinstance(event.value, str) else ""
        before = self._agent
        problem = self._save_agent(value)
        # "" is nothing written: the file holds it already, as at the select's first Changed.
        if problem:
            self.notify(problem, severity="error", timeout=8, markup=False)
            return
        if problem is None:
            self._agent = value
            page = page_of(self)
            if page is not None:
                page.kept("the captain's agent", lambda: self._put_agent(before))

    def _put_agent(self, agent: str) -> bool:
        """Undo of a pick: the captain's agent the file held before, shown and saved; whether
        it was."""
        select = self.query_one("#captain-agent", Select)
        problem = self._save_agent(agent)
        if problem:
            self.notify(problem, severity="error", timeout=8, markup=False)
            return False
        with self.prevent(Select.Changed):
            select.value = agent
        self._agent = agent
        return True

    @staticmethod
    def _save_agent(agent: str) -> str | None:
        """Write ``[team.profiles.captain] agent`` (cleared for ``""``, and a profile left
        holding nothing goes, as a role row's does); why not, when the config would not take
        it, and ``""`` when the file holds ``agent`` already, so nothing is written."""
        try:
            config = load_config()
        except Exception as exc:  # a broken config.toml: say so, change nothing
            return f"config unreadable — the captain's agent is left as it was: {exc}"
        if captain_agent(config) == agent:
            return ""
        profile = config.team.profiles.get(CAPTAIN)
        if agent:
            config.team.profiles[CAPTAIN] = (profile or RoleLaunchProfile()).model_copy(
                update={"agent": agent}
            )
        elif profile is not None:
            profile.agent = None
            if profile.account is None and not profile.carries_launch:
                config.team.profiles.pop(CAPTAIN, None)
        try:
            save_config(config)
        except OSError as exc:  # the operator's filesystem saying no
            return f"could not write the config — the captain's agent is left as it was: {exc}"
        return None

    @staticmethod
    def _save(on: bool) -> str | None:
        """Write ``[experimental] captain``; why not, when the config would not take it."""
        try:
            config = load_config()
        except Exception as exc:  # a broken config.toml: say so, change nothing
            return f"config unreadable — the captain is left as it was: {exc}"
        config.experimental.captain = on
        try:
            save_config(config)
        except OSError as exc:  # the operator's filesystem saying no
            return f"could not write the config — the captain is left as it was: {exc}"
        return None


class RemoteToggle(Vertical):
    """*Remote*: a box that saves ``[experimental] remote`` and posts :class:`RemoteSwitched`.
    Kept as it changes, and its page's ctrl+z takes it back (M5, ``cli.ui.save_model``).

    It shows Remote as it reads: undecided, on for a home where Remote was set up before it had
    a switch (``core.experimental.remote_set_up``). A tick writes the choice, ``true`` or
    ``false``, which a reset keeps.
    """

    DEFAULT_CSS = """
    RemoteToggle { height: auto; }
    """

    def compose(self) -> ComposeResult:
        forced = experimental.remote_forced()
        shown = bool(getattr(self.app, "remote_on", experimental.remote_enabled()))
        with Horizontal(classes="row preference"):
            yield Checkbox(
                "Remote",
                value=shown if forced is None else forced,
                disabled=forced is not None,
                id="remote-toggle",
                compact=True,
            )
        about = (
            REMOTE_ABOUT
            if forced is None
            else forced_text(forced, experimental.REMOTE_ENV, "Remote")
        )
        yield Static(Text(about), id="remote-toggle-about", classes="about")

    def reread(self, config: AppConfig) -> None:
        """The form re-read the file (Reset all among them): the box shows Remote as it now
        reads, and the shell follows when that moved it. While the variable decides, the box
        keeps the state it forces."""
        if experimental.remote_forced() is not None:
            return
        on = (
            experimental.remote_set_up()
            if config.experimental.remote is None
            else bool(config.experimental.remote)
        )
        box = self.query_one("#remote-toggle", Checkbox)
        with self.prevent(Checkbox.Changed):
            box.value = on
        if on != bool(getattr(self.app, "remote_on", box.value)):
            # To the app itself, not bubbled: Reset all re-reads from its dialog's callback,
            # which runs as the app, and Textual never bubbles a message up to its sender.
            self.app.post_message(RemoteSwitched(on))

    def on_checkbox_changed(self, event: Checkbox.Changed) -> None:
        if event.checkbox.id != "remote-toggle":
            return
        event.stop()
        if not self._switch(event.value):
            return
        page = page_of(self)
        if page is not None:
            back = not event.value
            page.kept(f"Remote: {'on' if event.value else 'off'}", lambda: self._switch(back))

    def _switch(self, on: bool) -> bool:
        """Save Remote ``on`` (or off), shown on the box, and tell the shell; said, and the
        box put back, when the config would not take it."""
        box = self.query_one("#remote-toggle", Checkbox)
        problem = self._save(on)
        if problem is not None:
            self.notify(problem, severity="error", timeout=8, markup=False)
            with self.prevent(Checkbox.Changed):
                box.value = not on
            return False
        if box.value != on:
            with self.prevent(Checkbox.Changed):
                box.value = on
        self.post_message(RemoteSwitched(experimental.remote_enabled()))
        return True

    @staticmethod
    def _save(on: bool) -> str | None:
        """Write ``[experimental] remote``; why not, when the config would not take it."""
        try:
            config = load_config()
        except Exception as exc:  # a broken config.toml: say so, change nothing
            return f"config unreadable — Remote is left as it was: {exc}"
        config.experimental.remote = on
        try:
            save_config(config)
        except OSError as exc:  # the operator's filesystem saying no
            return f"could not write the config — Remote is left as it was: {exc}"
        return None
