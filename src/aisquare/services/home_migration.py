"""The move of a home from before 0.10.2 to the new layout (#31, H2): ``~/.aisquare`` to
``~/.rlx``, or a home ``RLX_HOME``/``AISQUARE_HOME`` names in place, as docs/home-layout.md lays
it out.

WHEN: only when asked, by ``rlx upgrade`` (and the app's Update, which runs it) and its hidden
``rlx move-home``; never on an ordinary command's start (the manager's condition 1 on #31).
An old home keeps working as it is until then (H1).

ONLY WHEN THE HOME IS QUIET (:func:`running_on_home`): no rlx on it, no fleet agent of any of
its projects that may still run, no live pane on rlx's tmux server or one a launch record names,
no watcher, no Remote served from another process, no ``rlx launch`` waiting. Otherwise it
refuses, naming each, and nothing is touched.

HOW: every step is a rename on the same volume (never a copy and a delete) or a rewrite of what
holds a path, written down first in a journal kept in the home (:class:`Journal`,
``backups/home-migration/journal.json``), so a crash at any step resumes on the next run, or rolls
back. The steps, in order:

1. ``backup``: the store, checkpointed, copied by SQLite's backup API into the journal's folder,
   where it stays until the next release; and what a replay may start from today (the fleet rows
   whose launch records place them) and which managed accounts are signed in.
2. ``keychain``: on macOS, each managed Claude Code slot's login is copied to the Keychain item
   its new place names (``core.claude_accounts.keychain_service``), as Claude Code writes one,
   through ``security -i`` on its stdin, and read back by Claude Code's own lookup: the same
   bytes, or the move stops. A login too long for that line is never put on a command line: the
   slot is pinned to its old item's name instead (``core.claude_accounts.keychain_pin``).
3. ``move``: each old name at the home's top renamed to its new place (``paths.MOVES``); in a
   named home the store goes last, as the rename that makes it read as new; the default home
   then renames ``~/.aisquare`` to ``~/.rlx``, the rename that makes it read as new.
4. ``rewrite``: the paths the store, ``agents.json``, ``state.json``, ``config.toml`` and
   ``remote.json`` hold, in one transaction for the store; and every launch record re-issued by
   the seal code itself (``core.launch_seal.reissue``) with its paths moved. Each file is copied
   aside first.
5. ``check``: every fleet row whose replay was allowed before is allowed now, by the same check
   (``services.fleet._replay_refusal``, never loosened), and every account signed in before is
   signed in now; otherwise the move rolls back whole.
6. ``hooks``: each managed account's hooks written again for its new place (``agents
   refresh-hooks``, which records Codex's trust of them), or the paths in its settings moved.
7. ``link``: ``~/.aisquare`` left as a link to ``~/.rlx`` for one release (POSIX; a junction on
   Windows), so a stale reference (a shell snapshot, a script) still finds the home.
8. ``forget``: the old Keychain items deleted.
9. ``done``.

ROLLBACK is defined for every step before 8 (:func:`roll_back_move`): the link removed, each file
copied aside put back (every file a hook refresh writes among them), the store restored from its
backup, each rename reversed, the root's first, and the Keychain copies deleted and the pins
taken away, so the home is as it was. The roll-back is journaled too, phase by phase
(:data:`UNDO_PHASES`), so a crash during it is finished by the next run. A failure at any step
rolls back on its own; a crash leaves the journal, and the next run resumes (or ``--rollback``
rolls back). Only step 8 and after are forward-only: once the old items are deleted, the move
only goes on. A move back to a release before 0.10.2 after it finds no ``context.db`` in
``~/.aisquare`` and starts empty (:data:`DOWNGRADE_GAP`).
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import re
import shutil
import sqlite3
import sys
import tempfile
from collections.abc import Callable, Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal

from pydantic import BaseModel, Field, ValidationError

from aisquare.core import claude_accounts as accounts_core
from aisquare.core import launch_seal, paths
from aisquare.core import program as prog
from aisquare.core.atomic import write_replacing
from aisquare.core.locking import holding
from aisquare.models import ClaudeAccount

if TYPE_CHECKING:
    from aisquare.core.tmux import TmuxServer

JOURNAL_VERSION = 1
FOLDER: tuple[str, ...] = paths.MOVE_JOURNAL[:-1]
"""The move's own folder under the home: its journal, the store's copy from before it, and each
file it rewrote as it was. Kept until the next release (the manager's condition 6 on #31)."""
JOURNAL_NAME = paths.MOVE_JOURNAL[-1]
STEPS = ("backup", "keychain", "move", "rewrite", "check", "hooks", "link", "forget", "done")
"""The journal's steps, in order (the module's docstring)."""
UNDO_PHASES = ("link", "aside", "store", "root", "renames", "made", "keychain", "folder")
"""A roll-back's phases, in order: each is written down once done, and each within it (a file put
back, a rename reversed) as it is done, so the next run finishes it where it stopped."""
FORWARD_ONLY = "forget"
"""The first step after which the move cannot be rolled back: the old Keychain items go."""
FIRST_RELEASE = "0.10.2"
"""The first release that reads a home in the new layout."""
DOWNGRADE_GAP = (
    f"rlx before {FIRST_RELEASE} keeps its home in ~/.aisquare, in the old layout: run after "
    "the move, it finds no context.db there and starts empty, so do not move back to one"
)
"""What a move back costs after the move (the manager's condition 7 on #31)."""
STORE_COLUMNS: tuple[tuple[str, str], ...] = (
    ("project", "root"),
    ("project", "linked_repos"),
    ("fleet_agent", "cwd"),
    ("fleet_agent", "launch_spec"),
    ("fleet_agent", "first_prompt"),
    ("fleet_agent", "socket_path"),
    ("team_session", "transcript_path"),
    ("team_session", "account"),
    ("usage_session", "transcript_path"),
    ("claude_account", "config_dir"),
    ("agent_account", "config_dir"),
    ("project_explainability", "key_path"),
    ("brief", "data"),
    ("repo_profile", "root"),
    ("project_setting", "value"),
    ("ui_state", "value"),
    ("team_meta", "value"),
    ("metric", "snapshot_ref"),
)
"""The store's columns that hold a path, or a value with one in it (a launch spec, a brief, a
setting): each path under the old home is moved. The rest (notes, entries, prompts) is prose,
left as written and counted (:attr:`Moved.left`): ``~/.aisquare``'s link keeps a path in it
working for one release."""
HOME_FILES: tuple[Callable[[], Path], ...] = (
    paths.agents_registry_path,
    paths.state_path,
    paths.config_path,
    paths.remote_state_path,
)
"""The home's own files that may hold a path under it, asked of ``core.paths`` once the home is in
the new layout: the connected agents' directories, the accounts' and roles' directories in
config."""


class MigrationRefused(Exception):
    """The move would not start: the home is not quiet, or it cannot be moved as it stands.
    Nothing was touched."""

    def __init__(self, reason: str, running: Sequence[str] = ()) -> None:
        super().__init__(reason)
        self.reason = reason
        self.running = tuple(running)


class MigrationFailed(Exception):
    """A step failed. ``rolled_back``: the home is as it was before the move; else the journal
    holds where it stopped, and the next run resumes it."""

    def __init__(self, reason: str, *, rolled_back: bool) -> None:
        super().__init__(reason)
        self.reason = reason
        self.rolled_back = rolled_back


# --- the journal -------------------------------------------------------------------------------


class Rename(BaseModel):
    """One rename, by the names under the home's root: written before it is made."""

    source: list[str]
    target: list[str]
    done: bool = False
    skipped: bool = False
    """Nothing was there to rename when its turn came."""
    undone: bool = False
    """Reversed by a roll-back."""


class Copied(BaseModel):
    """One Keychain item copied to the name the slot's new place hashes to."""

    agent: str
    slot: int
    account: str
    old: str
    new: str
    state: Literal["planned", "copying", "copied", "absent", "pinned", "forgotten"] = "planned"
    """``pinned``: too long to copy without a command line, so the slot is pinned to its old
    item's name instead (``core.claude_accounts.keychain_pin``), and nothing is copied."""
    existed: bool = False
    """The new item was there before, holding the same login: a roll-back leaves it."""


class Aside(BaseModel):
    """A file the move rewrote, copied aside first: where it is (by its names under the root,
    in the new layout) and where its copy is (under :data:`FOLDER`)."""

    place: list[str]
    copy_at: list[str]
    tree: bool = False
    created: bool = False
    """The move made the file (a hook refresh's trust record): a roll-back removes it."""
    restored: bool = False


class Journal(BaseModel):
    """What the move did so far, kept in the home it moves (:data:`FOLDER`), so it goes with the
    home's last rename and the next run finds it there.

    Rollback is defined at every step before :data:`FORWARD_ONLY` (:func:`roll_back_move`): the link
    removed, each file in :attr:`aside` put back, the store restored from its backup, each
    rename in :attr:`renames` reversed (the root's first), and each Keychain copy in
    :attr:`items` deleted and each pin taken away; each phase written down in :attr:`undone`.
    A check that fails after the rewrite (step ``check``: the fleet rows whose replay is allowed
    not the same as before, or an account signed out) rolls back whole. From
    :data:`FORWARD_ONLY` on (the old Keychain items deleted) the move only goes on."""

    version: int = JOURNAL_VERSION
    old_root: str
    new_root: str
    named: bool
    spellings: list[str]
    """The old root as a stored path may spell it (as given, resolved; ``~/.aisquare``)."""
    started_at: datetime
    steps: list[str] = Field(default_factory=list)
    """The steps done, in order."""
    forgetting: bool = False
    """Step :data:`FORWARD_ONLY` began: no roll-back from here."""
    rolling_back: bool = False
    undone: list[str] = Field(default_factory=list)
    """The roll-back's phases done (:data:`UNDO_PHASES`), so a crash in it is finished, never
    begun again over what it already put back (the tester's blocker 2 on #31)."""
    in_flight: int | None = None
    """The rename a crash may have stopped between making it and writing it down, as the
    roll-back found it when it began."""
    renames: list[Rename] = Field(default_factory=list)
    renames_planned: bool = False
    """:attr:`renames` was listed, from the home as it stood when the step began (the backup
    before it may have made a folder the home did not have, as any command does)."""
    made: list[list[str]] = Field(default_factory=list)
    """The folders the renames made for their targets, outermost first."""
    flipped: bool = False
    """The default home's root renamed (``~/.aisquare`` to ``~/.rlx``)."""
    linked: bool = False
    items: list[Copied] = Field(default_factory=list)
    aside: list[Aside] = Field(default_factory=list)
    accepted: list[str] = Field(default_factory=list)
    """The fleet rows whose replay was allowed before the move."""
    signed_in: list[str] = Field(default_factory=list)
    """The managed accounts signed in before the move (``agent:slot``)."""
    notes: list[str] = Field(default_factory=list)
    left: list[str] = Field(default_factory=list)

    @property
    def root(self) -> Path:
        """Where the home is now: the old root until the default home's root is renamed."""
        return Path(self.new_root if self.flipped else self.old_root)


def _move_folder(root: Path) -> Path:
    return root.joinpath(*FOLDER)


def _move_journal_path(root: Path) -> Path:
    return _move_folder(root) / JOURNAL_NAME


def _save_move_journal(journal: Journal) -> None:
    path = _move_journal_path(journal.root)
    path.parent.mkdir(parents=True, exist_ok=True)
    write_replacing(path, journal.model_dump_json(indent=1) + "\n", keep_mode=False)


def _unfinished_move(root: Path) -> Journal | None:
    """The journal of a move of the home at ``root`` that has not finished, or ``None``."""
    try:
        journal = Journal.model_validate_json(_move_journal_path(root).read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, ValidationError):
        return None
    if "done" in journal.steps or journal.version > JOURNAL_VERSION:
        return None
    return journal


# --- where the home goes -----------------------------------------------------------------------


@dataclass(frozen=True)
class Plan:
    """The move a home needs: where it is, where it goes, and what is renamed."""

    old_root: Path
    new_root: Path
    named: bool
    renames: tuple[tuple[str, tuple[str, ...]], ...]
    """Each name at the old root's top and its place in the new layout."""
    journal: Journal | None = None
    """An unfinished move's journal: the next run resumes it."""

    @property
    def shown(self) -> str:
        """The move in words: ``~/.aisquare → ~/.rlx`` or ``<home>, in place``."""
        if self.named:
            return f"{paths.shown(self.old_root)}, in place"
        return f"{paths.shown(self.old_root)} → {paths.shown(self.new_root)}"


def planned_move() -> Plan | None:
    """The move this process's home needs (:func:`paths.home_of`), or ``None``: a home in the new
    layout with no unfinished move, or no home at all. Reads only. Raises
    :class:`MigrationRefused` for a home that cannot be moved as it stands: both ``~/.aisquare``
    and ``~/.rlx`` there, or a place in the new layout taken already."""
    default = _names_the_default_home()
    root, layout = paths.home_of(_without_home_variables() if default else None)
    named = paths.named_home() is not None and not default
    unfinished = _unfinished_move(root)
    if unfinished is not None:
        if not unfinished.named:
            # Found where it lies: under ~/.rlx, the root's rename was made, whatever a crash
            # between that rename and its line in the journal left the journal saying.
            unfinished.flipped = root == Path(unfinished.new_root)
        return Plan(
            Path(unfinished.old_root),
            Path(unfinished.new_root),
            unfinished.named,
            (),
            unfinished,
        )
    if layout == paths.NEW:
        old = Path.home() / paths.OLD_HOME_NAME
        if not named and root.is_dir() and old.is_dir() and not old.is_symlink():
            raise MigrationRefused(
                f"both {paths.shown(old)} and {paths.shown(root)} are here: rlx uses "
                f"{paths.shown(root)}, and {paths.shown(old)} is left as it is"
            )
        return None
    if not root.is_dir():
        return None
    new_root = root if named else Path.home() / paths.HOME_NAME
    if not named and os.path.lexists(new_root):
        raise MigrationRefused(f"{paths.shown(new_root)} is there already")
    renames: list[tuple[str, tuple[str, ...]]] = []
    taken: list[str] = []
    for name in sorted(os.listdir(root)):
        place = paths.new_place(name)
        if place is None:
            continue
        target = root.joinpath(*place)
        if os.path.lexists(target) and not _an_empty_folder(target):
            taken.append(f"{name} (its place, {'/'.join(place)}, holds files already)")
        renames.append((name, place))
    if taken:
        raise MigrationRefused(f"{paths.shown(root)} cannot be laid out anew: {'; '.join(taken)}")
    return Plan(root, new_root, named, tuple(renames))


def _an_empty_folder(path: Path) -> bool:
    try:
        return path.is_dir() and not path.is_symlink() and not any(path.iterdir())
    except OSError:
        return False


# --- is the home quiet -------------------------------------------------------------------------


def running_on_home(*, but_the_service: bool = False) -> list[str]:
    """What runs on this home, each named with how it is stopped; ``[]`` for a quiet home (the
    manager's condition 2 on #31: the whole home, every project's). Reads only, and writes
    nothing: what cannot be asked is said as running, so a home that cannot be told quiet is
    not moved."""
    from aisquare.core import instance_scan, instances, launchers
    from aisquare.services import lifecycle, watch

    found: list[str] = []
    roll = instances.roll(prune=False)
    mine = instances.this_instance()
    for instance in roll.live:
        if but_the_service and instance.kind == "service":
            continue  # the move stops it, and starts it again after
        if mine is None or instance.id != mine.id:
            found.append(f"{instance.label}: quit it")
    for unread in roll.unreadable:
        found.append(f"an rlx this release cannot read (pid {unread.pid or '?'}): quit it")
    registered = {instance.pid for instance in roll.live}
    for ui in instance_scan.unregistered_uis(registered):
        found.append(f"rlx (pid {ui.pid}): quit it")
    live, _unlistened, fleet_error = lifecycle.running_fleet()
    if fleet_error is not None:
        found.append(f"the fleet, which could not be read ({fleet_error})")
    shut_down = prog.shell_advice(f" ({prog.word()} fleet shutdown --all)")
    found += [f"fleet agent {label}: stop it{shut_down}" for label in live]
    found += _rlx_server_panes()
    watcher = watch.running_pid()
    if watcher is not None:
        pid = f"pid {watcher}" if watcher > 0 else "its pid unknown"
        found.append(
            f"the watcher ({pid}): stop it{prog.shell_advice(f' ({prog.word()} watch --stop)')}"
        )
    remote = lifecycle.running_remote()
    if remote is not None:
        found.append(f"Remote, on in {remote}: turn it off")
    found += [f"{label}: end it" for label in lifecycle.running_launchers()]
    # The agents `rlx launch` became (POSIX exec), on a managed slot or the plain one: their
    # hooks write into the home and their login is refreshed in the Keychain item step 8
    # deletes (the reviewer's M4 on #31).
    found += [f"{session.label}: end it" for session in launchers.exec_sessions()]
    return found


def _rlx_server_panes() -> list[str]:
    """The live panes on rlx's tmux server (today's config's socket) and on each server a
    launch record names, but the one this process runs in: anything on rlx's own server may
    run under the home (a fleet window, a sign-in, an editor)."""
    from aisquare.core import config as config_core
    from aisquare.core.tmux import TmuxError, TmuxUnavailable, asking_server

    servers: dict[tuple[str, str | None], None] = {}
    try:
        servers[(config_core.load_config().fleet.tmux_socket, None)] = None
    except Exception:  # an unreadable config: ask the default socket
        servers[("asq", None)] = None
    for _digest, window in launch_seal.recorded_windows():
        servers[(window.tmux_socket, window.socket_path)] = None
    found: list[str] = []
    for socket, path in servers:
        server = asking_server(socket)
        if path is not None:
            server = server.at(Path(path))
        try:
            panes = server.live_panes_or_raise()
        except TmuxUnavailable:
            break  # no tmux here: no pane can be live
        except TmuxError as exc:
            found.append(f"rlx's tmux server {socket!r}, which did not answer ({exc})")
            continue
        own = _own_pane_on(server)
        count = sum(len(ids - {own}) for ids in panes.values())
        if count:
            windows = f"{count} live pane{'s' if count != 1 else ''}"
            found.append(f"{windows} on rlx's tmux server {socket!r}: close them")
    return found


def _own_pane_on(server: TmuxServer) -> str | None:
    """The pane this process runs in when it runs on ``server`` (``$TMUX`` names its socket),
    else ``None``: ``rlx upgrade`` typed in a window of rlx's own server is no pane to stop."""
    pane, socket = os.environ.get("TMUX_PANE"), os.environ.get("TMUX", "").split(",")[0]
    if not pane or not socket:
        return None
    try:
        same = os.path.realpath(socket) == os.path.realpath(server.socket_path())
    except Exception:
        return None
    return pane if same else None


# --- moving the paths a file holds -------------------------------------------------------------

_DELIMITER = r"""/\\\s'"`;,:()\[\]{}<>|"""


class _Mover:
    """Moves each path under the old home in a value to its place in the new: the root to the
    new root, and the name below it by :func:`paths.new_place`. Any other value is left as it
    is. Idempotent: no new place starts with an old name."""

    def __init__(
        self, spellings: Sequence[str], new_root: str, *, home_spelled: str | None
    ) -> None:
        self._to: dict[str, str] = {spelling: new_root for spelling in spellings}
        if home_spelled is not None:
            self._to[home_spelled] = "~/" + paths.HOME_NAME
        for spelling, root in list(self._to.items()):
            if "\\" in spelling:  # a Windows path, as JSON or TOML escape it
                self._to[spelling.replace("\\", "\\\\")] = root.replace("\\", "\\\\")
        roots = "|".join(re.escape(root) for root in sorted(self._to, key=len, reverse=True))
        self._pattern = re.compile(
            rf"(?P<root>{roots})(?:(?P<sep>[/\\]{{1,2}})(?P<first>[^{_DELIMITER}]+))?"
            rf"(?=$|[{_DELIMITER}])"
        )

    def text(self, value: str) -> str:
        return self._pattern.sub(self._moved, value)

    def _moved(self, found: re.Match[str]) -> str:
        root = self._to[found.group("root")]
        first = found.group("first")
        if first is None:
            return root
        sep = found.group("sep")
        return root + sep + sep.join(paths.new_place(first) or (first,))

    def value(self, value: object) -> object:
        """``value`` (JSON's: a dict, a list, a string, …) with every string moved, keys too."""
        if isinstance(value, str):
            return self.text(value)
        if isinstance(value, list):
            return [self.value(item) for item in value]
        if isinstance(value, dict):
            return {self.text(str(key)): self.value(item) for key, item in value.items()}
        return value

    def stored(self, value: str) -> str:
        """A stored value moved: walked as JSON when it is a JSON object or array, else as
        text. One with nothing to move comes back as it is."""
        if value.lstrip()[:1] in ("{", "["):
            try:
                loaded = json.loads(value)
            except ValueError:
                return self.text(value)
            moved = self.value(loaded)
            if moved == loaded:
                return value
            compact = '": ' not in value and '", "' not in value
            return json.dumps(moved, ensure_ascii=False, separators=(",", ":") if compact else None)
        return self.text(value)


def _mover_of(journal: Journal) -> _Mover:
    spelled = None if journal.named else "~/" + paths.OLD_HOME_NAME
    return _Mover(journal.spellings, journal.new_root, home_spelled=spelled)


def _home_spellings(root: Path) -> list[str]:
    found = [str(root)]
    with contextlib.suppress(OSError):
        resolved = os.path.realpath(root)
        if resolved not in found:
            found.append(resolved)
    if sys.platform == "win32":
        found += [spelling.replace("\\", "/") for spelling in list(found)]
    return found


# --- the move ----------------------------------------------------------------------------------


@dataclass
class Moved:
    """What a move did."""

    old_root: Path
    new_root: Path
    named: bool
    backup: Path
    """The store's copy from before the move, kept until the next release."""
    copied: list[str] = field(default_factory=list)
    """The Keychain logins copied, by account (``claude-code 2``)."""
    pinned: list[str] = field(default_factory=list)
    """The logins too long to copy, kept under their old names by a pin (``claude-code 4``)."""
    notes: list[str] = field(default_factory=list)
    left: list[str] = field(default_factory=list)
    """What still names the old home, left as written (``team_event.text: 3``)."""
    linked: bool = False
    cache_dropped: bool = False

    def as_json(self) -> dict[str, Any]:
        """The move as ``--json`` reports it (``rlx upgrade`` reads it back from the new
        install's ``move-home``)."""
        return {
            "moved": True,
            "old_home": str(self.old_root),
            "home": str(self.new_root),
            "in_place": self.named,
            "backup": str(self.backup),
            "keychain_copied": list(self.copied),
            "keychain_pinned": list(self.pinned),
            "linked": self.linked,
            "left": list(self.left),
            "notes": list(self.notes),
            "cache_dropped": self.cache_dropped,
            "downgrade": DOWNGRADE_GAP,
        }


@contextlib.contextmanager
def _one_at_a_time(old_root: str) -> Iterator[None]:
    """Hold this home's move lock, outside the home (its folders are renamed under it, and
    Windows renames no folder with a file open in it); refuse while another move holds it."""
    digest = hashlib.sha256(old_root.encode("utf-8", "surrogatepass")).hexdigest()[:16]
    lock = Path(tempfile.gettempdir()) / f"rlx-home-migration-{digest}.lock"
    try:
        with holding(lock, wait_s=0, mode=0o600):
            yield
    except TimeoutError:
        raise MigrationRefused("another move of this home is running") from None


def _names_the_default_home(environment: Mapping[str, str] | None = None) -> bool:
    """Whether ``RLX_HOME`` or ``AISQUARE_HOME`` names ``~/.aisquare`` itself: the fleet hands
    every window it opens the home by both names, resolved (``services.fleet``), and a shell
    started from one inherits them. Such a home is the default one, moved to ``~/.rlx`` and
    linked: laid out in place, the next rlx without the variable read ``~/.aisquare`` as an old
    home with no store, and made an empty one (the tester's blocker 3 on #31)."""
    named = paths.named_home(environment)
    return named is not None and paths._names_the_old_default(named, Path.home())


def _without_home_variables() -> dict[str, str]:
    """This process's environment without the two home variables: the default home's rule."""
    return {
        name: value
        for name, value in os.environ.items()
        if name not in (paths.RLX_HOME_ENV, paths.HOME_ENV_VAR)
    }


@contextlib.contextmanager
def _as_the_default_home() -> Iterator[None]:
    """For a move of the default home a variable names (:func:`_names_the_default_home`), the
    variables set aside while it runs, so every path ``core.paths`` builds follows the default
    home's rule (``~/.aisquare`` until its rename, ``~/.rlx`` after), and put back after."""
    if not _names_the_default_home():
        yield
        return
    kept = {
        name: os.environ.pop(name)
        for name in (paths.RLX_HOME_ENV, paths.HOME_ENV_VAR)
        if name in os.environ
    }
    try:
        yield
    finally:
        os.environ.update(kept)


def move_home(
    *, drop_cache: bool = False, say: Callable[[str], None] = lambda line: None
) -> Moved | None:
    """Move this process's home to the new layout, or resume a move a crash left: the steps
    of the module's docstring. ``None`` when the home needs no move. Raises
    :class:`MigrationRefused` (nothing touched) while the home is not quiet or cannot be moved,
    and :class:`MigrationFailed` when a step fails: rolled back before
    :data:`FORWARD_ONLY`, else left for the next run to finish."""
    with _as_the_default_home():
        return _move_home(drop_cache=drop_cache, say=say)


def _move_home(*, drop_cache: bool, say: Callable[[str], None]) -> Moved | None:
    """:func:`move_home`, the home's variables as the move reads them."""
    found = planned_move()
    if found is None:
        return None
    stopped = _stop_the_service()
    try:
        moved = _move_quiet_home(found, drop_cache=drop_cache, say=say)
    except (MigrationRefused, MigrationFailed) as exc:
        if stopped:
            exc.reason = f"{exc.reason}; {_start_the_service_again()}"
            exc.args = (exc.reason,)
        raise
    if stopped:
        moved.notes.append(_start_the_service_again())
    return moved


def _stop_the_service() -> bool:
    """Stop the rlx service (``services.service_host``) for the move, as a quit request does:
    whether one ran. It lives on the home the move renames, so the move stops it first and
    starts it again after (the owner's brief, item 4), on the home where the move left it.
    :class:`MigrationRefused` when it will not stop."""
    from aisquare.services import service_host

    try:
        return service_host.stop_service()
    except service_host.ServiceError as exc:
        raise MigrationRefused(f"{exc}: the home was not moved") from exc


def _start_the_service_again() -> str:
    """Start the rlx service :func:`_stop_the_service` stopped, by this install: what the move
    says of it."""
    from aisquare.services import service_host

    try:
        service = service_host.start_service()
    except service_host.ServiceError as exc:
        return f"the {service_host.SERVICE} did not start again: {exc}"
    return f"the {service_host.SERVICE} runs again (pid {service.pid})"


def _move_quiet_home(found: Plan, *, drop_cache: bool, say: Callable[[str], None]) -> Moved:
    """:func:`move_home` once the service is stopped: refused unless the home is quiet."""
    try:
        running = running_on_home()
    except Exception as exc:  # a home that cannot be told quiet is not moved
        raise MigrationRefused(
            f"whether anything runs on the home could not be told ({_why_stopped(exc)})"
        ) from exc
    if running:
        raise MigrationRefused(f"{len(running)} thing(s) run on this home", running)
    with _one_at_a_time(str(found.old_root)):
        journal = found.journal
        if journal is None:
            journal = Journal(
                old_root=str(found.old_root),
                new_root=str(found.new_root),
                named=found.named,
                spellings=_home_spellings(found.old_root),
                started_at=datetime.now(UTC),
            )
            try:
                _save_move_journal(journal)
            except OSError as exc:
                raise MigrationRefused(
                    f"its journal could not be written ({_why_stopped(exc)})"
                ) from exc
            say(f"moving the home: {found.shown}")
        elif journal.rolling_back:
            _roll_back_move(journal)
            raise MigrationFailed("a roll-back a crash stopped was finished", rolled_back=True)
        else:
            say(f"resuming the home's move: {found.shown}")
        try:
            _run_move_steps(journal, say)
        except MigrationRefused:
            raise
        except Exception as exc:
            if journal.forgetting:
                raise MigrationFailed(_why_stopped(exc), rolled_back=False) from exc
            say(f"the move stopped ({_why_stopped(exc)}): rolling it back")
            try:
                _roll_back_move(journal)
            except Exception as undo:
                raise MigrationFailed(
                    f"{_why_stopped(exc)}; and its roll-back stopped too "
                    f"({_why_stopped(undo)}): run it again to finish the roll-back",
                    rolled_back=False,
                ) from undo
            raise MigrationFailed(_why_stopped(exc), rolled_back=True) from exc
        moved = Moved(
            Path(journal.old_root),
            Path(journal.new_root),
            journal.named,
            _move_folder(journal.root) / paths.OLD_STORE,
            copied=[
                f"{item.agent} {item.slot}"
                for item in journal.items
                if item.state in ("copied", "forgotten")
            ],
            pinned=[
                f"{item.agent} {item.slot}" for item in journal.items if item.state == "pinned"
            ],
            notes=list(journal.notes),
            left=list(journal.left),
            linked=journal.linked,
        )
        if drop_cache:
            try:
                _drop_the_cache()
                moved.cache_dropped = True
            except MigrationRefused as exc:
                moved.notes.append(f"the cache was not dropped: {exc.reason}")
        return moved


def _why_stopped(exc: BaseException) -> str:
    if isinstance(exc, MigrationFailed):
        return exc.reason
    if isinstance(exc, OSError):
        return f"{exc.filename or ''}: {exc.strerror or exc}".lstrip(": ")
    return str(exc) or type(exc).__name__


_MOVE_STEPS: dict[str, Callable[[Journal], None]] = {}


def _move_step(name: str) -> Callable[[Callable[[Journal], None]], Callable[[Journal], None]]:
    def register(function: Callable[[Journal], None]) -> Callable[[Journal], None]:
        _MOVE_STEPS[name] = function
        return function

    return register


def _run_move_steps(journal: Journal, say: Callable[[str], None]) -> None:
    for name in STEPS:
        if name in journal.steps:
            continue
        if name != "done":
            _MOVE_STEPS[name](journal)
        journal.steps.append(name)
        _save_move_journal(journal)


@_move_step("backup")
def _back_up_store(journal: Journal) -> None:
    """The store copied by SQLite's backup API, after a checkpoint, into the journal's folder;
    and what the check after the rewrite compares with."""
    root = journal.root
    store = root / paths.OLD_STORE
    copy = _move_folder(root) / paths.OLD_STORE
    copy.parent.mkdir(parents=True, exist_ok=True)
    if store.is_file():
        with contextlib.closing(sqlite3.connect(str(store), timeout=5.0)) as source:
            source.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            with contextlib.closing(sqlite3.connect(str(copy))) as target:
                source.backup(target)
                verdict = target.execute("PRAGMA integrity_check").fetchone()
        if verdict is None or verdict[0] != "ok":
            raise MigrationFailed("the store's backup did not read back whole", rolled_back=False)
    journal.accepted = _replayable_rows()
    journal.signed_in = _signed_in_slots()


@_move_step("keychain")
def _copy_logins(journal: Journal) -> None:
    """Each managed slot's Keychain item copied to the name its new place hashes to, as Claude
    Code writes one, and read back by its own lookup (the manager's condition 8 on #31)."""
    if not journal.items:
        journal.items = _logins_to_copy(journal)
        _save_move_journal(journal)
    for item in journal.items:
        if item.state in ("copied", "absent", "pinned"):
            continue
        old = accounts_core.keychain_item_data(item.old, item.account)
        if old.unreadable is not None:
            raise MigrationFailed(
                f"the login of {item.agent} account {item.slot} could not be read: "
                f"{old.unreadable}",
                rolled_back=False,
            )
        if old.data is None:
            item.state = "absent"
            _save_move_journal(journal)
            continue
        if item.state == "planned" and not accounts_core.keychain_copy_fits(
            item.new, item.account, old.data
        ):
            # Too long for `security -i`'s line, and never put on a command line: the slot keeps
            # its old item's name instead (the owner's brief's alternative, #2071's note).
            _pin_the_slot(item, journal)
            continue
        if item.state == "planned":
            there = accounts_core.keychain_item_data(item.new, item.account)
            if there.data is not None:
                if there.printed != old.printed:
                    raise MigrationFailed(
                        f"the Keychain already holds another login under {item.new!r}",
                        rolled_back=False,
                    )
                item.existed = True
            item.state = "copying"
            _save_move_journal(journal)
        if not item.existed:
            why = accounts_core.add_keychain_item(item.new, item.account, old.data)
            if why is not None:
                raise MigrationFailed(
                    f"the login of {item.agent} account {item.slot} was not copied: {why}",
                    rolled_back=False,
                )
        copy = accounts_core.keychain_item_data(item.new, item.account)
        if copy.data is None or copy.printed != old.printed:
            raise MigrationFailed(
                f"the copy of {item.agent} account {item.slot}'s login read back differently",
                rolled_back=False,
            )
        item.state = "copied"
        _save_move_journal(journal)
    accounts_core.forget_keychain_reads()


def _the_slot(item: Copied) -> ClaudeAccount:
    """``item``'s account where the home keeps it now, in whichever layout it is in."""
    return accounts_core._managed(item.slot, item.agent)


def _pin_the_slot(item: Copied, journal: Journal) -> None:
    """Pin ``item``'s slot to the directory its item is named by now, its raw
    ``CLAUDE_CONFIG_DIR`` before the move (``core.claude_accounts.pin_keychain_dir``), so after
    the move Claude Code and rlx still name the old item; nothing is copied, nothing deleted."""
    account = _the_slot(item)
    raw = accounts_core.launch_env(account)[accounts_core.CONFIG_DIR_VAR]
    accounts_core.pin_keychain_dir(account, raw)
    item.state = "pinned"
    _save_move_journal(journal)


def _logins_to_copy(journal: Journal) -> list[Copied]:
    """The Keychain items the move copies: each managed slot whose item is named by its config
    dir (Claude Code's), on macOS, under ``CLAUDE_SECURESTORAGE_CONFIG_DIR`` none (it names
    the item then, wherever the slot is)."""
    if not accounts_core.keychain_platform():
        return []
    mover = _mover_of(journal)
    account_name = accounts_core.keychain_account()
    found: list[Copied] = []
    for account in accounts_core.all_managed_accounts():
        old = accounts_core.keychain_item(account)
        if old is None:
            continue
        there = Path(mover.text(str(account.config_dir)))
        new = accounts_core.keychain_item(account.model_copy(update={"config_dir": there}))
        if new is None or new == old:
            continue
        found.append(
            Copied(agent=account.agent, slot=account.slot, account=account_name, old=old, new=new)
        )
    return found


def _planned_renames(root: Path) -> list[Rename]:
    """Each old name at ``root``'s top and its new place, the store last of all: in a named home,
    its rename is the one that makes the home read as new (``paths.layout_of``), its sidecars
    just before it."""
    found = [
        (name, place) for name in os.listdir(root) if (place := paths.new_place(name)) is not None
    ]
    found.sort(
        key=lambda entry: (
            entry[0] == paths.OLD_STORE,
            entry[0].startswith(paths.OLD_STORE),
            entry[0],
        )
    )
    return [Rename(source=[name], target=list(place)) for name, place in found]


def _rename_once(source: Path, target: Path) -> None:
    """One rename of the move (a seam the tests crash between)."""
    os.rename(source, target)


@_move_step("move")
def _rename_names(journal: Journal) -> None:
    """Each old name renamed to its new place, the store last; the default home's root then
    renamed to ``~/.rlx``. Each written down before it is made, and found made on a resume."""
    root = Path(journal.old_root)
    if not journal.renames_planned and not journal.flipped:
        journal.renames = _planned_renames(root)
        journal.renames_planned = True
        _save_move_journal(journal)
    for rename in journal.renames:
        if rename.done or journal.flipped:
            continue
        source, target = root.joinpath(*rename.source), root.joinpath(*rename.target)
        if not os.path.lexists(source):
            # Made before a crash, or nothing there now (a lock let go, a sidecar the
            # checkpoint took).
            rename.done = os.path.lexists(target)
            rename.skipped = not rename.done
            _save_move_journal(journal)
            continue
        for depth in range(1, len(rename.target)):
            parent = root.joinpath(*rename.target[:depth])
            if not parent.exists():
                journal.made.append(rename.target[:depth])
                _save_move_journal(journal)
                parent.mkdir()
        if _an_empty_folder(target):
            target.rmdir()
        _rename_once(source, target)
        rename.done = True
        _save_move_journal(journal)
    if not journal.named and not journal.flipped:
        new_root = Path(journal.new_root)
        if os.path.lexists(new_root) and not os.path.lexists(root):
            journal.flipped = True  # made before a crash
        else:
            _rename_once(root, new_root)
            journal.flipped = True
        _save_move_journal(journal)


@_move_step("rewrite")
def _rewrite_paths(journal: Journal) -> None:
    """The paths the store holds, in one transaction; the home's own files; and every launch
    record, re-issued by the seal code with its paths moved."""
    mover = _mover_of(journal)
    store = paths.db_path()
    if store.is_file():
        connection = sqlite3.connect(str(store), timeout=5.0)
        with contextlib.closing(connection), connection:  # one transaction, then closed
            _move_store_paths(connection, mover, journal.spellings)
            journal.left = _paths_left_in(connection, mover, journal.spellings)
    for place in HOME_FILES:
        _move_file_paths(journal, place(), mover)
    seals = paths.launch_seals_dir()
    if seals.is_dir():
        _copy_aside(journal, seals, tree=True)
        failed, skipped = launch_seal.reissue(mover.value)
        if failed:
            raise MigrationFailed(
                f"the launch records were not re-issued: {'; '.join(failed)}", rolled_back=False
            )
        journal.notes += [f"a launch record left as written ({one})" for one in skipped]


def _text_columns(connection: sqlite3.Connection) -> dict[str, list[str]]:
    tables = [
        name
        for (name,) in connection.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND sql NOT LIKE "
            "'CREATE VIRTUAL TABLE%' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '%_fts%'"
        )
    ]
    found: dict[str, list[str]] = {}
    for table in tables:
        found[table] = [
            row[1]
            for row in connection.execute(f'PRAGMA table_info("{table}")')
            if (row[2] or "").upper() in ("TEXT", "")
        ]
    return found


