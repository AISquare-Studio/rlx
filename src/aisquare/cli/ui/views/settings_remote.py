"""What Remote needs from the owner, the sidebar's chip, and App settings' way in (#33, Q17).

Remote is one page now, the ``R`` panel (``views.remote``): App settings' *Remote* tab held a
second one over the same controller, each with what the other lacked (0.10.2, Q17). What only
this module's page had moved there, and is still defined here, where its tests import it:

- *Paste your ngrok token* (:class:`TokenSlot`): the field, where a free one comes from, and
  *Save*. Remote needs nothing else from the owner (the owner, 2026-10-23: Remote works out
  of the box, #34). Only an install whose ngrok is missing gets one more line, with a fix in
  the app, never a command to run (:class:`NgrokMissing`).
- **What Remote needs** (#34: ngrok bundled, its token pasted here), in one place:
  :func:`ngrok_found` (is there an ngrok to start: the bundled SDK, else one on PATH),
  :func:`has_authtoken` (is a token configured), :func:`store_ngrok_token` (check a pasted
  one with ngrok and keep it, or raise :class:`TokenRefused` with why) and
  :meth:`NgrokMissing.fix` (look again, for a damaged install's ngrok). All of it is
  ``services.ngrok_sdk``'s; :func:`remote_needs` looks at it on a worker.

:class:`RemoteLink` is App settings' *Remote* tab: what Remote is, whether it is on, and *Open
Remote control*, which opens the Remote page as ``R`` does.

:class:`RemoteChip` is the sidebar's ``📡 remote · N devices`` while Remote is on: a click
opens the Remote page.
"""

from __future__ import annotations

import os
import shutil
import threading
from collections.abc import Mapping
from pathlib import Path

from rich.text import Text
from textual import events, on
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.message import Message
from textual.widgets import Button, Input, Static
from textual.worker import Worker, WorkerState

from aisquare.cli.ui.remote_control import RemoteController
from aisquare.cli.ui.save_model import CommitInput
from aisquare.cli.ui.ticks import while_running
from aisquare.services import ngrok_sdk
from aisquare.services.ngrok_sdk import BAD_TOKEN
from aisquare.services.ngrok_tunnel import AUTHTOKEN_FROM as AUTHTOKEN_FROM
from aisquare.services.ngrok_tunnel import AUTHTOKEN_HINT

CHIP_TICK_S = 2.0
"""How often the sidebar's chip, and App settings' *Remote* tab, look whether Remote is on,
and how many devices are in."""

TOKEN_WORKER = "settings-remote-token"
"""The worker that saves a pasted ngrok token (:func:`store_ngrok_token`)."""
CHECK_WORKER = "settings-remote-check"
"""The worker that looks at what Remote needs (:func:`remote_needs`)."""

TITLE = ("Remote control", "the fleet on your phone")
"""The tab's heading: its name, and what it is, dim (``app_settings.section_title``)."""
LINK_ABOUT = "a link, its QR code and a password, on one page: R opens it from anywhere"
OPEN_LABEL = "Open Remote control"
TOKEN_TITLE = "Paste your ngrok token"
REPLACE_TITLE = "Replace your ngrok token"
TOKEN_ABOUT = "Remote reaches your phone through ngrok: a free account's token, once."
ENV_TOKEN = (
    "NGROK_AUTHTOKEN is set where rlx started: ngrok signs in with it, not with a token saved here"
)
"""The one line beside the token field while ``NGROK_AUTHTOKEN`` overrides what it saves."""
NGROK_MISSING = ngrok_sdk.SDK_MISSING
TOKEN_TROUBLE = (BAD_TOKEN, AUTHTOKEN_HINT)
"""What a tunnel says that ended for want of a token ngrok takes: the token slot shows."""


# --- what Remote needs (#34: services.ngrok_sdk) ---------------------------------------------


class TokenRefused(Exception):
    """Why a pasted token was not saved, in plain words: the line beside the field says it."""


def ngrok_found() -> bool:
    """Whether there is an ngrok to start: the bundled one (``ngrok_sdk``), which every install
    has but a damaged one, else an ``ngrok`` on PATH."""
    return ngrok_sdk.sdk() is not None or shutil.which("ngrok") is not None


