"""``RemotePanel`` — the one Remote page: Remote on/off, ngrok's token, password, link + QR,
devices.

PLAN §5 step 1: "press ``m`` → Remote on → modal shows link, QR, password" (``R``
now: the sidebar took ``m`` for marking cards). The
dialog is a view over the app's one :class:`RemoteController` (see
``remote_control.py``): every control calls the controller and then repaints
from it, and a one-second tick repaints too, because the public URL arrives
from ngrok's log on a background thread. Opened like the theme picker
(``push_screen``), closed with Esc.

Remote in one place (0.10.2, Q17): Remote lived here, in App settings' *Remote* tab and on
the Settings hub's *Remote control* row, two pages over one controller that each had what
the other lacked. This one stays, and every way in opens it (:func:`open_remote_page`):
``R``, the palette, the sidebar's chip, the hub's row and App settings' *Remote* tab. What
only the Settings page had came here: the ngrok token's slot and the missing-ngrok line
(``settings_remote``), looked at on a worker, and *Change* for the token. Not its copy of
ngrok's link at each turn-on: here the link, its QR and *Copy* are on screen, and a copy the
human did not ask for put Remote's link on the clipboard.

*New link* and *Regenerate* ask first (:class:`RemoteConfirm`): each signs every device out,
and New link also stops every phone's saved link from opening. The box guards a misclick: its
Cancel has the focus, and it opens in the half of the screen away from the button pressed, so
neither a second Enter nor a double-click's second click is a yes.

The QR is segno's compact terminal rendering (half-block characters, ~18 rows
for an ngrok URL) of exactly the text in the link row — one string feeds both,
so what the phone scans is what the human reads — in colours of its own, never
the theme's (:data:`QR_COLOURS`). Only of ngrok's link: the local one, all the panel
has while ngrok is missing, starting or restarting, leads a phone to its own loopback.
"""

from __future__ import annotations

import io
from datetime import UTC, datetime, tzinfo
from typing import Any, ClassVar

from rich.text import Text
from textual import on
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.geometry import Region
from textual.screen import ModalScreen
from textual.widgets import Button, DataTable, Input, Label, Select, Static, Switch
from textual.widgets.data_table import ColumnKey
from textual.worker import Worker

from aisquare.cli.ui.clipboard import copy_and_tell
from aisquare.cli.ui.remote_control import AUTO_OFF_CHOICES, RemoteController
from aisquare.cli.ui.save_model import UNDO_BINDING, SavesAsYouGo
from aisquare.cli.ui.ticks import while_running
from aisquare.cli.ui.views import settings_remote
from aisquare.cli.ui.views.settings_remote import (
    CHECK_WORKER,
    TOKEN_TROUBLE,
    NgrokLookedFor,
    NgrokMissing,
    TokenSaved,
    TokenSlot,
)
from aisquare.services.ngrok_sdk import BAD_TOKEN
from aisquare.services.remote_server import _remote_instant

QR_UNAVAILABLE = "QR unavailable: this install lacks segno"
"""The QR's notice, before the command that puts segno back in the install that runs
(``remote_install_hint``). It said ``pip install segno``, which the install the docs give, a
uv tool, cannot follow: its environment has no pip (review of #259, round 39, 8/9)."""
LOCAL_ONLY = (
    "this machine only: a phone needs ngrok's link, which shows here, with its QR, once ngrok is up"
)
"""Under the local link, which the panel shows while ngrok is not up."""


def served_elsewhere_state() -> str:
    """The state, while another process serves Remote from this home and this one does not."""
    return "on in another process or another rlx — turn it off there"


def served_elsewhere_link() -> str:
    """The link row, then: that Remote's link is the other process's to show."""
    return "its link is shown where it runs"