def _rows_naming(
    connection: sqlite3.Connection, table: str, column: str, spellings: Sequence[str]
) -> list[tuple[int, str]]:
    where = " OR ".join(f'instr("{column}", ?) > 0' for _ in spellings)
    return [
        (rowid, value)
        for rowid, value in connection.execute(
            f'SELECT rowid, "{column}" FROM "{table}" WHERE {where}', tuple(spellings)
        )
        if isinstance(value, str)
    ]


def _move_store_paths(
    connection: sqlite3.Connection, mover: _Mover, spellings: Sequence[str]
) -> None:
    columns = _text_columns(connection)
    for table, column in STORE_COLUMNS:
        if column not in columns.get(table, []):
            continue
        for rowid, value in _rows_naming(connection, table, column, spellings):
            moved = mover.stored(value)
            if moved != value:
                connection.execute(
                    f'UPDATE "{table}" SET "{column}" = ? WHERE rowid = ?', (moved, rowid)
                )


def _paths_left_in(
    connection: sqlite3.Connection, mover: _Mover, spellings: Sequence[str]
) -> list[str]:
    """Each column outside :data:`STORE_COLUMNS` that still names a path under the old home,
    with how many rows: prose, left as written."""
    rewritten = set(STORE_COLUMNS)
    left: list[str] = []
    for table, names in _text_columns(connection).items():
        for column in names:
            if (table, column) in rewritten:
                continue
            count = sum(
                1
                for _rowid, value in _rows_naming(connection, table, column, spellings)
                if mover.stored(value) != value
            )
            if count:
                left.append(f"{table}.{column}: {count}")
    return left