def has_authtoken(
    environ: Mapping[str, str] = os.environ, configs: list[Path] | None = None
) -> bool:
    """Whether ngrok has a token to sign in with (``ngrok_sdk.find_authtoken``):
    ``NGROK_AUTHTOKEN``, the one pasted here, or an ``authtoken:`` in the human's own ngrok
    config (``configs``: ngrok 3's and ngrok 2's when not given)."""
    return ngrok_sdk.find_authtoken(environ, configs) is not None


def token_from_environment(environ: Mapping[str, str] = os.environ) -> bool:
    """Whether ``NGROK_AUTHTOKEN`` is set: ngrok signs in with it, whatever is saved here."""
    return bool(environ.get("NGROK_AUTHTOKEN", "").strip())


def remote_needs() -> tuple[bool, bool, bool]:
    """What Remote needs, as it is now: an ngrok (:func:`ngrok_found`, which imports the SDK's
    native library), a token (:func:`has_authtoken`, which parses ngrok's own configs), and
    whether ``NGROK_AUTHTOKEN`` overrides a saved one. On a worker (:data:`CHECK_WORKER`)."""
    return ngrok_found(), has_authtoken(), token_from_environment()


def store_ngrok_token(token: str) -> None:
    """Save ``token`` where Remote's ngrok reads it, or raise :class:`TokenRefused` saying why.

    ngrok's service is asked first whether it takes the token (``ngrok_sdk.check_token``);
    one it refuses is not kept. Kept in the aisquare home, owner-only. The token is never
    said: not in a toast, a log or an error. On a worker (:data:`TOKEN_WORKER`).
    """
    token = token.strip()
    if not token:
        raise TokenRefused("paste the token first")
    if any(character.isspace() for character in token):
        raise TokenRefused("that is not one token: it has spaces in it")
    refused = ngrok_sdk.check_token(token)
    if refused is not None:
        raise TokenRefused(refused)
    try:
        ngrok_sdk.save_token(token)
    except OSError as exc:
        raise TokenRefused(f"the token could not be saved: {type(exc).__name__}") from None


class RemoteLink(Vertical):
    """App settings' *Remote* tab: what Remote is, whether it is on, and *Open Remote control*,
    which opens the one Remote page (``views.remote.open_remote_page``), as ``R`` does. Remote
    is set up there, not here: the tab held a second Remote page (Q17)."""

    DEFAULT_CSS = """
    RemoteLink { height: auto; }
    RemoteLink #settings-remote-state { height: auto; margin-bottom: 1; }
    RemoteLink Horizontal { height: auto; }
    """

    def __init__(self, controller: RemoteController | None = None, *, id: str | None = None):
        super().__init__(id=id)
        self._controller = controller

    @property
    def controller(self) -> RemoteController:
        """The app's one Remote: the one the Remote page is a view over."""
        if self._controller is None:
            self._controller = self.app.remote  # type: ignore[attr-defined]
        return self._controller

    def compose(self) -> ComposeResult:
        # Here, not at the top: App settings, which holds the form's format, mounts this.
        from aisquare.cli.ui.views.app_settings import section_title

        yield Static(section_title(*TITLE), id="settings-remote-title", classes="section")
        yield Static(Text(LINK_ABOUT), id="settings-remote-about", classes="about")
        yield Static("", id="settings-remote-state")
        with Horizontal():
            yield Button(OPEN_LABEL, id="settings-remote-open", variant="primary", compact=True)

    def on_mount(self) -> None:
        self.remote_link_paint()
        self.set_interval(CHIP_TICK_S, self.remote_link_paint)

    @while_running
    def remote_link_paint(self) -> None:
        """Whether Remote is on, and how many devices are signed in, as the chip says it."""
        controller = self.controller
        if not controller.running:
            self.query_one("#settings-remote-state", Static).update(
                Text("Remote is off", style="dim")
            )
            return
        signed_in = sum(1 for device in controller.devices() if device.get("signed_in"))
        noun = "device" if signed_in == 1 else "devices"
        self.query_one("#settings-remote-state", Static).update(
            Text.assemble(("● on", "bold green"), f"  · {signed_in} {noun} signed in")
        )

    @on(Button.Pressed, "#settings-remote-open")
    def _open_remote_pressed(self, event: Button.Pressed) -> None:
        event.stop()
        from aisquare.cli.ui.views.remote import open_remote_page  # it imports this module

        open_remote_page(self.app)


class TokenSaved(Message):
    """A pasted ngrok token was saved: the Remote page looks again at what Remote needs."""


class NgrokLookedFor(Message):
    """:meth:`NgrokMissing.fix` ran: the Remote page looks again."""