WRITES_ON = "writes reach the fleet"
WRITES_OFF = "writes are off: phones can watch the fleet, not act on it"
"""Beside *Allow write actions* while it is off. Not the server's refusal
(``read_only_reason``), which tells a phone to switch it "in the R panel": said in the R
panel, beside the switch it names (0.10.1's final review)."""
LOCKOUT = "new unlocks paused; if the link leaked, New link replaces it and the password"
LOCKOUT_OFF = "new unlocks paused; if the link leaked, turn Remote on, then New link"
"""The lockout's advice while Remote is off here: New link is greyed until Remote is on, so
it says the step before (0.10.1's final review)."""
LOCKOUT_ELSEWHERE = (
    "new unlocks paused; if the link leaked, give it a new link where Remote runs: another "
    "process or another rlx"
)
NEW_LINK_CONFIRM = (
    "New link?",
    "Every phone's saved link stops opening, and every device is signed out. Each one needs "
    "the new link and the new password.",
    "New link",
)
REGENERATE_CONFIRM = (
    "New password?",
    "Every device is signed out and unlocks again with the new password. The link stays.",
    "Regenerate",
)
"""What *New link* and *Regenerate* ask first (:class:`RemoteConfirm`): the question, what
breaks, and the button that does it. Each acted on one click (0.10.1's final review)."""


LOCAL_ZONE: tzinfo | None = None
"""The zone the panel says its times in: ``None`` is this machine's own (a seam for tests)."""
QR_COLOURS = "#ffffff on #000000"
"""The QR's own colours: light glyphs on a dark ground, whatever the theme.

segno's compact art draws the LIGHT modules as block glyphs and leaves the dark
ones to the background, so it reads right only light-on-dark. In the theme's
colours, every light theme (``t`` offers five) and an ANSI one on a light
terminal drew it reflectance-reversed, its quiet zone a dark frame: a scanner
without inversion support could not read it (sweep of #243). Not "black on
white", which reverses it in every theme."""


def qr_art(url: str) -> Text:
    """The QR for ``url`` in :data:`QR_COLOURS`; the notice, plain, when segno is absent."""
    art = qr_text(url)
    return Text(art) if art.startswith(QR_UNAVAILABLE) else Text(art, style=QR_COLOURS)


def qr_text(url: str) -> str:
    """The QR for ``url`` as compact terminal art; a one-line notice when segno is absent."""
    try:
        import segno
    except ImportError:
        return QR_UNAVAILABLE  # what installs segno is the shell's (R13)
    buffer = io.StringIO()
    segno.make(url, error="l").terminal(out=buffer, compact=True, border=1)
    return buffer.getvalue().rstrip("\n")