SLOT_FILE_LIMIT = 8 * 1024 * 1024
"""The largest file at a slot's top the hook step copies aside: settings and trust records are
small; a transcript or a history is never one a refresh writes."""


def _slot_files(directory: Path) -> set[Path]:
    """The regular files at ``directory``'s top (no link followed) up to :data:`SLOT_FILE_LIMIT`."""
    found: set[Path] = set()
    try:
        entries = list(os.scandir(directory))
    except OSError:
        return found
    for entry in entries:
        try:
            if entry.is_file(follow_symlinks=False) and entry.stat().st_size <= SLOT_FILE_LIMIT:
                found.add(Path(entry.path))
        except OSError:
            continue
    return found


def _made_by_the_move(journal: Journal, made: set[Path]) -> None:
    """Record each file in ``made`` as one the move created: a roll-back removes it."""
    root = journal.root
    for path in sorted(made):
        place = list(path.relative_to(root).parts)
        if any(aside.place == place for aside in journal.aside):
            continue
        journal.aside.append(Aside(place=place, copy_at=[], created=True))
    if made:
        _save_move_journal(journal)


def _copy_aside(journal: Journal, path: Path, *, tree: bool = False) -> None:
    """Copy ``path`` aside before it is rewritten, once: a roll-back puts it back."""
    root = journal.root
    place = list(path.relative_to(root).parts)
    if any(aside.place == place for aside in journal.aside):
        return
    copy_at = [*FOLDER, "files", *place]
    copy = root.joinpath(*copy_at)
    copy.parent.mkdir(parents=True, exist_ok=True)
    if tree:
        if copy.exists():
            shutil.rmtree(copy)
        shutil.copytree(path, copy, symlinks=True)
    else:
        shutil.copy2(path, copy)
    journal.aside.append(Aside(place=place, copy_at=copy_at, tree=tree))
    _save_move_journal(journal)


