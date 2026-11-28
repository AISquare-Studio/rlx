"""The rlx service at login (S2, #33): started by the OS when this user logs in, on by default,
switchable off, per OS, per user and per home. The design is #1875, with the manager's
decisions #1877 and #1879.

- **macOS:** a LaunchAgent, ``~/Library/LaunchAgents/studio.aisquare.rlx.service.plist``, loaded
  in the login (Aqua) session only and started again when it crashes. Not a LaunchDaemon:
  Claude Code keeps each login in the login Keychain, which answers only inside the login
  session, so the service's usage reads need it there. It starts at login, not at boot.
- **Linux, and Windows through WSL** (where the 0.10.2 fleet runs): a systemd user unit,
  ``~/.config/systemd/user/rlx.service``, enabled for ``default.target`` and started again when
  it fails. Running it with nobody logged in (lingering) is a system-level choice, made only
  when the person asks (:func:`turn_lingering_on`). A WSL distro does not start at Windows
  logon, so there it starts with WSL; a Windows logon task is 0.10.3's (rlxd).
- **Native Windows:** nothing. The service runs inside WSL (``service_host.NOT_HERE``), so a
  logon entry would start a process that stops at once. Nothing is recorded either, so 0.10.3's
  rlxd still finds the home undecided, and on by default.

One item per home: the default home's is the label and unit above; a home a variable names
(``RLX_HOME``, ``AISQUARE_HOME``) gets ``.<8 hex of its path>`` and ``rlx-<hex>.service``, so two
homes never fight over one item, as S1's lock keeps one service per home.

The choice is ``config.toml``'s ``[service] start_at_login``, absent until one is made:

- ``rlx init`` and the installers add the item only while it is absent, for the default home
  alone (never a scratch home's), and ``rlx init`` not under CI (:func:`register_by_default`);
  they write ``true``.
- ``rlx service install`` and App settings' *Start rlx at login* add it and write ``true``;
  ``rlx service uninstall`` and the switch turned off take it away and write ``false``.
- Every other path only refreshes an item that is there and ours (:func:`refresh_the_item`): a
  new path to rlx, a new PATH. It never makes one. So a removal in the OS's own settings stays.

The OS's tools (``launchctl``, ``systemctl``, ``loginctl``) are run in one place,
:func:`_service_manager`, which the suite replaces (``tests/conftest.py::no_real_login_item``),
as it points :func:`launch_agents_dir` and :func:`systemd_user_dir` at each test's own folder.
"""

from __future__ import annotations

import contextlib
import hashlib
import os
import plistlib
import re
import subprocess
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal, TypeVar

from aisquare.core import atomic, paths, spawn
from aisquare.core import program as prog
from aisquare.core.config import load_config, save_config
from aisquare.core.version import PRODUCT_NAME

LABEL = "studio.aisquare.rlx.service"
"""The default home's LaunchAgent label, and its file's name (``<label>.plist``)."""
UNIT = f"{PRODUCT_NAME}.service"
"""The default home's systemd user unit."""
SERVICE = f"{PRODUCT_NAME} service"
"""What it is called in what it says (``service_host.SERVICE``)."""
RUN_ARGS: tuple[str, ...] = ("service", "run", "--at-login")
"""What the OS runs: ``rlx service run`` in the foreground, as launchd and systemd expect, told
it was started at login, where a refusal (one runs already) is no crash to restart
(``cli.service``)."""
RESTART_SECONDS = 5
"""systemd's RestartSec: the wait before a failed service is started again."""
STDERR_FOLDER = ("Library", "Logs", "rlx")
"""``~/Library/Logs/rlx/<label>.log``: what the service says before its own log opens. Not in
the home, which the home's move to ``~/.rlx`` (H2, #31) takes from under a loaded agent."""
OS_SECONDS = 15.0
"""How long one call of the OS's tools may take."""

How = Literal["launchd", "systemd"]
T = TypeVar("T")

NOT_HERE = (
    f"{PRODUCT_NAME} starts at login on macOS and Linux, and on Windows inside WSL, where the "
    "fleet runs; there is nothing to set up on Windows itself"
)
NO_SYSTEMD = (
    f"{PRODUCT_NAME} starts at login as a systemd user service, and systemd does not run here"
)
NO_SYSTEMD_IN_WSL = (
    f"{PRODUCT_NAME} starts at login as a systemd user service, and this WSL runs without systemd "
    "(it is turned on in /etc/wsl.conf)"
)
OFF_IN_RLX = f"turned off in {PRODUCT_NAME}"
NEVER_ON = "not set up yet"
REMOVED = f"turned off outside {PRODUCT_NAME}: its login item was removed"
OFF_IN_LOGIN_ITEMS = "turned off in System Settings ▸ General ▸ Login Items"
UNIT_DISABLED = f"turned off outside {PRODUCT_NAME}: its systemd user unit was disabled"
TURN_IT_ON_THERE = "macOS keeps it off: turn it on in System Settings ▸ General ▸ Login Items"
"""Why it will not start at login, as the switch and ``rlx service status`` say it: words, never a
command (asq names none, 0.10.1's R13)."""