class RemotePanel(SavesAsYouGo, ModalScreen[None]):
    """Remote on/off · password · allow write · auto-off · devices · link + QR. Esc closes.

    Its two settings, *Allow write actions* and *Auto-off*, are kept as they change, and
    ctrl+z takes the last back (M5, ``cli.ui.save_model``). *Remote* itself, a new link or
    password and a revoke are actions, not settings, with no Undo: a link given out cannot
    be taken back."""

    CSS = """
    RemotePanel { align: center middle; }
    #remotebox { width: 104; max-width: 100%; height: 90%; border: heavy $accent;
                 background: $surface; padding: 0 1; }
    #remotehint { height: 1; color: $text-muted; }
    #remotebox .row { height: auto; min-height: 3; }
    #remotebox .row Static { padding-top: 1; width: auto; }
    /* The two sentences beside a switch wrap in what the row has left: as wide as their
       text, they ran past the box, which cut off another process's auto-off and the
       command that turns writes on. Two ids, to outweigh the rule above. */
    #remotebox #remote-state, #remotebox #remote-write-hint { width: 1fr; }
    #remotebox .row Label { width: 22; padding-top: 1; }
    #remotebox .row Switch { margin-right: 1; }
    #remotebox .row Button { margin-top: 1; margin-left: 2; }
    #remote-status { height: auto; min-height: 1; color: $warning; }
    #remote-unlocks { height: auto; color: $error; }
    #remote-link { width: 100%; height: auto; }
    #remote-auto-off { width: 16; }
    #remote-auto-off-label { padding-top: 2; }
    #remote-qr { height: auto; width: auto; }
    #remote-devices { height: 8; }
    #remote-write-hint { color: $text-muted; }
    """
    BINDINGS: ClassVar = [("escape", "close_panel", "close"), UNDO_BINDING]

    def __init__(self, controller: RemoteController) -> None:
        super().__init__()
        self.controller = controller
        self.ngrok: bool | None = None
        """Whether there is an ngrok to start, as last looked (``settings_remote.ngrok_found``);
        ``None`` until the first look answers."""
        self.token: bool | None = None
        """Whether an ngrok token is configured, as last looked (``has_authtoken``)."""
        self.env_token = False
        """Whether ``NGROK_AUTHTOKEN`` is set, as last looked."""
        self._replacing = False
        """*Change* beside the token opened its slot, until a token is saved."""
        self._was_running: bool | None = None
        self._device_rows: list[tuple[str, ...]] = []
        """The devices table's cells as last painted, a row per device, its id first."""
        self._device_columns: list[ColumnKey] = []
        self._qr_url: tuple[str | None, bool] | None = None
        """The link the QR was last drawn for, and whether another process served Remote
        then; ``None`` before the first paint."""
        self._painted: dict[str, bool] = {}
        """What each switch was last painted to show, by id. A switch showing anything else
        was moved by the user since, and its ``Changed`` is still on its way."""

    # --- layout ---------------------------------------------------------------------------

    def compose(self) -> ComposeResult:
        state = self.controller.state
        writes = self.controller.write_actions_allowed()
        with VerticalScroll(id="remotebox"):
            yield Static("Remote control — the fleet on your phone · Esc closes", id="remotehint")
            with Horizontal(classes="row"):
                yield Label("Remote")
                yield Switch(self.controller.running, id="remote-on")
                yield Static("", id="remote-state")
            yield Static("", id="remote-status")
            # What Remote needs from the owner, under why it has no tunnel: only a damaged
            # install's missing ngrok, and the token while there is none or ngrok refused it.
            yield NgrokMissing(id="settings-remote-ngrok-missing")
            yield TokenSlot(id="settings-remote-token")
            # Copy sits on the LABEL row and the link gets a line of its own.
            # Sharing one row cost the demo its most important control: a real
            # ngrok link is ~84 characters, which with the 22-wide label runs
            # past the 104-wide dialog and pushed Copy off the edge — measured,
            # the button was laid out at x=135 inside a box ending at x=151 and
            # painted nowhere. On its own line the link can be any length (it
            # wraps instead of shoving a neighbour out) and Copy cannot move.
            with Horizontal(classes="row"):
                yield Label("Link")
                yield Button("Copy", id="remote-copy", compact=True)
                new_link = Button("New link", id="remote-new-link", compact=True)
                new_link.tooltip = (
                    "a new link and a new password: the old link stops opening, and every device "
                    "unlocks again"
                )
                yield new_link
            yield Static("", id="remote-link")
            yield Static("", id="remote-qr")
            with Horizontal(classes="row"):
                yield Label("Password")
                yield Static("", id="remote-password")
                yield Button("Regenerate", id="remote-regen", compact=True)
            with Horizontal(classes="row"):
                yield Label("Allow write actions")
                yield Switch(writes, id="remote-allow-write")
                yield Static("", id="remote-write-hint")
            with Horizontal(classes="row"):
                yield Label("Auto-off", id="remote-auto-off-label")
                yield Select(
                    [(_auto_off_label(minutes), minutes) for minutes in AUTO_OFF_CHOICES],
                    value=state.auto_off_minutes,
                    allow_blank=False,
                    id="remote-auto-off",
                )
            with Horizontal(classes="row", id="remote-token-row"):
                yield Label("ngrok token")
                yield Button("Change", id="remote-change-token", compact=True)
            yield Label("Connected devices")
            yield Static("", id="remote-unlocks")
            yield DataTable(id="remote-devices", cursor_type="row", zebra_stripes=True)
            with Horizontal(classes="row"):
                yield Button("Revoke selected", id="remote-revoke", compact=True)

    def on_mount(self) -> None:
        table = self.query_one("#remote-devices", DataTable)
        self._device_columns = table.add_columns("id", "device", "last seen", "expires", "")
        self.repaint()
        self.remote_needs_check()
        self.set_interval(1.0, self.repaint)
        self.query_one("#remote-on", Switch).focus()

    # --- what Remote needs (an ngrok, its token) ------------------------------------------------

    def remote_needs_check(self) -> None:
        """Look again at what Remote needs, off the UI thread, and paint once it answers: the
        SDK's import loads a native library, and the token is looked for in up to two YAML
        files of ngrok's (review of #274, m6)."""
        self.run_worker(
            settings_remote.remote_needs,
            name=CHECK_WORKER,
            group=CHECK_WORKER,
            exclusive=True,
            thread=True,
            exit_on_error=False,
        )

    @on(Worker.StateChanged)
    def _remote_needs_answered(self, event: Worker.StateChanged) -> None:
        needs = event.worker.result
        if event.worker.name != CHECK_WORKER or not isinstance(needs, tuple):
            return
        self.ngrok, self.token, self.env_token = needs
        self._paint_remote_needs()  # what the answer changes, and only that

    def _paint_remote_needs(self) -> None:
        """The missing-ngrok line while ngrok is missing; the token slot while there is no
        token, ngrok refused the one there is (on and off alike), or *Change* asked for it,
        and *Change* itself while there is one. Neither before the first look answers."""
        self.query_one(NgrokMissing).display = self.ngrok is False
        trouble = self.controller.tunnel_error()
        slot = self.query_one(TokenSlot)
        slot.display = self.token is False or trouble in TOKEN_TROUBLE or self._replacing
        slot.remote_paint(
            replacing=bool(self.token) or trouble == BAD_TOKEN, env_token=self.env_token
        )
        self.query_one("#remote-token-row").display = bool(self.token)

    def on_token_saved(self, event: TokenSaved) -> None:
        event.stop()
        self._replacing = False
        self.controller.token_saved()  # a tunnel not up, or signing in, starts with the new one
        self.remote_needs_check()

    def on_ngrok_looked_for(self, event: NgrokLookedFor) -> None:
        event.stop()
        self.remote_needs_check()

    # --- paint from the controller -------------------------------------------------------------

    @while_running
    def repaint(self, *, heard: str | None = None) -> None:
        """Paint everything from the controller; the one-second tick runs it too.

        On Textual's own thread, so a tick reads ``remote_server_status()`` once, for
        the devices and the failed unlocks both, and draws the link and its QR only when
        the link changed: every tick used to encode the QR anew (about 4 ms of segno)
        and read the status twice, each read three digests of ``remote.json``.
        ``heard`` is the switch whose ``Changed`` was just handled (:meth:`_paint_switch`).
        """
        controller = self.controller
        running = controller.running
        if self._was_running and not running:
            self.remote_needs_check()  # back to off: look again
        self._was_running = running
        elsewhere = not running and controller.served_elsewhere()
        writes = controller.write_actions_allowed()
        status = controller.remote_status()
        self._paint_switch("remote-on", running, heard)
        self._paint_switch("remote-allow-write", writes, heard)
        self.query_one("#remote-state", Static).update(self._state_text(elsewhere=elsewhere))
        # Text, never a str, which is read as markup: the sentences carry exception text
        # and paths, where "[b]" was a tag and "[/b]" a MarkupError out of the repaint.
        self.query_one("#remote-status", Static).update(Text(controller.status_line()))
        self._paint_remote_needs()
        self.query_one("#remote-password", Static).update(
            Text(controller.password() or "—", style="bold")
        )
        self.query_one("#remote-write-hint", Static).update(
            Text(WRITES_ON if writes else WRITES_OFF)
        )
        self.query_one("#remote-unlocks", Static).update(
            self._unlocks_text(status, elsewhere=elsewhere)
        )
        # With the token remote.json holds now: a --new-link from a shell redraws both.
        url, public = controller.remote_link()
        if (url, elsewhere) != self._qr_url:
            self._qr_url = (url, elsewhere)
            link = Text(served_elsewhere_link()) if elsewhere else _link_text(url, public=public)
            self.query_one("#remote-link", Static).update(link)
            # The local link's QR led a phone to its own loopback: "cannot connect", for a
            # first-time user without ngrok most of all (sweep of #243).
            self.query_one("#remote-qr", Static).update(qr_art(url) if url and public else "")
        self.query_one("#remote-regen", Button).disabled = not running
        # New link, as Regenerate, acts on the Remote this process serves: off, or served
        # elsewhere, a press would change nothing (review of #542, F1).
        self.query_one("#remote-new-link", Button).disabled = not running
        # The timer of a Remote another process serves is that process's: a pick here saved
        # this UI's next one and changed nothing on the Remote the panel said was on.
        self.query_one("#remote-auto-off", Select).disabled = elsewhere
        self.query_one("#remote-copy", Button).disabled = url is None
        self._paint_devices(controller.devices(status))

    def _paint_switch(self, switch_id: str, value: bool, heard: str | None) -> None:
        """Show the controller's ``value`` on a switch, posting no ``Changed`` of its own.

        Each value written back used to come round as a ``Changed`` the handler took for
        the user's, and with two presses in flight they never ran out: the handler flipped
        the controller, this flipped the switch back, its echo flipped the controller again,
        starting uvicorn and ngrok and revoking every phone, for as long as the panel stayed
        open (sweep of #243). A switch the user moved since the last paint keeps the user's
        word until :meth:`on_switch_changed` has handled it (``heard``): the one-second tick
        landing between a press and its ``Changed`` turned it back, and the press was lost.
        """
        switch = self.query_one(f"#{switch_id}", Switch)
        if heard != switch_id and switch.value != self._painted.get(switch_id, switch.value):
            return
        with self.prevent(Switch.Changed):
            switch.value = value
        self._painted[switch_id] = value

    def _state_text(self, *, elsewhere: bool = False) -> Text:
        """Whether Remote is on, and how; ``elsewhere``: another process serves this home, and
        that Remote's auto-off is said, not this UI's picker's, which beside it read as a
        timer the public Remote did not have (sweep 3 of #243)."""
        controller = self.controller
        if not controller.running and not elsewhere:
            return Text("off", style="dim")
        if not controller.running:
            text = Text(served_elsewhere_state(), style="bold yellow")
            read, served = controller.served_auto_off()
            if read and served is not None:
                text.append(f"  · auto-off at {_auto_off_time(served)}", style="dim")
            elif read:
                text.append("  · no auto-off", style="dim")
            return text
        text = Text("on", style="bold green")
        if controller.public_origin is None:
            text.append("  · local only — no tunnel yet", style="dim")
        deadline = controller.adopt_server_deadline()  # a phone's extension shows here too
        if deadline is not None:
            text.append(f"  · auto-off at {_auto_off_time(deadline)}", style="dim")
        elif controller.state.auto_off_minutes is None:
            # Never: say so, rather than leave the slot the timer usually fills empty —
            # "on" with nothing after it reads like the timer simply has not armed yet.
            text.append("  · no auto-off", style="dim")
        return text

    def _unlocks_text(self, status: dict[str, Any], *, elsewhere: bool = False) -> Text:
        """Wrong passphrases lately, and, once they paused new unlocks, what to do about it:
        New link here, Remote on first while it is off here (New link is greyed until then),
        or where Remote runs when another process serves it (its New link is disabled here,
        as Regenerate is)."""
        failed, until = self.controller.unlock_failures(status)
        if not failed:
            return Text("")
        if until is None:
            return Text(f"{failed} failed unlocks in 30 min", style="dim")
        if elsewhere:
            advice = LOCKOUT_ELSEWHERE
        elif not self.controller.running:
            advice = LOCKOUT_OFF
        else:
            advice = LOCKOUT
        return Text(f"{failed} failed unlocks in 30 min — {advice}")

    def _paint_devices(self, devices: list[dict[str, Any]]) -> None:
        """The devices table, kept to ``devices``. The same devices get their changed cells
        replaced in place, so the cursor and the scroll stay where the user put them; it
        was left alone instead, and a phone signed out by a day idle still read "signed
        in", one back on its old "last seen": the columns a revoke is decided from. Other
        devices rebuild it, the cursor kept on the device it was on, or the row it was."""
        table = self.query_one("#remote-devices", DataTable)
        rows = [_device_cells(device) for device in devices]
        ids = [row[0] for row in rows]
        self.query_one("#remote-revoke", Button).disabled = not ids
        if ids == [row[0] for row in self._device_rows] and table.row_count == len(rows):
            for row, painted in zip(rows, self._device_rows, strict=True):
                for column, cell, was in zip(self._device_columns, row, painted, strict=True):
                    if cell != was:  # "signed out" is wider than "signed in"
                        table.update_cell(row[0], column, Text(cell), update_width=True)
            self._device_rows = rows
            return
        at = table.cursor_row
        on = self._device_rows[at][0] if 0 <= at < len(self._device_rows) else None
        self._device_rows = rows
        table.clear()
        for row in rows:
            # Text cells, never str: a str cell is parsed as Rich markup, and a User-Agent
            # is the phone's own text — `x [/b]` raised MarkupError and took the TUI down.
            table.add_row(*(Text(cell) for cell in row), key=row[0])
        if rows:
            table.move_cursor(row=ids.index(on) if on in ids else min(max(at, 0), len(rows) - 1))

    # --- the controls ---------------------------------------------------------------------------

    def on_switch_changed(self, event: Switch.Changed) -> None:
        """A switch moved. Act on what it shows now, only when that DISAGREES with the
        controller; a repaint posts no ``Changed`` of its own (:meth:`_paint_switch`).

        A ``Changed`` whose value the switch no longer shows is a press a later one has
        taken back, still on its way, and acting on it is what flipped Remote on and off
        for as long as the panel stayed open: two presses before the first was handled
        (impatience while Remote stops, key repeat, keys batched over SSH) are the human's
        net word, which the last of them carries. Comparing against the controller
        needs no flag and cannot go stale: a real toggle never matches it.
        """
        switch = event.switch
        if event.value != switch.value:
            return
        if switch.id == "remote-on" and event.value != self.controller.running:
            # Never waiting for a Remote to stop: its server and ngrok take seconds to wind
            # down, on a thread of their own, and the status line says when they are done.
            if event.value:
                self.controller.turn_on(wait=False)
            else:
                self.controller.turn_off(wait=False)
        elif (
            switch.id == "remote-allow-write"
            and event.value != self.controller.write_actions_allowed()
        ):
            # Written on a thread of its own, as every control's write of remote.json is: it
            # waits for that file's lock (sweep of #243). The switch shows the press at once.
            self.controller.set_allow_write(event.value, wait=False)
            if event.value:
                self.notify("Write actions are ON for remote devices", severity="warning")
            back = not event.value
            self.kept(
                f"allow write actions: {'on' if event.value else 'off'}",
                lambda: self._put_write(back),
            )
        self.repaint(heard=switch.id)

    def _put_write(self, on: bool) -> None:
        """Undo of *Allow write actions*: the switch as it was, written and shown."""
        self.controller.set_allow_write(on, wait=False)
        self.repaint(heard="remote-allow-write")

    def on_select_changed(self, event: Select.Changed) -> None:
        if event.select.id != "remote-auto-off" or event.value not in AUTO_OFF_CHOICES:
            return
        before = self.controller.state.auto_off_minutes
        if event.value == before:
            return  # the Select announcing its initial value at mount — not a change
        self.controller.set_auto_off(event.value, wait=False)
        self.repaint()
        self.kept(f"auto-off: {_auto_off_label(event.value)}", lambda: self._put_auto_off(before))

    def _put_auto_off(self, minutes: int | None) -> None:
        """Undo of *Auto-off*: the choice as it was, written and shown."""
        select = self.query_one("#remote-auto-off", Select)
        with self.prevent(Select.Changed):
            select.value = minutes
        self.controller.set_auto_off(minutes, wait=False)
        self.repaint()

    def on_button_pressed(self, event: Button.Pressed) -> None:
        """The buttons. A new passphrase and a revoke are written on the controller's writer
        thread; the fleet UI says each once it is done (``RemoteController.on_done``).
        *Regenerate* and *New link* ask first, saying what each breaks."""
        if event.button.id == "remote-regen":
            self._confirm_remote(REGENERATE_CONFIRM, new_link=False, pressed=event.button.region)
        elif event.button.id == "remote-new-link":
            self._confirm_remote(NEW_LINK_CONFIRM, new_link=True, pressed=event.button.region)
        elif event.button.id == "remote-copy":
            url = self.controller.link_url()
            if url is not None:
                copy_and_tell(self.app, url, "Link copied", sent="the link")
        elif event.button.id == "remote-revoke":
            self._revoke_selected()
        elif event.button.id == "remote-change-token":
            self._replacing = not self._replacing  # pressed again, it closes the slot
            self.repaint()
            if self._replacing:
                field = self.query_one("#settings-remote-token-field", Input)
                field.focus()
                field.scroll_visible(animate=False)
            return
        self.repaint()

    def _confirm_remote(
        self, asked: tuple[str, str, str], *, new_link: bool, pressed: Region
    ) -> None:
        """Ask ``asked`` (:class:`RemoteConfirm`), away from ``pressed``, the button's cells,
        and once confirmed write the new passphrase, with a new link when ``new_link``.
        Cancel and Esc change nothing. A second press that reached the button before the box
        did (two Enters, two clicks in one frame) asks nothing more: one box, not two."""
        if self.app.screen is not self:
            return

        def remote_confirm_answered(confirmed: bool | None) -> None:
            if confirmed:
                self.controller.regenerate_password(wait=False, new_link=new_link)
                self.repaint()

        self.app.push_screen(RemoteConfirm(*asked, away_from=pressed), remote_confirm_answered)

    def _revoke_selected(self) -> None:
        table = self.query_one("#remote-devices", DataTable)
        row = table.cursor_row
        if not 0 <= row < len(self._device_rows):
            return
        device_id = self._device_rows[row][0]
        self.controller.revoke_device(device_id, wait=False)  # said once done, or why not

    def action_close_panel(self) -> None:
        self.dismiss(None)