def _move_file_paths(journal: Journal, path: Path, mover: _Mover) -> None:
    """``path``'s paths under the old home moved, the file copied aside first; a JSON file
    walked, any other as text."""
    try:
        body = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return
    moved = mover.stored(body) if path.suffix == ".json" else mover.text(body)
    if moved == body:
        return
    _copy_aside(journal, path)
    write_replacing(
        path, moved if moved.endswith("\n") or not body.endswith("\n") else moved + "\n"
    )


@_move_step("check")
def _check_the_move(journal: Journal) -> None:
    """The fleet rows whose replay the fleet's own check allows are the same rows after the move
    as before it, every one, and every account signed in before is signed in now: else the move
    rolls back whole. The same, not merely kept: a row the check refused before and allows
    after was let through by the move, which loosens the seal check (the manager's condition 4
    on #31: never loosen it)."""
    before, after = set(journal.accepted), set(_replayable_rows())
    if before != after:
        lost, gained = sorted(before - after), sorted(after - before)
        said = []
        if lost:
            said.append(f"{', '.join(lost)} could no longer be restarted from its launch record")
        if gained:
            said.append(f"{', '.join(gained)}, refused before, would be restarted after")
        raise MigrationFailed(
            f"after the rewrite, the fleet's seal check changed its answer: {'; '.join(said)}",
            rolled_back=False,
        )
    accounts_core.forget_keychain_reads()
    signed_out = sorted(set(journal.signed_in) - set(_signed_in_slots()))
    if signed_out:
        raise MigrationFailed(
            f"after the move, {', '.join(signed_out)} read as signed out", rolled_back=False
        )


