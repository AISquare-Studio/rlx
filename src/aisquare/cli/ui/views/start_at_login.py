"""App settings ▸ User preferences ▸ *Start rlx at login* (S2, #33): the rlx service's login
item, added and taken away as the switch moves, and ctrl+z takes it back (M5,
``cli.ui.save_model``).

Not a ``config.toml`` setting: what it shows is the OS's, read in a thread (``launchctl`` and
``systemctl`` are processes), each time the page is shown, so a removal in System Settings' Login
Items reads as off. What it writes goes through ``services.service_login``, as
``rlx service install`` and ``uninstall`` do. What it says names no command (0.10.1's R13).
"""

from __future__ import annotations

from rich.text import Text
from textual import on
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.widgets import Label, Static, Switch
from textual.worker import Worker, WorkerState

from aisquare.cli.ui.save_model import page_of
from aisquare.core.version import PRODUCT_NAME
from aisquare.services import service_login

SWITCH = "start-at-login"
ABOUT = "start-at-login-about"
WORKER = "settings-start-at-login"
"""The group of its workers: reads, and the writes the switch asks for."""
READ_WORKER = "settings-start-at-login-read"
WRITE_WORKER = "settings-start-at-login-write"
"""Their names: a read Update and Uninstall wait out, and a write they refuse under
(``ui.app._waited_reads``, ``_writing_workers``)."""

TITLE = ("Login", f"whether {PRODUCT_NAME} starts when you log in")
LABEL = f"Start {PRODUCT_NAME} at login"
READING = f"Reading whether {PRODUCT_NAME} starts at login…"
ON = (
    f"On: the {PRODUCT_NAME} service starts each time you log in, not at boot, so Remote and the "
    "usage reads go on with no window open."
)
TURNED_ON = f"{PRODUCT_NAME} starts at login"
TURNED_OFF = f"{PRODUCT_NAME} no longer starts at login; the service running now keeps going"


def off_line(why: str | None) -> str:
    return f"Off: {why}." if why else "Off."


class StartAtLogin(Vertical):
    """The section: its title, the switch, and the line that says how it stands."""

    DEFAULT_CSS = """
    StartAtLogin { height: auto; }
    """

    def __init__(self, *, id: str | None = None) -> None:
        super().__init__(id=id)
        self._state: service_login.AtLogin | None = None

    def compose(self) -> ComposeResult:
        # Here, not at the top: App settings, which holds the form's format, mounts this.
        from aisquare.cli.ui.views.app_settings import section_title

        yield Static(section_title(*TITLE), classes="section")
        with Horizontal(classes="row preference"):
            yield Label(LABEL, classes="preference-label")
            yield Switch(False, id=SWITCH, disabled=True)
        yield Static(Text(READING), id=ABOUT, classes="about")

    # --- reading -----------------------------------------------------------------------------

    def on_show(self) -> None:
        self.read_again()

    def read_again(self) -> None:
        """How it stands, read off the UI thread; painted when it answers."""
        self.run_worker(
            service_login.at_login_state,
            name=READ_WORKER,
            group=WORKER,
            thread=True,
            exit_on_error=False,
        )

    def _paint(self, state: service_login.AtLogin, *, said: str | None = None) -> None:
        self._state = state
        switch = self.query_one(f"#{SWITCH}", Switch)
        with switch.prevent(Switch.Changed):
            switch.value = state.on
        switch.disabled = not state.can
        line = ON if state.on else off_line(state.why_off)
        self.query_one(f"#{ABOUT}", Static).update(Text(said or line))

    # --- writing -----------------------------------------------------------------------------

    @on(Switch.Changed, f"#{SWITCH}")
    def _switch_moved(self, event: Switch.Changed) -> None:
        # Its own: the page keeps config.toml's switches, and this is none of them.
        event.stop()
        self._start_at_login_set(event.value, undo=False)

    def _start_at_login_set(self, on: bool, *, undo: bool) -> None:
        def start_at_login_written() -> tuple[bool, bool, service_login.AtLogin | str]:
            try:
                if on:
                    return on, undo, service_login.register_at_login().state
                return on, undo, service_login.unregister_at_login()
            except service_login.LoginError as exc:
                return on, undo, str(exc)
            except OSError as exc:  # config.toml that cannot be written: said, as a field's is
                return on, undo, f"could not save the choice: {exc.strerror or exc}"

        self.run_worker(
            start_at_login_written,
            name=WRITE_WORKER,
            group=WORKER,
            thread=True,
            exit_on_error=False,
        )

    def _start_at_login_take_back(self, on: bool) -> bool:
        """ctrl+z: the switch back where it was, on screen now and in the OS as the worker
        answers."""
        switch = self.query_one(f"#{SWITCH}", Switch)
        with switch.prevent(Switch.Changed):
            switch.value = on
        self._start_at_login_set(on, undo=True)
        return True

    @on(Worker.StateChanged)
    def _answered(self, event: Worker.StateChanged) -> None:
        worker = event.worker
        if worker.group != WORKER:
            return
        event.stop()
        if event.state is WorkerState.ERROR:
            self._paint_failed(str(worker.error))
            return
        if event.state is not WorkerState.SUCCESS:
            return
        result = worker.result
        if isinstance(result, service_login.AtLogin):
            self._paint(result)
            return
        if not isinstance(result, tuple):
            return
        on, undo, outcome = result
        page = page_of(self)
        if isinstance(outcome, str):
            self._paint_failed(outcome)
            return
        self._paint(outcome)
        if page is None:
            return
        page.accept(self.query_one(f"#{SWITCH}", Switch))
        if undo:
            return
        page.kept(
            LABEL,
            lambda: self._start_at_login_take_back(not on),
            said=TURNED_ON if on else TURNED_OFF,
        )

    def _paint_failed(self, why: str) -> None:
        """A change the OS did not take: the switch shows what is, and the line says why."""
        switch = self.query_one(f"#{SWITCH}", Switch)
        state = self._state
        with switch.prevent(Switch.Changed):
            switch.value = bool(state and state.on)
        page = page_of(self)
        if page is not None:
            page.refuse(switch, f"Not changed: {why}")
        else:
            self.query_one(f"#{ABOUT}", Static).update(Text(f"Not changed: {why}"))