class LoginError(Exception):
    """What could not be done; the message is the line to show."""


class NotHere(LoginError):
    """Native Windows: nothing to do, and nothing recorded (#1879)."""


@dataclass(frozen=True)
class OsReply:
    """What one of the OS's tools answered: its exit code, and its output and errors in one."""

    code: int
    said: str

    @property
    def ok(self) -> bool:
        return self.code == 0


def _service_manager(argv: Sequence[str]) -> OsReply:
    """The one place rlx runs ``launchctl``, ``systemctl`` or ``loginctl`` (``core.spawn.
    SEAMS``). Never raises: a tool that is missing or hangs is an answer too."""
    try:
        done = subprocess.run(
            list(argv),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=OS_SECONDS,
            check=False,
            env=spawn.untraced_env(),
        )
    except FileNotFoundError:
        return OsReply(127, f"{argv[0]} is not installed")
    except subprocess.TimeoutExpired:
        return OsReply(124, f"{argv[0]} did not answer within {OS_SECONDS:.0f} s")
    except OSError as exc:
        return OsReply(126, f"{argv[0]}: {exc}")
    said = "\n".join(part.strip() for part in (done.stdout, done.stderr) if part and part.strip())
    return OsReply(done.returncode, said)


# --- where: which OS, which home, and where its login items live ---------------------------------


def _platform() -> str:
    return sys.platform


def _systemd_runs() -> bool:
    """Whether systemd is this system's init (its own test, ``sd_booted``)."""
    return Path("/run/systemd/system").is_dir()


def _in_wsl() -> bool:
    if os.environ.get("WSL_DISTRO_NAME"):
        return True
    try:
        return "microsoft" in Path("/proc/sys/kernel/osrelease").read_text().lower()
    except OSError:
        return False


def _under_ci() -> bool:
    value = os.environ.get("CI")
    return value is not None and value.strip().lower() not in ("", "0", "false", "no", "off")


def login_how() -> How:
    """How the service starts at login here; :class:`NotHere` on native Windows, and
    :class:`LoginError` where systemd does not run."""
    platform = _platform()
    if platform == "darwin":
        return "launchd"
    if platform.startswith("linux"):
        if _systemd_runs():
            return "systemd"
        raise LoginError(NO_SYSTEMD_IN_WSL if _in_wsl() else NO_SYSTEMD)
    raise NotHere(NOT_HERE)


def _named_home() -> Path | None:
    """The home a variable names (``paths.named_home``); ``None`` for the default home, and
    for a variable that names the default home's own path, as the captain's windows are given
    (``services.fleet``): that IS the default home, with its one item (review of #33, m2)."""
    named = paths.named_home()
    if named is None:
        return None
    unnamed = {
        name: value
        for name, value in os.environ.items()
        if name not in (paths.RLX_HOME_ENV, paths.HOME_ENV_VAR)
    }
    default, _ = paths.home_of(unnamed)
    return None if _same_path(named, default) else named


def _same_path(one: Path, other: Path) -> bool:
    try:
        return one.expanduser().resolve() == other.expanduser().resolve()
    except OSError:
        return one.expanduser().absolute() == other.expanduser().absolute()


def _named_by() -> str:
    """The variable that names the home: ``RLX_HOME`` first, as ``paths.named_home`` reads."""
    return paths.RLX_HOME_ENV if os.environ.get(paths.RLX_HOME_ENV) else paths.HOME_ENV_VAR


def home_tag() -> str | None:
    """``None`` for the default home; for a named one, 8 hex of its path."""
    named = _named_home()
    if named is None:
        return None
    return hashlib.sha256(str(named.absolute()).encode()).hexdigest()[:8]


def label() -> str:
    """This home's LaunchAgent label."""
    tag = home_tag()
    return LABEL if tag is None else f"{LABEL}.{tag}"


def unit_name() -> str:
    """This home's systemd user unit."""
    tag = home_tag()
    return UNIT if tag is None else f"{PRODUCT_NAME}-{tag}.service"


def launch_agents_dir() -> Path:
    return Path.home() / "Library" / "LaunchAgents"


def systemd_user_dir() -> Path:
    config = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(config) / "systemd" / "user"


def item_path(how: How) -> Path:
    """This home's LaunchAgent plist, or its user unit's file."""
    if how == "launchd":
        return launch_agents_dir() / f"{label()}.plist"
    return systemd_user_dir() / unit_name()


def _uid() -> int:
    getuid = getattr(os, "getuid", None)  # POSIX's; none on native Windows
    if getuid is None:
        raise NotHere(NOT_HERE)
    return int(getuid())


def _domain() -> str:
    """launchd's domain for this user's login session: ``gui/<uid>``."""
    return f"gui/{_uid()}"


def _target() -> str:
    return f"{_domain()}/{label()}"