class RemoteConfirm(ModalScreen[bool]):
    """*New link?* or *New password?*: what breaks, the action's button and Cancel. Answers
    ``True`` at the action's button, ``None`` at Cancel or Esc; no *Don't ask again*, as a
    misclick here signs every phone out.

    It guards that misclick (#17's round 2: at 80x24 a double-click on *Regenerate*, or Enter
    twice, regenerated with no box left on screen). Cancel has the focus on open, as every
    other ``variant="error"`` box's does (``fresh``'s *Start fresh*, ``fleet_bulk``'s
    :class:`ConfirmBulk`). And it opens in the half of the screen away from ``away_from``, the
    cells of the button that opened it: from 80x24 up its buttons stay in the half it opens
    in, so a second click there lands on the panel's backdrop, never on the action's button."""

    DEFAULT_CSS = """
    RemoteConfirm { align: center middle; }
    RemoteConfirm > Vertical { width: 64; max-width: 92%; height: auto; border: heavy $accent;
                               background: $surface; padding: 0 1; }
    RemoteConfirm #remote-confirm-question { height: auto; padding: 1 0 0 0; text-style: bold; }
    RemoteConfirm #remote-confirm-breaks { height: auto; padding: 1 0; }
    RemoteConfirm Horizontal { height: auto; align-horizontal: right; padding-bottom: 1; }
    RemoteConfirm Horizontal Button { margin-left: 2; min-width: 10; }
    """
    BINDINGS: ClassVar = [Binding("escape", "cancel", "cancel")]
    AUTO_FOCUS = "#remote-confirm-cancel"

    def __init__(
        self, question: str, breaks: str, verb: str, *, away_from: Region | None = None
    ) -> None:
        super().__init__()
        self.question_text = question
        self.breaks_text = breaks
        self.verb = verb
        self.away_from = away_from

    def on_mount(self) -> None:
        """Top or bottom, before the first paint: the half the pressed button is not in."""
        if self.away_from:
            middle = self.away_from.y + self.away_from.height / 2
            self.styles.align_vertical = "top" if middle >= self.app.size.height / 2 else "bottom"

    def compose(self) -> ComposeResult:
        with Vertical():
            yield Static(Text(self.question_text), id="remote-confirm-question")
            yield Static(Text(self.breaks_text), id="remote-confirm-breaks")
            with Horizontal():
                yield Button(self.verb, id="remote-confirm-yes", variant="error")
                yield Button("Cancel", id="remote-confirm-cancel")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        event.stop()
        if event.button.id == "remote-confirm-yes":
            self.dismiss(True)
        else:
            self.dismiss(None)

    def action_cancel(self) -> None:
        self.dismiss(None)