def _replayable_rows() -> list[str]:
    """The fleet rows of every project whose replay the fleet allows from their launch records
    now (``services.fleet._replay_refusal``, the check every restart, switch and hand-over
    passes), the home their agent ran under found where the record says."""
    from aisquare.core.store import store_session
    from aisquare.services import fleet as fleet_service

    if not paths.db_path().is_file():
        return []
    accepted: list[str] = []
    with store_session() as store:
        for project in store.list_projects(all=True, include_forgotten=True):
            for row in store.fleet_agents(project.id):
                record = launch_seal.read(row.id)
                if isinstance(record, str) or fleet_service._replay_refusal(project, record):
                    continue
                home = (record.session.home if record.session else None) or record.account_home
                if home and not Path(home).is_dir():
                    continue
                accepted.append(row.id)
    return sorted(accepted)


def _signed_in_slots() -> list[str]:
    return sorted(
        f"{account.agent}:{account.slot}"
        for account in accounts_core.all_managed_accounts()
        if accounts_core.signed_in(account, fresh=True)
    )


@_move_step("hooks")
def _write_hooks_again(journal: Journal) -> None:
    """Each managed account's hooks written again for its new place, where it holds rlx's:
    ``agents refresh-hooks``, which also records Codex's trust of what it writes; else the
    paths under the old home in its settings moved (the manager's condition 5 on #31)."""
    from aisquare.core import agents as agent_core
    from aisquare.services import agents as agents_service

    mover = _mover_of(journal)
    for account in accounts_core.all_managed_accounts():
        spec = agent_core.spec(account.agent, account.config_dir)
        settings = spec.settings_path if spec is not None else None
        if settings is None or not settings.is_file():
            continue
        # What a refresh may write, all of it set aside first: the settings, and beside them
        # Codex's trust of the hooks (its config.toml's [hooks.state], its trust record) and
        # agents.json's record of what was connected. Before, only the settings were, and a
        # roll-back left the trust of the new hooks over the old (the tester's blocker 1).
        before = _slot_files(account.config_dir)
        for path in sorted(before):
            _copy_aside(journal, path)
        registry = paths.agents_registry_path()
        if registry.is_file():
            _copy_aside(journal, registry)
        try:
            if agent_core.hook_commands(account.agent, account.config_dir):
                try:
                    agents_service.refresh_hooks(account.agent, account.config_dir)
                    continue
                except (KeyError, ValueError, OSError) as exc:
                    journal.notes.append(
                        f"{account.agent} account {account.slot}'s hooks were not written again "
                        f"({_why_stopped(exc)}): their paths were moved instead"
                    )
            _move_file_paths(journal, settings, mover)
        finally:
            _made_by_the_move(journal, _slot_files(account.config_dir) - before)