# --- what the OS is given ------------------------------------------------------------------------


def entry_point() -> Path:
    """The installed ``rlx`` the OS runs: the uv tool's shim (``~/.local/bin/rlx``) where uv
    recorded one, else the ``rlx`` beside this install's interpreter. :class:`LoginError` for a
    checkout, a development venv or uvx's cache, whose code a login item must not run."""
    from aisquare.services import install_route

    route = install_route.detect()
    if route.kind in (install_route.EDITABLE, install_route.VENV, install_route.UVX):
        raise LoginError(
            f"this {PRODUCT_NAME} is {route.describe()}, which a login item should not run: "
            f"install {PRODUCT_NAME}, then set it up from that install"
        )
    if route.receipt is not None:
        for shim in route.receipt.entrypoints:
            if shim.name == PRODUCT_NAME:
                return shim
    beside = Path(sys.executable).parent / PRODUCT_NAME
    if beside.is_file():
        return beside
    raise LoginError(f"no {PRODUCT_NAME} program was found beside {paths.shown(beside.parent)}")


def service_argv() -> list[str]:
    return [str(entry_point()), *RUN_ARGS]


def service_env(environ: Mapping[str, str] | None = None) -> dict[str, str]:
    """What the item carries of this shell: PATH (tmux, git and the agents must resolve, and
    neither launchd nor systemd reads a shell profile), and the variable that names the home,
    when one does. Nothing else: never ``CLAUDE_CONFIG_DIR`` or any secret."""
    found = os.environ if environ is None else environ
    names = ("PATH", paths.RLX_HOME_ENV, paths.HOME_ENV_VAR)
    return {name: found[name] for name in names if found.get(name)}


def plist_bytes(argv: Sequence[str], env: Mapping[str, str], *, stderr: Path) -> bytes:
    """The LaunchAgent: loaded in the login (Aqua) session only, started when it is loaded,
    started again when it exits other than cleanly (a crash, a signal), never after a stop or
    a refusal, which exit 0. ``AbandonProcessGroup``: its end never takes a tmux server the
    service started, and every agent pane in it, with it."""
    agent: dict[str, Any] = {
        "Label": label(),
        "ProgramArguments": list(argv),
        "EnvironmentVariables": dict(env),
        "LimitLoadToSessionType": "Aqua",
        "RunAtLoad": True,
        "KeepAlive": {"SuccessfulExit": False},
        "ProcessType": "Background",
        "AbandonProcessGroup": True,
        "StandardErrorPath": str(stderr),
    }
    return plistlib.dumps(agent, fmt=plistlib.FMT_XML)


def _systemd_word(word: str, *, in_exec: bool) -> str:
    """``word`` as systemd reads it back: ``%`` doubled (its specifiers), and ``$`` in a command
    line (its variables); quoted when it holds a space, a quote, a backslash or a ``;``."""
    text = word.replace("%", "%%")
    if in_exec:
        text = text.replace("$", "$$")
    if not text or any(char in text for char in " \t\"'\\;"):
        text = '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'
    return text


def unit_text(argv: Sequence[str], env: Mapping[str, str]) -> str:
    """The systemd user unit: started with this user's manager (``default.target``), started
    again when it fails, never after a stop or a refusal, which exit 0. ``KillMode=process``:
    a stop ends the service alone, never the tmux server it started and every agent pane in it
    (the default, ``control-group``, would kill them on each stop and each upgrade)."""
    lines = [
        "[Unit]",
        f"Description={PRODUCT_NAME} service: Remote, the usage reads and the fleet's nudges "
        f"with no {PRODUCT_NAME} window open",
        "Documentation=https://github.com/AISquare-Studio/rlx/blob/main/docs/service.md",
        "",
        "[Service]",
        "Type=simple",
        "ExecStart=" + " ".join(_systemd_word(word, in_exec=True) for word in argv),
        *(
            "Environment=" + _systemd_word(f"{name}={value}", in_exec=False)
            for name, value in env.items()
        ),
        "Restart=on-failure",
        f"RestartSec={RESTART_SECONDS}",
        "KillMode=process",
        "",
        "[Install]",
        "WantedBy=default.target",
        "",
    ]
    return "\n".join(lines)


def _stderr_path() -> Path:
    return launch_agents_dir().parent.parent.joinpath(*STDERR_FOLDER) / f"{label()}.log"


def item_body(how: How) -> bytes:
    """What this home's item holds when written now. :class:`LoginError` when there is no
    installed rlx to run (:func:`entry_point`). The default home's carries no home variable,
    even one that names it: the item reads the same whichever shell wrote it."""
    argv, env = service_argv(), service_env()
    if _named_home() is None:
        env = {name: value for name, value in env.items() if name == "PATH"}
    if how == "launchd":
        return plist_bytes(argv, env, stderr=_stderr_path())
    return unit_text(argv, env).encode()