class TokenSlot(Vertical):
    """*Paste your ngrok token*: the field, where a free one comes from, and why a token was
    refused, beside the field. The Remote page shows it while no token is
    configured, and as *Replace your ngrok token* while ngrok refuses the one there is or its
    *Change* asked for it (``RemotePanel._paint_remote_needs``), with a line saying so where
    ``NGROK_AUTHTOKEN`` overrides what it saves.

    Enter, or leaving the field (M5: there is no *Save*), hands it to :func:`store_ngrok_token`
    on a worker; the field hides what is typed, and the token is said nowhere. No ctrl+z takes
    a token back: the one it replaced is a secret rlx keeps no copy of.
    """

    DEFAULT_CSS = """
    TokenSlot { height: auto; margin: 1 0 0 0; }
    TokenSlot #settings-remote-token-title { height: 1; text-style: bold; }
    TokenSlot .remote-token-about { height: auto; color: $text-muted; }
    TokenSlot #settings-remote-token-row { height: auto; margin: 1 0 0 0; }
    TokenSlot #settings-remote-token-field { width: 1fr; max-width: 60; }
    TokenSlot #settings-remote-token-row Button { margin: 0 0 0 2; }
    TokenSlot #settings-remote-token-problem { height: auto; color: $error; }
    TokenSlot #settings-remote-token-env { height: auto; color: $warning; }
    """

    def compose(self) -> ComposeResult:
        yield Static(Text(TOKEN_TITLE), id="settings-remote-token-title")
        yield Static(Text(TOKEN_ABOUT), classes="remote-token-about")
        yield Static(Text(ENV_TOKEN), id="settings-remote-token-env")
        yield Static(
            Text.assemble("A free one: ", (AUTHTOKEN_FROM, f"link {AUTHTOKEN_FROM}")),
            id="settings-remote-token-link",
            classes="remote-token-about",
        )
        with Horizontal(id="settings-remote-token-row"):
            yield CommitInput(
                placeholder="your ngrok token",
                password=True,
                id="settings-remote-token-field",
                compact=True,
            )
            yield Button("Get one", id="settings-remote-token-open", compact=True)
        yield Static("", id="settings-remote-token-problem")

    def on_mount(self) -> None:
        field = self.query_one("#settings-remote-token-field", CommitInput)
        field.leaving = self.remote_keep_typed_token  # set here, as every page's (M5)
        self.query_one("#settings-remote-token-problem").display = False
        self.query_one("#settings-remote-token-env").display = False

    def remote_paint(self, *, replacing: bool, env_token: bool) -> None:
        """*Replace your ngrok token* where there is one, else *Paste…*; the line saying that
        ``NGROK_AUTHTOKEN`` wins over what is saved here, while it is set."""
        title = REPLACE_TITLE if replacing else TOKEN_TITLE
        shown = self.query_one("#settings-remote-token-title", Static)
        if str(shown.content) != title:
            shown.update(Text(title))
        self.query_one("#settings-remote-token-env").display = env_token

    @on(Button.Pressed, "#settings-remote-token-open")
    def _remote_open_dashboard(self, event: Button.Pressed) -> None:
        event.stop()
        self.app.open_url(AUTHTOKEN_FROM)

    @on(CommitInput.Committed, "#settings-remote-token-field")
    def _save_committed(self, event: CommitInput.Committed) -> None:
        event.stop()
        if event.value:  # a field emptied is no token to save
            self.remote_save_token()

    def remote_keep_typed_token(self, field: CommitInput) -> None:
        """The page closed, or rlx quitting, with a token typed and not entered
        (``CommitInput.leaving``): checked and saved as Enter would, showing nothing
        here. While rlx runs, on a worker of the app's, which says how it went; quitting, before
        rlx goes, for as long as ngrok's check may take (``ngrok_sdk.CHECK_SECONDS``) at most."""
        token = field.value
        if not token.strip():
            return
        app = self.app

        def remote_store_typed() -> str:
            try:
                store_ngrok_token(token)
            except TokenRefused as exc:
                return f"the ngrok token was not saved: {exc}"
            except Exception as exc:  # never the token: only what kind of failure
                return f"the ngrok token could not be saved: {type(exc).__name__}"
            return "✓ ngrok token saved"

        if app._running:

            def remote_store_and_say() -> None:
                said = remote_store_typed()
                app.call_from_thread(app.notify, said, timeout=6, markup=False)

            app.run_worker(
                remote_store_and_say,
                name=TOKEN_WORKER,
                group=TOKEN_WORKER,
                thread=True,
                exit_on_error=False,
            )
            return
        quitting = threading.Thread(target=remote_store_typed, name=TOKEN_WORKER, daemon=True)
        quitting.start()
        quitting.join(ngrok_sdk.CHECK_SECONDS + 1)

    def remote_save_token(self) -> None:
        """Save what the field holds, off the UI thread; one save at a time."""
        if any(
            worker.node is self and worker.name == TOKEN_WORKER and not worker.is_finished
            for worker in self.workers
        ):
            return
        token = self.query_one("#settings-remote-token-field", Input).value
        self.run_worker(
            lambda: store_ngrok_token(token),
            name=TOKEN_WORKER,
            group=TOKEN_WORKER,
            thread=True,
            exit_on_error=False,
        )

    def on_worker_state_changed(self, event: Worker.StateChanged) -> None:
        if event.worker.name != TOKEN_WORKER or not event.worker.is_finished:
            return
        event.stop()
        problem = self.query_one("#settings-remote-token-problem", Static)
        error = event.worker.error
        if event.state is WorkerState.SUCCESS:
            self.query_one("#settings-remote-token-field", CommitInput).keep("")
            problem.display = False
            self.notify("✓ ngrok token saved", timeout=5, markup=False)
            self.post_message(TokenSaved())
            return
        self.query_one("#settings-remote-token-field", CommitInput).release()  # to try again
        if isinstance(error, TokenRefused):
            said = str(error)
        elif event.state is WorkerState.ERROR:  # never the token: only what kind of failure
            said = f"the token could not be saved: {type(error).__name__}"
        else:
            return
        problem.update(Text(said))
        problem.display = True