@_move_step("link")
def _link_old_home(journal: Journal) -> None:
    """``~/.aisquare`` left as a link to ``~/.rlx`` for one release (a junction on Windows,
    which makes a symbolic link only with Developer Mode on)."""
    if journal.named:
        return
    old, new = Path(journal.old_root), Path(journal.new_root)
    if os.path.lexists(old) and (old.is_symlink() or _a_junction(old)):
        journal.linked = True
        return
    if os.path.lexists(old):
        # Made again between the root's rename and this step (an asq-statusline under the
        # variable, say): kept aside, never deleted, and the link made (the tester's N2).
        aside = old.with_name(f"{old.name}-made-during-the-move-{datetime.now(UTC):%Y%m%dT%H%M%S}")
        _rename_once(old, aside)
        journal.notes.append(
            f"{paths.shown(old)} was made again during the move: what it held is kept at "
            f"{paths.shown(aside)}"
        )
    try:
        os.symlink(new, old, target_is_directory=True)
    except OSError:
        if sys.platform != "win32" or not _make_junction(new, old):
            journal.notes.append(
                f"{paths.shown(old)} could not be left as a link to {paths.shown(new)}: "
                "a stale reference to it no longer finds the home"
            )
            return
    journal.linked = True


def _a_junction(path: Path) -> bool:
    is_junction = getattr(os.path, "isjunction", None)
    return bool(is_junction(path)) if is_junction is not None else False