def _ours(how: How, body: bytes) -> bool:
    """Whether a file at this home's item path is rlx's: its label, or its ``service run``."""
    if how == "launchd":
        try:
            return bool(plistlib.loads(body).get("Label") == label())
        except Exception:  # not a plist at all: not ours to rewrite
            return False
    return any(
        line.startswith("ExecStart=") and " service run" in line
        for line in body.decode(errors="replace").splitlines()
    )


# --- the person's choice -------------------------------------------------------------------------


class ConfigUnreadable(LoginError):
    """``config.toml`` cannot be read, so the choice cannot be: nothing is added, refreshed or
    taken away until it can (tester of #33, B1). The message is the line to show."""


def config_unreadable() -> str | None:
    """Why ``config.toml`` cannot be read, or ``None`` when it can (or is not there)."""
    try:
        load_config()
    except Exception as exc:  # TOMLDecodeError, a ValidationError, an OSError: all say why
        said = " ".join(str(exc).split())  # pydantic's is four lines: one, as a note is
        return f"config.toml cannot be read ({type(exc).__name__}: {said})"
    return None


def _readable_or_refused() -> None:
    why = config_unreadable()
    if why is not None:
        raise ConfigUnreadable(f"{why}: the login item is left as it is until it can be read")


def read_choice() -> bool | None:
    """``[service] start_at_login``; ``None`` while nothing was chosen, or the file cannot be
    read (shown as undecided; every writer here refuses first, :class:`ConfigUnreadable`)."""
    try:
        return load_config().service.start_at_login
    except Exception:  # an unreadable config.toml: the form and doctor say so
        return None


def _keep_the_choice(on: bool | None) -> None:
    """``[service] start_at_login`` written (``None``: taken away). Raises what ``save_config``
    raises (``PermissionError``: the CLI's ``expected_config_write_errors``)."""
    config = load_config()
    if config.service.start_at_login is on:
        return
    config.service.start_at_login = on
    save_config(config)


# --- how it stands -------------------------------------------------------------------------------


@dataclass(frozen=True)
class AtLogin:
    """Whether the OS starts the service at this user's next login, and if not, why."""

    how: How | None
    on: bool
    item: Path | None
    choice: bool | None
    why_off: str | None

    @property
    def can(self) -> bool:
        """Whether it can start at login here at all."""
        return self.how is not None

    def to_json(self) -> dict[str, Any]:
        return {
            "on": self.on,
            "how": self.how,
            "item": None if self.item is None else str(self.item),
            "chosen": None if self.choice is None else ("on" if self.choice else "off"),
            "why_off": self.why_off,
        }


def at_login_state() -> AtLogin:
    """How it stands: the item in place and not turned off by the OS is on."""
    choice = read_choice()
    try:
        how = login_how()
    except LoginError as exc:
        return AtLogin(None, False, None, choice, str(exc))
    item = item_path(how)
    if not item.is_file():
        unreadable = config_unreadable() if choice is None else None
        why = unreadable or (NEVER_ON if choice is None else REMOVED if choice else OFF_IN_RLX)
        return AtLogin(how, False, item, choice, why)
    off = _turned_off_by_the_os(how)
    if off is not None and choice is False:
        off = OFF_IN_RLX
    return AtLogin(how, off is None, item, choice, off)


def _turned_off_by_the_os(how: How) -> str | None:
    """Why the OS will not start the item that is in place; ``None`` when it will (or cannot
    say: an item in place is taken at its word)."""
    if how == "launchd":
        reply = _service_manager(["launchctl", "print-disabled", _domain()])
        return OFF_IN_LOGIN_ITEMS if reply.ok and _disabled_in(reply.said, label()) else None
    reply = _service_manager(["systemctl", "--user", "is-enabled", unit_name()])
    state = reply.said.strip().splitlines()[-1].strip() if reply.said.strip() else ""
    if state in ("disabled", "masked", "masked-runtime"):
        return UNIT_DISABLED
    return None


def _disabled_in(listing: str, name: str) -> bool:
    """Whether ``launchctl print-disabled`` lists ``name`` as disabled: ``"<label>" =>
    disabled`` (macOS 13 and later) or ``=> true`` (before)."""
    for line in listing.splitlines():
        listed, arrow, value = line.strip().partition("=>")
        if arrow and listed.strip().strip('"') == name:
            return value.strip().lower() in ("disabled", "true")
    return False


# --- adding it and taking it away ----------------------------------------------------------------


@dataclass(frozen=True)
class Registered:
    """What :func:`register_at_login` did: how it stands now, and what is worth saying."""

    state: AtLogin
    notes: tuple[str, ...] = ()


def register_at_login(*, linger: bool = False) -> Registered:
    """Add this home's login item (again), and start the service through the OS now: an explicit
    install, so it also takes the item off launchd's disabled list. Records the choice as on.
    :class:`NotHere` on native Windows (nothing recorded), :class:`LoginError` when it cannot be
    added, or its file cannot be written; what ``save_config`` raises for the choice."""
    how = login_how()
    _readable_or_refused()
    body = item_body(how)
    notes = _add_launch_agent(body) if how == "launchd" else _add_user_unit(body)
    if how == "systemd" and linger:
        notes.append(turn_lingering_on())
    _keep_the_choice(True)
    state = at_login_state()
    if how == "launchd" and state.why_off == OFF_IN_LOGIN_ITEMS:
        # Background Task Management keeps an item turned off there whatever launchctl says.
        notes.append(TURN_IT_ON_THERE)
    return Registered(state, tuple(notes))