class NgrokMissing(Vertical):
    """The one line an install whose ngrok is missing gets, with its fix in the app: never a
    command to run in a terminal. Only a damaged install gets it: the bundled ngrok is a core
    dependency (#34). :meth:`fix` looks again, for an ngrok put back meanwhile."""

    DEFAULT_CSS = """
    NgrokMissing { height: auto; margin: 1 0 0 0; }
    NgrokMissing #settings-remote-ngrok-line { height: auto; color: $warning; }
    NgrokMissing Horizontal { height: auto; margin: 1 0 0 0; }
    """

    def compose(self) -> ComposeResult:
        yield Static(Text(f"✗ {NGROK_MISSING}"), id="settings-remote-ngrok-line")
        with Horizontal():
            yield Button("Check again", id="settings-remote-ngrok-fix", compact=True)

    @on(Button.Pressed, "#settings-remote-ngrok-fix")
    def _fix_pressed(self, event: Button.Pressed) -> None:
        event.stop()
        self.fix()

    def fix(self) -> None:
        """The in-app fix for a missing ngrok, then a look again."""
        self.post_message(NgrokLookedFor())


class RemoteChip(Static):
    """``📡 remote · N devices`` in the sidebar while this asq's Remote is on; hidden while it
    is off. A click opens the Remote page.

    A ``Static``, not one of the sidebar's ``Activatable`` rows: never under the keyboard
    cursor, and none of its names is the sidebar's (``tests/test_remote_names_stay_off_the_
    hook_graph.py``: a remote module shares no name with the rest)."""

    DEFAULT_CSS = """
    RemoteChip { height: 1; color: $success; text-wrap: nowrap; text-overflow: ellipsis; }
    """

    ALLOW_SELECT = False

    def __init__(self, controller: RemoteController, *, id: str | None = None) -> None:
        super().__init__(Text(""), id=id)
        self.controller = controller
        self.tooltip = "Remote control is on — open it (R)"

    def on_mount(self) -> None:
        self.display = False
        self.remote_tick()
        self.set_interval(CHIP_TICK_S, self.remote_tick)

    @while_running
    def remote_tick(self) -> None:
        controller = self.controller
        self.display = controller.running
        if not controller.running:
            return
        signed_in = sum(1 for device in controller.devices() if device.get("signed_in"))
        noun = "device" if signed_in == 1 else "devices"
        self.update(Text(f"📡 remote · {signed_in} {noun}"))

    def on_click(self, event: events.Click) -> None:
        event.stop()
        from aisquare.cli.ui.views.remote import open_remote_page  # it imports this module

        open_remote_page(self.app)