def _make_junction(target: Path, link: Path) -> bool:
    try:
        import _winapi  # type: ignore[import-not-found,unused-ignore]

        create: Callable[[str, str], None] = getattr(_winapi, "CreateJunction")  # noqa: B009
        create(str(target), str(link))
    except (ImportError, AttributeError, OSError):
        return False
    return True


@_move_step("forget")
def _forget_old_logins(journal: Journal) -> None:
    """The old Keychain items deleted, now that every copy read back and the move checked out:
    forward-only from here."""
    journal.forgetting = True
    _save_move_journal(journal)
    for item in journal.items:
        if item.state != "copied":
            continue
        why = accounts_core.delete_keychain_item(item.old, item.account)
        if why is not None:
            journal.notes.append(
                f"the old Keychain item {item.old!r} was not deleted ({why}): delete it in "
                "Keychain Access"
            )
        item.state = "forgotten"
        _save_move_journal(journal)
    accounts_core.forget_keychain_reads()


# --- rolling back ------------------------------------------------------------------------------


def roll_back_move(journal: Journal | None = None) -> bool:
    """Put the home back as it was before an unfinished move, every step undone in reverse:
    whether there was one to undo. Refused (:class:`MigrationFailed`) once the old Keychain
    items began to go (:data:`FORWARD_ONLY`). Each undo is found done on a re-run, so a crash
    here is finished by the next run."""
    with _as_the_default_home():
        if journal is not None:
            return _roll_back_move(journal)
        found = planned_move()
        if found is None or found.journal is None:
            return False
        try:
            running = running_on_home()
        except Exception as exc:
            raise MigrationRefused(
                f"whether anything runs on the home could not be told ({_why_stopped(exc)})"
            ) from exc
        if running:
            raise MigrationRefused(f"{len(running)} thing(s) run on this home", running)
        with _one_at_a_time(str(found.old_root)):
            return _roll_back_move(found.journal)


def _roll_back_move(journal: Journal | None = None) -> bool:
    """:func:`roll_back_move`, the home's variables as the move reads them."""
    if journal is None:
        found = planned_move()
        journal = found.journal if found is not None else None
        if journal is None:
            return False
    if journal.forgetting:
        raise MigrationFailed(
            "the move deleted the old Keychain items already: it can only be finished",
            rolled_back=False,
        )
    if not journal.rolling_back:
        journal.rolling_back = True
        # Which rename a crash may have stopped between making it and writing it down: read
        # once, as the forward run left the journal, never again from a half-reversed one.
        journal.in_flight = next(
            (at for at, r in enumerate(journal.renames) if not r.done and not r.skipped), None
        )
        _save_move_journal(journal)
    for phase in UNDO_PHASES:
        if phase in journal.undone:
            continue
        _UNDO[phase](journal)
        if phase == "folder":
            return True  # the journal went with the folder: the home is as it was
        journal.undone.append(phase)
        _save_move_journal(journal)
    return True


def _undo_link(journal: Journal) -> None:
    """The link ``~/.aisquare`` → ``~/.rlx`` removed, when the move made it."""
    old_root = Path(journal.old_root)
    if journal.linked or (not journal.named and journal.flipped and old_root.is_symlink()):
        with contextlib.suppress(FileNotFoundError):
            old_root.unlink()
        journal.linked = False