def open_remote_page(app: App[Any]) -> None:
    """Open the one Remote page over ``app``'s one controller (``FleetApp.remote``): what
    ``R``, the palette, the sidebar's chip, the Settings hub's *Remote control* and App
    settings' *Remote* tab all call. Never a second one over the first."""
    if isinstance(app.screen, RemotePanel):
        return
    app.push_screen(RemotePanel(app.remote))  # type: ignore[attr-defined]


def _link_text(url: str | None, *, public: bool) -> Text:
    """The link row: ngrok's link, or the local one and that a phone cannot open it."""
    if url is None:
        return Text("turn Remote on for a link")
    if public:
        return Text(url)
    return Text.assemble(url, "\n", (LOCAL_ONLY, "dim"))


def _auto_off_label(minutes: int | None) -> str:
    """What the Auto-off picker shows for a choice; ``None`` is Never."""
    return "Never" if minutes is None else f"{minutes} min"


def _device_cells(device: dict[str, Any]) -> tuple[str, ...]:
    """One device as the table shows it: id, browser, last seen, sign-in end, state."""
    return (
        str(device["id"]),
        _short_cell(device.get("ua"), 40) or "unknown device",
        _device_time(device.get("last_seen")),
        _device_time(device.get("expires_at")),
        "signed in" if device.get("signed_in") else "signed out",
    )