def _write_item(item: Path, body: bytes) -> None:
    try:
        item.parent.mkdir(parents=True, exist_ok=True)
        atomic.write_replacing(item, body, keep_mode=False)
    except OSError as exc:
        raise LoginError(f"could not write {paths.shown(item)}: {exc.strerror or exc}") from exc


def _add_launch_agent(body: bytes) -> list[str]:
    domain, target, item = _domain(), _target(), item_path("launchd")
    loaded = _service_manager(["launchctl", "print", target]).ok
    _write_item(item, body)
    _stderr_path().parent.mkdir(parents=True, exist_ok=True)
    notes: list[str] = []
    enabled = _service_manager(["launchctl", "enable", target])
    if not enabled.ok:
        notes.append(f"launchd kept it on its disabled list: {enabled.said or enabled.code}")
    if loaded:
        # Loaded already: started if it is not running, and a running one is left as it is.
        _service_manager(["launchctl", "kickstart", target])
        return notes
    started = _service_manager(["launchctl", "bootstrap", domain, str(item)])
    if not started.ok:
        # Over SSH, say, with no login session at the Mac to load it into: the file is in
        # place, and the next login there loads it.
        notes.append(
            "it starts at your next login at the Mac: launchd could not load it in this "
            f"session ({started.said or started.code})"
        )
    return notes


def _add_user_unit(body: bytes) -> list[str]:
    """The unit written, loaded and enabled; one systemd will not take is taken away again, so
    nothing reads as on (review of #33, m1)."""
    item = item_path("systemd")
    existed = item.is_file()
    _write_item(item, body)
    reloaded = _service_manager(["systemctl", "--user", "daemon-reload"])
    if not reloaded.ok:
        _back_out(item, existed)
        raise LoginError(f"systemd's user manager did not answer: {reloaded.said or reloaded.code}")
    enabled = _service_manager(["systemctl", "--user", "enable", "--now", unit_name()])
    if not enabled.ok:
        _back_out(item, existed)
        _service_manager(["systemctl", "--user", "daemon-reload"])
        raise LoginError(f"systemd did not enable {unit_name()}: {enabled.said or enabled.code}")
    return []


def _back_out(item: Path, existed: bool) -> None:
    """A unit this install wrote, removed after systemd refused it: one that was there before
    (refreshed, then refused) is left for the person, as it was theirs to remove."""
    if not existed:
        with contextlib.suppress(OSError):
            item.unlink(missing_ok=True)


def unregister_at_login() -> AtLogin:
    """Take this home's login item away, stopping the service the OS runs from it, and record
    the choice as off. :class:`NotHere` on native Windows (nothing recorded), :class:`LoginError`
    when its file stays; what ``save_config`` raises for the choice."""
    how = login_how()
    _readable_or_refused()
    _take_the_item_away(how)
    _keep_the_choice(False)
    return at_login_state()


@dataclass(frozen=True)
class TakenItem:
    """One login item :func:`forget_at_login` took away, as it was: its file's bytes (``None``
    for a file its person cannot read, taken away all the same; tester of #33, round 4, NEW-1),
    the label (unit) it is loaded by, and whether the OS had it loaded (launchd) or enabled
    (systemd)."""

    path: Path
    body: bytes | None
    name: str
    live: bool


@dataclass(frozen=True)
class Taken:
    """What :func:`forget_at_login` took away, so :func:`put_back` can put exactly that back:
    the items, and this home's choice as it was (and whether it was forgotten)."""

    how: How | None
    items: tuple[TakenItem, ...] = ()
    choice: bool | None = None
    forgot: bool = False

    @property
    def gone(self) -> bool:
        """Whether an item was taken away."""
        return bool(self.items)


class NotAllTaken(LoginError):
    """An item's file stayed (its folder cannot be written): what was taken away before and
    after it, for :func:`put_back` (tester of #33, round 4, NIT)."""

    def __init__(self, message: str, taken: Taken) -> None:
        super().__init__(message)
        self.taken = taken