def _undo_aside(journal: Journal) -> None:
    """Each file the move rewrote put back from its copy, and each it made removed, at the
    places they had when it did (the new layout's: the renames are reversed after), each one
    written down as it is done."""
    root = journal.root
    for aside in reversed(journal.aside):
        if aside.restored:
            continue
        place = root.joinpath(*aside.place)
        if aside.created:
            with contextlib.suppress(FileNotFoundError):
                place.unlink()
        else:
            copy = root.joinpath(*aside.copy_at)
            if os.path.lexists(copy):
                if aside.tree:
                    shutil.rmtree(place, ignore_errors=True)
                    shutil.copytree(copy, place, symlinks=True)
                else:
                    shutil.copy2(copy, place)
        aside.restored = True
        _save_move_journal(journal)


def _undo_store(journal: Journal) -> None:
    _restore_the_store(journal)


def _undo_root(journal: Journal) -> None:
    """The default home's root renamed back to ``~/.aisquare``; the journal goes with it."""
    old_root, new_root = Path(journal.old_root), Path(journal.new_root)
    if journal.flipped:
        if os.path.lexists(new_root) and not os.path.lexists(old_root):
            _rename_once(new_root, old_root)
        journal.flipped = False


def _undo_renames(journal: Journal) -> None:
    """Each rename made, and the one in flight, reversed where its target is and its source is
    not, each written down as it is."""
    old_root = Path(journal.old_root)
    for at in range(len(journal.renames) - 1, -1, -1):
        rename = journal.renames[at]
        if rename.undone or not (rename.done or at == journal.in_flight):
            continue
        source, target = old_root.joinpath(*rename.source), old_root.joinpath(*rename.target)
        if os.path.lexists(target) and not os.path.lexists(source):
            _rename_once(target, source)
        rename.undone = True
        _save_move_journal(journal)


def _undo_made(journal: Journal) -> None:
    """The folders the renames made for their targets, removed while they are empty."""
    old_root = Path(journal.old_root)
    for parts in reversed(journal.made):
        with contextlib.suppress(OSError):
            old_root.joinpath(*parts).rmdir()


def _undo_keychain(journal: Journal) -> None:
    """Each Keychain copy deleted, and each pin taken away (its marker back at its old place)."""
    for item in journal.items:
        if item.state in ("copying", "copied") and not item.existed:
            why = accounts_core.delete_keychain_item(item.new, item.account)
            if why is not None:
                raise MigrationFailed(
                    f"the copy {item.new!r} was not deleted ({why})", rolled_back=False
                )
        elif item.state == "pinned":
            accounts_core.pin_keychain_dir(_the_slot(item), None)
        item.state = "planned"
        _save_move_journal(journal)
    accounts_core.forget_keychain_reads()


def _undo_folder(journal: Journal) -> None:
    """The move's own folder removed, its journal last: until then a crash finishes this."""
    folder = _move_folder(Path(journal.old_root))
    record = _move_journal_path(Path(journal.old_root))
    for entry in list(folder.iterdir()) if folder.is_dir() else []:
        if entry == record:
            continue
        if entry.is_dir() and not entry.is_symlink():
            shutil.rmtree(entry, ignore_errors=True)
        else:
            with contextlib.suppress(FileNotFoundError):
                entry.unlink()
    with contextlib.suppress(FileNotFoundError):
        record.unlink()
    with contextlib.suppress(OSError):
        folder.rmdir()
    with contextlib.suppress(OSError):
        folder.parent.rmdir()  # backups/, when the move made it


_UNDO: dict[str, Callable[[Journal], None]] = {
    "link": _undo_link,
    "aside": _undo_aside,
    "store": _undo_store,
    "root": _undo_root,
    "renames": _undo_renames,
    "made": _undo_made,
    "keychain": _undo_keychain,
    "folder": _undo_folder,
}


def _restore_the_store(journal: Journal) -> None:
    """The store, wherever the move left it, given back what its backup holds."""
    copy = _move_folder(journal.root) / paths.OLD_STORE
    if not copy.is_file():
        return
    root = journal.root
    places: list[tuple[str, ...]] = [(paths.OLD_STORE,)]
    places += [tuple(r.target) for r in journal.renames if r.source == [paths.OLD_STORE]]
    for parts in places:
        store = root.joinpath(*parts)
        if store.is_file():
            with (
                contextlib.closing(sqlite3.connect(str(copy))) as source,
                contextlib.closing(sqlite3.connect(str(store), timeout=5.0)) as target,
            ):
                source.backup(target)
            return


def cache_size() -> int:
    """How many bytes this home's cache holds (``paths.cache_dir``): regenerable, so the move
    offers to drop it (the owner's brief, must-handle 5). Links are not followed."""
    total = 0
    for directory, _dirs, files in os.walk(paths.cache_dir()):
        for name in files:
            with contextlib.suppress(OSError):
                total += os.lstat(os.path.join(directory, name)).st_size
    return total


def shown_size(size: int) -> str:
    """``size`` bytes as a person reads it: ``5.6 GB``, ``340 MB``, ``12 KB``."""
    value = float(size)
    for unit in ("bytes", "KB", "MB", "GB"):
        if value < 1000 or unit == "GB":
            return f"{value:.0f} {unit}" if unit in ("bytes", "KB") else f"{value:.1f} {unit}"
        value /= 1000
    return f"{value:.1f} TB"


def drop_cache() -> None:
    """:func:`_drop_the_cache`, for the commands: ``rlx move-home --drop-cache`` and the offer
    after a move (``rlx upgrade``)."""
    _drop_the_cache()


def _drop_the_cache() -> None:
    """This home's cache removed, only while nothing runs on the home: it holds each managed
    slot's ``CLAUDE_CODE_TMPDIR`` (a live session's scratch, a background task's output, the
    worktrees agents keep there), so it is dropped as the move is made, the rlx service stopped
    first and started again after (the tester's blocker 2, round 4). Raises
    :class:`MigrationRefused`, naming what runs, and drops nothing then."""
    stopped = _stop_the_service()
    try:
        try:
            running = running_on_home()
        except Exception as exc:
            raise MigrationRefused(
                f"whether anything runs on the home could not be told ({_why_stopped(exc)})"
            ) from exc
        if running:
            raise MigrationRefused(
                f"the cache holds the scratch of what runs ({len(running)} thing(s))", running
            )
        shutil.rmtree(paths.cache_dir(), ignore_errors=True)
    finally:
        if stopped:
            _start_the_service_again()


def said_of_move(moved: Moved) -> list[str]:
    """What a move did, line by line, for a person (``rlx upgrade``, ``rlx move-home``)."""
    lines = [f"the home moved: {paths.shown(moved.old_root)} → {paths.shown(moved.new_root)}"]
    if moved.named:
        lines = [f"the home was laid out anew, in place: {paths.shown(moved.new_root)}"]
    if moved.copied:
        lines.append(f"Keychain logins copied to their new place: {', '.join(moved.copied)}")
    if moved.pinned:
        lines.append(
            "Keychain logins too long to copy without a command line, kept under their old "
            f"names (CLAUDE_SECURESTORAGE_CONFIG_DIR pinned for them): {', '.join(moved.pinned)}"
        )
    if moved.linked:
        lines.append(
            f"{paths.shown(moved.old_root)} is a link to {paths.shown(moved.new_root)} until "
            "the next release"
        )
    lines.append(
        f"the store as it was before is kept at {paths.shown(moved.backup)} until the next release"
    )
    if moved.left:
        lines.append(
            "left as written, naming the old home (notes and prose): " + ", ".join(moved.left)
        )
    if moved.cache_dropped:
        lines.append("the cache was dropped: rlx makes it again as it needs it")
    lines += moved.notes
    lines.append(DOWNGRADE_GAP)
    return lines