def _auto_off_time(at: datetime, now: datetime | None = None) -> str:
    """When auto-off comes, in this machine's zone: the time alone today (``21:58``), with its
    date on any other day (``Oct 14 21:58``), as a device's times read (:func:`_device_time`).

    The time alone said nothing of the day: ``serve --auto-off`` sets a deadline up to a week
    ahead, and the panel gave a serve's a day off as ``auto-off at 23:08`` at 23:08, which
    read as now, and one 25 hours off as an hour off (sweep 4 of #243). The panel's own,
    eight hours ahead at most, crosses midnight too.
    """
    here = at.astimezone(LOCAL_ZONE)
    today = (datetime.now(UTC) if now is None else now).astimezone(LOCAL_ZONE).date()
    return f"{here:%H:%M}" if here.date() == today else _dated(here)


def _dated(here: datetime) -> str:
    """``Oct 14 20:58``: a time with its date, the day unpadded."""
    return f"{here:%b} {here.day} {here:%H:%M}"


def _device_time(value: Any) -> str:
    """A device's time in this machine's zone, with its date (``Oct 14 20:58``), as the
    auto-off line says one on another day (:func:`_auto_off_time`): a sign-in ends a week on,
    on the same weekday.

    The server's UTC stamps were cut to 19 characters, the offset and the seconds' last
    digit gone: in Los Angeles at 20:58 a phone seen that second read ``2026-10-08T03:58:0…``,
    tomorrow, two rows under "auto-off at 21:58" (sweep of #243). Read as the server reads
    them (:func:`~aisquare.services.remote_server._remote_instant`: a stamp without its offset
    is UTC); one that is no time at all shows as it came.
    """
    at = _remote_instant(value)
    if at is None:
        return _short_cell(value, 19) or "—"
    return _dated(at.astimezone(LOCAL_ZONE))


def _short_cell(value: Any, width: int) -> str:
    text = str(value) if value is not None else ""
    return text if len(text) <= width else text[: width - 1] + "…"