def forget_at_login() -> Taken:
    """``rlx uninstall``'s step, and a move to a release with no service (``rlx upgrade``):
    EVERY home's login item taken away, as the install they all run goes or loses its service
    (or the OS would start a program that is gone, or a command that release does not have,
    at every login, and that release has no way to take one away; tester of #33, round 2, m1).

    This home's ON choice is forgotten only when its item was there and on, so a later install
    is on by default again: one whose item was removed or turned off in the OS stays, or the
    next init would add back what the person took away (review of #33, round 3); an OFF one
    stays too, as that release keeps a section it does not know. One in a file that cannot be
    read is left as it is. What was taken away, for :func:`put_back`. :class:`NotAllTaken` when
    an item's file stays, carrying what was taken away all the same."""
    try:
        how = login_how()
    except LoginError:
        return Taken(None)  # nothing can be in place here
    choice = read_choice()
    was_on = at_login_state().on
    items, stayed = _take_every_item_away(how)
    forgot = False
    if choice is True and was_on:
        # A config.toml it cannot write: the uninstall goes on without it.
        with contextlib.suppress(OSError):
            _keep_the_choice(None)
            forgot = True
    taken = Taken(how, tuple(items), choice, forgot)
    if stayed:
        raise NotAllTaken("; ".join(stayed), taken)
    return taken


def put_back(taken: Taken) -> str | None:
    """Exactly what :func:`forget_at_login` took away, put back, after a move that did not
    happen: whether each item starts at login, as it did, and the choice as it was. Each file
    as it was, and a unit enabled again only where it was; never an explicit install, which
    would turn back on an item the person had turned off, or add this home's for another's
    (review of #33, round 3).

    And nothing started: a LaunchAgent loaded again (``bootstrap``) starts at once, as its
    ``RunAtLoad`` says, and so does a unit enabled ``--now``, so a service the upgrade had
    stopped, and would not start again, ran again (manager's decision #2927). The agent loads
    at the next login; whether the service runs now is the upgrade's to say, and a start goes
    through the OS (:func:`start_through_the_os`). What to say of it; ``None`` when nothing was
    taken away."""
    if not taken.items and not taken.forgot:
        return None
    back = [item for item in taken.items if _written_back(item)]
    missed = [item for item in taken.items if item not in back]
    if taken.how == "systemd" and back:
        _service_manager(["systemctl", "--user", "daemon-reload"])
        for item in back:
            if item.live:
                _service_manager(["systemctl", "--user", "enable", item.name])
    mine = None if taken.how is None else item_path(taken.how)
    if taken.forgot and all(item.path != mine for item in missed):
        # This home's ON, its item back. One that could not come back stays forgotten, so the
        # default home's next init adds it again rather than read it as removed in the OS.
        with contextlib.suppress(OSError):
            _keep_the_choice(taken.choice)
    said = f"the {SERVICE}'s login items, taken away for the move, were put back as they were"
    if not missed:
        return said
    return (
        f"{said}, but for {', '.join(paths.shown(item.path) for item in missed)}, which could "
        "not be"
        + prog.shell_advice(
            f" (`{prog.words('service', 'install')}`, run for its home, adds it again)"
        )
    )


def _written_back(item: TakenItem) -> bool:
    """``item``'s file as it was: rewritten, or still there as it was (its folder would not let
    it go); ``False`` when neither, or its bytes were never read."""
    if item.body is None:
        return False
    with contextlib.suppress(OSError):
        if item.path.read_bytes() == item.body:
            return True
    try:
        _write_item(item.path, item.body)
    except LoginError:
        return False
    return True


def _every_item(how: How) -> list[tuple[Path, str]]:
    """Every home's rlx login item in place, with the label (unit) it is loaded by: the
    default home's and each named one's (``.<8 hex>``, ``rlx-<8 hex>.service``)."""
    found: list[tuple[Path, str]] = []
    if how == "launchd":
        for item in sorted(launch_agents_dir().glob(f"{LABEL}*.plist")):
            name = item.name.removesuffix(".plist")
            if name == LABEL or _NAMED_SUFFIX.fullmatch(name.removeprefix(f"{LABEL}.")):
                found.append((item, name))
        return found
    for item in sorted(systemd_user_dir().glob(f"{PRODUCT_NAME}*.service")):
        stem = item.name.removesuffix(".service")
        named = _NAMED_SUFFIX.fullmatch(stem.removeprefix(f"{PRODUCT_NAME}-"))
        if (item.name == UNIT or named) and _ours_or_unreadable(how, item):
            found.append((item, item.name))
    return found


def _ours_or_unreadable(how: How, item: Path) -> bool:
    """:func:`_ours`, or a file named as ours that cannot be read to tell, taken for ours: a
    name this exact is rlx's (tester of #33, round 4, NEW-1)."""
    try:
        return _ours(how, item.read_bytes())
    except OSError:
        return True


def _live(how: How, name: str) -> bool:
    """Whether the OS has the item loaded (launchd) or enabled (systemd) now."""
    if how == "launchd":
        return _service_manager(["launchctl", "print", f"{_domain()}/{name}"]).ok
    reply = _service_manager(["systemctl", "--user", "is-enabled", name])
    return reply.ok and reply.said.strip().splitlines()[-1:] == ["enabled"]


