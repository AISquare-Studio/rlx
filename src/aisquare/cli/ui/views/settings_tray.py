"""Settings' *Tray* (#34): the rlx service's menu bar / tray icon, on App settings ▸ User
preferences.

Three rows, each saved at once through ``core.config.save_config`` (the one writer) and taken
back by its page's ctrl+z (M5, ``cli.ui.save_model``):

- *Show rlx's icon in the menu bar* (the tray, on Linux) (``[tray] enabled``);
- *Icon*: rlx's backslash, the whole r\\x mark, or the backslash as a template (macOS paints
  it as its other menu bar icons);
- *Open rlx in*: the person's own terminal app, or one named.

The running service follows within half a minute (``service_tray.TrayPart``): it starts,
stops or redraws its tray. App settings' Reset all re-reads the file under them
(:meth:`TrayToggle.reread`).
"""

from __future__ import annotations

import sys
from collections.abc import Callable

from rich.text import Text
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.widgets import Label, Select, Static, Switch

from aisquare.cli.ui.save_model import page_of
from aisquare.core.config import AppConfig, TraySettings, load_config, save_config
from aisquare.core.version import PRODUCT_NAME

TITLE = ("Tray", f"the {PRODUCT_NAME} service's icon")
ABOUT = (
    f"The {PRODUCT_NAME} service shows it while it runs, within half a minute of a change: "
    f"its menu opens {PRODUCT_NAME}, restarts it or quits it. macOS: the menu bar; Linux: the "
    "tray, where the desktop has one. Template: the backslash painted as macOS's other icons."
)
ICON_OPTIONS = (
    ("Backslash", "backslash"),
    ("r\\x mark", "mark"),
    ("Template", "template"),
)
"""Short, so the select fits the tab at 80x24: the about line says what a template is."""
SHOW_LABEL = (
    f"Show {PRODUCT_NAME}'s icon in the {'menu bar' if sys.platform == 'darwin' else 'tray'}"
)
"""Where the icon goes on this OS: macOS's menu bar, Linux's tray."""
DEFAULT_TERMINAL = "Your default terminal"
MAC_TERMINALS = ("Terminal", "iTerm", "Ghostty", "WezTerm", "kitty", "Alacritty", "Warp")
"""Apps macOS can open rlx's ``.command`` in (``open -a``)."""
LINUX_TERMINALS = (
    "gnome-terminal",
    "konsole",
    "xfce4-terminal",
    "kitty",
    "alacritty",
    "foot",
    "wezterm",
    "xterm",
)


def terminal_options(current: str, platform: str = sys.platform) -> list[tuple[str, str]]:
    """The *Open rlx in* choices: the person's own first, then the common ones, and the one
    the file names when it is none of those (kept, not dropped)."""
    names = MAC_TERMINALS if platform == "darwin" else LINUX_TERMINALS
    options = [(DEFAULT_TERMINAL, ""), *((name, name) for name in names)]
    if current and current not in names:
        options.append((current, current))
    return options


class TrayToggle(Vertical):
    """The *Tray* section: a switch and two selects, each kept as it changes."""

    DEFAULT_CSS = """
    TrayToggle { height: auto; }
    """

    _shown: TraySettings = TraySettings()
    """What the file held as the rows were last set: what ctrl+z puts back."""

    def compose(self) -> ComposeResult:
        # Here, not at the top: App settings, which holds the form's format, mounts this.
        from aisquare.cli.ui.views.app_settings import section_title

        try:
            settings = load_config().tray
        except Exception:  # a broken config.toml: the form says so; the rows show defaults
            settings = TraySettings()
        self._shown = settings
        yield Static(section_title(*TITLE), id="tray-title", classes="section")
        with Horizontal(classes="row preference"):
            yield Label(SHOW_LABEL, classes="preference-label")
            yield Switch(settings.enabled, id="tray-enabled")
        with Horizontal(classes="row preference"):
            # A row's plain label, as the captain's agent's: it gives the select its 26 columns
            # in the tab's 45 at 80x24, which a preference label's 32 would not.
            yield Label("Icon", id="tray-icon-label")
            yield Select(
                list(ICON_OPTIONS),
                value=settings.icon,
                allow_blank=False,
                id="tray-icon",
                compact=True,
            )
        with Horizontal(classes="row preference"):
            yield Label(f"Open {PRODUCT_NAME} in", id="tray-terminal-label")
            yield Select(
                terminal_options(settings.terminal),
                value=settings.terminal,
                allow_blank=False,
                id="tray-terminal",
                compact=True,
            )
        yield Static(Text(ABOUT), id="tray-about", classes="about")

    def reread(self, config: AppConfig) -> None:
        """The form re-read the file (Reset all): the rows show what it holds."""
        settings = config.tray
        self._shown = settings
        with self.prevent(Switch.Changed, Select.Changed):
            self.query_one("#tray-enabled", Switch).value = settings.enabled
            self.query_one("#tray-icon", Select).value = settings.icon
            terminal = self.query_one("#tray-terminal", Select)
            terminal.set_options(terminal_options(settings.terminal))
            terminal.value = settings.terminal

    def on_switch_changed(self, event: Switch.Changed) -> None:
        if event.switch.id != "tray-enabled":
            return
        event.stop()
        on = bool(event.value)
        self._keep_tray(
            f"the tray: {'on' if on else 'off'}",
            lambda settings: settings.model_copy(update={"enabled": on}),
        )

    def on_select_changed(self, event: Select.Changed) -> None:
        if event.select.id not in ("tray-icon", "tray-terminal"):
            return
        event.stop()
        value = event.value if isinstance(event.value, str) else ""
        key = "icon" if event.select.id == "tray-icon" else "terminal"
        if getattr(self._shown, key) == value:
            return  # what the file holds already: the select's first Changed
        what = "the tray's icon" if key == "icon" else "the tray's terminal"
        self._keep_tray(what, lambda settings: settings.model_copy(update={key: value}))

    def _keep_tray(self, what: str, change: Callable[[TraySettings], TraySettings]) -> None:
        before = self._shown
        problem = self._save_tray(change(before))
        if problem is not None:
            self.notify(problem, severity="error", timeout=8, markup=False)
            self._show_tray(before)
            return
        page = page_of(self)
        if page is not None:
            page.kept(what, lambda: self._put_tray_back(before))

    def _put_tray_back(self, settings: TraySettings) -> bool:
        """ctrl+z: the tray's settings as the file held them before, shown and saved."""
        problem = self._save_tray(settings)
        if problem is not None:
            self.notify(problem, severity="error", timeout=8, markup=False)
            return False
        self._show_tray(settings)
        return True

    def _show_tray(self, settings: TraySettings) -> None:
        with self.prevent(Switch.Changed, Select.Changed):
            self.query_one("#tray-enabled", Switch).value = settings.enabled
            self.query_one("#tray-icon", Select).value = settings.icon
            self.query_one("#tray-terminal", Select).value = settings.terminal

    def _save_tray(self, settings: TraySettings) -> str | None:
        """Write ``[tray]``; why not, when the config would not take it."""
        try:
            config = load_config()
        except Exception as exc:  # a broken config.toml: say so, change nothing
            return f"config unreadable — the tray is left as it was: {exc}"
        config.tray = settings
        try:
            save_config(config)
        except OSError as exc:  # the operator's filesystem saying no
            return f"could not write the config — the tray is left as it was: {exc}"
        self._shown = settings
        return None