def _take_every_item_away(how: How) -> tuple[list[TakenItem], list[str]]:
    """This home's item (booted out whether its file is there or not), then every other home's
    (:func:`_every_item`), each as it was before; and why each file that stayed did. One that
    stays stops none of the rest from going (tester of #33, round 4, NIT)."""
    mine = item_path(how)
    candidates = [(mine, label() if how == "launchd" else unit_name())]
    candidates += [(path, name) for path, name in _every_item(how) if path != mine]
    taken = [
        TakenItem(path, _bytes_or_none(path), name, _live(how, name))
        for path, name in candidates
        if path.is_file()
    ]
    stayed: list[str] = []
    try:
        _take_the_item_away(how)
    except LoginError as exc:
        stayed.append(str(exc))
    others = [gone for gone in taken if gone.path != mine]
    for gone in others:
        if how == "launchd":
            _service_manager(["launchctl", "bootout", f"{_domain()}/{gone.name}"])
        else:
            _service_manager(["systemctl", "--user", "disable", "--now", gone.name])
        try:
            gone.path.unlink(missing_ok=True)
        except OSError as exc:
            stayed.append(f"could not remove {paths.shown(gone.path)}: {exc.strerror or exc}")
    if others and how == "systemd":
        _service_manager(["systemctl", "--user", "daemon-reload"])
    return taken, stayed


def _bytes_or_none(item: Path) -> bytes | None:
    """``item``'s bytes, kept to put back; ``None`` for a file that cannot be read (``chmod
    000``), which is taken away all the same: nothing the move needs is in it."""
    try:
        return item.read_bytes()
    except OSError:
        return None


_NAMED_SUFFIX = re.compile(r"[0-9a-f]{8}")
"""A named home's tag (:func:`home_tag`)."""


def _take_the_item_away(how: How) -> None:
    """Booted out (disabled and stopped) whether its file is there or not: a job loaded from a
    file deleted since stays loaded until logout (tester of #33). "Not loaded" is no failure."""
    item = item_path(how)
    if how == "launchd":
        _service_manager(["launchctl", "bootout", _target()])
    else:
        _service_manager(["systemctl", "--user", "disable", "--now", unit_name()])
    try:
        item.unlink(missing_ok=True)
    except OSError as exc:
        raise LoginError(f"could not remove {paths.shown(item)}: {exc.strerror or exc}") from exc
    if how == "systemd":
        _service_manager(["systemctl", "--user", "daemon-reload"])


def refresh_the_item() -> bool:
    """An item that is there and ours rewritten as it would be written now (a new path to rlx,
    a new PATH), and never one made; whether it changed. Never raises: a refresh that cannot
    be done leaves the item as it was. A loaded LaunchAgent takes the new file at its next load:
    reloading it would stop the service."""
    try:
        how = login_how()
        item = item_path(how)
        if not item.is_file() or config_unreadable() is not None:
            return False
        old = item.read_bytes()
        new = item_body(how)
    except (LoginError, OSError):
        return False
    if old == new or not _ours(how, old):
        return False
    try:
        _write_item(item, new)
    except LoginError:
        return False
    if how == "systemd":
        _service_manager(["systemctl", "--user", "daemon-reload"])
    return True


def register_by_default(*, by_installer: bool = False) -> str:
    """``rlx init``'s step, and the installers' (``rlx service install --if-undecided``): add the
    item while nothing was chosen, for the default home only, and not under CI unless an
    installer asks (installing is the person's act); refresh it otherwise. What to say of it.
    Raises only what ``save_config`` raises for the choice."""
    try:
        login_how()
    except NotHere as exc:
        return f"the {PRODUCT_NAME} service does not start at login: {exc}"
    except LoginError:
        pass  # said below, from how it stands
    unreadable = config_unreadable()
    if unreadable is not None:
        # The choice cannot be read, so it cannot be told from no choice: nothing is added,
        # refreshed or taken away (tester of #33, B1).
        return f"the {PRODUCT_NAME} service's login item was left as it is: {unreadable}"
    if read_choice() is not None:
        refresh_the_item()
        return state_line(at_login_state())
    named = _named_home()
    if named is not None:
        return (
            f"the {PRODUCT_NAME} service was not set to start at login: this home is named by "
            f"{_named_by()}, and a named home gets a login item only when asked"
            + prog.shell_advice(f" (`{prog.words('service', 'install')}`)")
        )
    if _under_ci() and not by_installer:
        return (
            f"the {PRODUCT_NAME} service was not set to start at login: CI is set, and a CI run "
            "adds no login item unless asked"
            + prog.shell_advice(f" (`{prog.words('service', 'install')}`)")
        )
    try:
        done = register_at_login()
    except LoginError as exc:
        return f"the {PRODUCT_NAME} service does not start at login: {exc}"
    return "; ".join((state_line(done.state), *done.notes))


def decline_by_default() -> str:
    """``rlx init --no-service``'s step, and so the installers' ``--no-service``: record that the
    person said no while nothing was chosen, so a later init, installer or ``--if-undecided``
    never adds the item (the manager's ruling on #33). A choice already made is left as it is:
    the switch and ``rlx service uninstall`` turn it off. Native Windows and an unreadable
    ``config.toml`` record nothing. What to say of it. Raises only what ``save_config`` raises."""
    try:
        login_how()
    except NotHere as exc:
        return f"the {PRODUCT_NAME} service does not start at login: {exc}"
    except LoginError:
        pass  # said below, from how it stands
    unreadable = config_unreadable()
    if unreadable is not None:
        return f"the {PRODUCT_NAME} service's login item was left as it is: {unreadable}"
    if read_choice() is None:
        _keep_the_choice(False)
        return (
            f"the {PRODUCT_NAME} service does not start at login: --no-service, kept for later "
            "runs" + prog.shell_advice(f" (`{prog.words('service', 'install')}` turns it on)")
        )
    return state_line(at_login_state())


def record_from_the_item() -> str:
    """``rlx init --reinit --yes`` replaced a ``config.toml`` it could not read, and no choice
    could be read from its text: for the default home, the choice recorded as the OS has it,
    the item in place being on and none being off, so no later init adds one the person had
    turned off (tester of #33, round 2). Nothing is added or taken away. What to say of it."""
    try:
        how = login_how()
    except LoginError:
        return state_line(at_login_state())
    # The default home's alone: a named one never gets an item by itself, so nothing about it
    # needs protecting, and its reset stays the defaults it asked for.
    if _named_home() is None and read_choice() is None and config_unreadable() is None:
        _keep_the_choice(item_path(how).is_file())
    return state_line(at_login_state())


def state_line(state: AtLogin) -> str:
    """How it stands, in one line for a person."""
    if state.on and state.item is not None:
        return f"the {PRODUCT_NAME} service starts at login ({paths.shown(state.item)})"
    return f"the {PRODUCT_NAME} service does not start at login: {state.why_off}"


# --- starting it through the OS ------------------------------------------------------------------


def start_through_the_os() -> bool:
    """Ask the OS to start the service from this home's item, when the item is there, ours and
    not turned off, so the service that holds S1's lock is the one the OS supervises, not a copy
    it does not know. Whether the OS took the request; the caller waits for the service.

    A LaunchAgent launchd has not loaded (one put back after an upgrade that did not move,
    :func:`put_back`, or written away from the Mac) is loaded, which starts it (``RunAtLoad``);
    one loaded is kickstarted. Away from a login session at the Mac the load fails, and the
    caller starts the service itself."""
    try:
        how = login_how()
        item = item_path(how)
        if not item.is_file() or not _ours(how, item.read_bytes()):
            return False
    except (LoginError, OSError):
        return False
    if _turned_off_by_the_os(how) is not None:
        return False
    if how == "launchd":
        if not _service_manager(["launchctl", "print", _target()]).ok:
            return _service_manager(["launchctl", "bootstrap", _domain(), str(item)]).ok
        return _service_manager(["launchctl", "kickstart", _target()]).ok
    return _service_manager(["systemctl", "--user", "start", unit_name()]).ok


def wait_for(found: Callable[[], T | None], timeout: float) -> T | None:
    """``found()`` asked every tenth of a second until it answers, for ``timeout`` seconds."""
    deadline = time.monotonic() + timeout
    while True:
        value = found()
        if value is not None or time.monotonic() >= deadline:
            return value
        time.sleep(0.1)


# --- Linux: lingering ----------------------------------------------------------------------------


def _user() -> str:
    import getpass

    try:
        return getpass.getuser()
    except (KeyError, OSError):
        return ""


def lingering() -> bool | None:
    """Whether systemd keeps this user's services running with nobody logged in; ``None`` when
    it cannot say."""
    reply = _service_manager(["loginctl", "show-user", _user(), "--property=Linger", "--value"])
    if not reply.ok:
        return None
    return reply.said.strip().lower() == "yes"


def turn_lingering_on() -> str:
    """Asked for, and only then: ``loginctl enable-linger`` for this user, a system-level choice
    (its services run from boot, with nobody logged in). What to say of it."""
    reply = _service_manager(["loginctl", "enable-linger", _user()])
    if reply.ok:
        return "it runs with nobody logged in too (systemd's lingering, for your user)"
    return f"lingering stays off: loginctl said {reply.said or reply.code}"


__all__ = [
    "LABEL",
    "RUN_ARGS",
    "UNIT",
    "AtLogin",
    "ConfigUnreadable",
    "LoginError",
    "NotHere",
    "OsReply",
    "Registered",
    "Taken",
    "at_login_state",
    "config_unreadable",
    "decline_by_default",
    "entry_point",
    "forget_at_login",
    "home_tag",
    "item_path",
    "label",
    "lingering",
    "login_how",
    "plist_bytes",
    "put_back",
    "read_choice",
    "record_from_the_item",
    "refresh_the_item",
    "register_at_login",
    "register_by_default",
    "service_argv",
    "service_env",
    "start_through_the_os",
    "state_line",
    "turn_lingering_on",
    "unit_name",
    "unit_text",
    "unregister_at_login",
    "wait_for",
]
