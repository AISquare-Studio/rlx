"""``rlx tidy``: free the disk rlx's ended agents and sessions left, by rules that know no language.

The owner's disk tidy (#58, brief ``disk-tidy.md``): 82 GB of one Mac was rlx leftovers, ended
reviewers' scratch checkouts with their build output, trees reap kept for an untracked
``target/``, and lock files. Every rule is here, and each decides from what rlx already knows or
a standard says, never from a directory's name:

- **liveness**, which rlx owns: fleet rows (live, paused, ended), the processes running, board
  sessions and their transcripts;
- **git's own truth**: a checkout goes only when every commit in it is reachable from a ref,
  and changed or untracked work is never thrown away (reap's ``_why_kept``);
- **the Cache Directory Tagging spec** (``core.cachedir``): a tagged directory is regenerable.

The rules (:data:`RULES`):

**rlx tidy never deletes a git repository** (manager #3585, for 0.10.2): every round of review
found a new way whole-checkout deletion could lose commits, while most of the space is
regenerable build output inside the checkouts. Deleting scratch checkouts is 0.10.3's, behind an
explicit confirmation per path.

1. **Ended agents' worktrees**: reap's worktree half, the trees ``fleet.ended_trees`` chooses,
   each judged by ``fleet._why_kept`` and removed by ``fleet._reap_tree`` under reap's branch
   lock. A tree's untracked ``CACHEDIR.TAG`` output keeps it no longer; it is deleted first,
   kept tree or not. A tree that holds any repository but its own is kept and named.
2. **Ended sessions' temp trees**, under the ``CLAUDE_CODE_TMPDIR`` rlx gives each managed
   Claude slot (``claude-<uid>/<project>/<session id>/``; slot 1 and Codex have none of rlx's).
   A session is ENDED on CURRENT positive evidence alone: its board row's ``ended_at``, set and
   not cleared by a resume; every row naming it ended (``fleet-held:`` ids included) and none
   made after; no live or paused row working in its directory; no running process naming it;
   and nothing of it moved for :data:`ENDED_IDLE`. Then only its caches go: ``CACHEDIR.TAG``
   folders strictly inside it, holding no repository at any depth, in no tag-at-a-root, and
   tracking nothing in a checkout around them. Every repository in it is KEPT and listed with
   its size. The whole directory goes only once it ended :data:`EVIDENCE_TTL` ago and holds no
   repository at all. The per-agent ``TMPDIR`` is #57's, which adds its rule here (#3494).
3. **Idle live coders' build output**: a live coder whose board session has waited
   ``[tidy] idle_build_hours`` with no task in doing loses the tagged directories in its
   worktree, never one that is a PEP 405 environment (``core.cachedir.is_environment``: the one
   standard-marker exception, since a live agent runs its commands from it).
4. **Stale home-migration locks** in the system's temp directory, found whatever this
   process's ``TMPDIR`` is (each agent has its own): ``rlx-home-migration-<16 hex>.lock``,
   this user's, untouched for :data:`LOCK_AGE`, and free (taken at once, then unlinked while
   held).

**Never touched:** anything outside rlx-owned roots (each project's ``.aisquare-worktrees``,
the accounts' temp root, the agents' temp root, and those lock names in the system temp
directory); a project's own checkout; ``~/.claude``; a paused agent's anything. Every delete
goes through :func:`_remove`, which refuses a path outside the roots :func:`plan` computed, the
roots themselves, and any symlink, whose target is never followed.

:func:`plan` decides and reads only; :func:`apply` carries out exactly the plan's items, each
re-checked just before (still there, still in its root, its session or agent still ended),
one that no longer holds skipped and said so, never swapped for another.
"""

from __future__ import annotations

import contextlib
import json
import os
import re
import shutil
import stat
import sys
import uuid
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

from aisquare.core import atomic, cachedir, claude_accounts, locking, paths, process_table
from aisquare.core import program as prog
from aisquare.core.config import TidySettings, load_config
from aisquare.core.store import FLEET_HELD, ContextStore, store_session
from aisquare.models import FleetAgent, ProjectInfo
from aisquare.services import fleet as fleet_service

ENDED_WORKTREE = "ended worktree"
ENDED_WORKTREE_CACHE = "ended worktree cache"
SESSION_CHECKOUT = "session checkout"
SESSION_CACHE = "session cache"
SESSION_DIR = "session dir"
IDLE_BUILD_CACHE = "idle build cache"
MIGRATION_LOCK = "migration lock"
RULES = (
    ENDED_WORKTREE,
    ENDED_WORKTREE_CACHE,
    SESSION_CHECKOUT,
    SESSION_CACHE,
    SESSION_DIR,
    IDLE_BUILD_CACHE,
    MIGRATION_LOCK,
)
"""Every rule, in the order a plan lists them."""

EXPLICIT_ENDS = ("stopped", "died")
"""How a fleet row ends on rlx's own word (``FleetAgent.ended_by``): the only ends tidy takes for
a session's end (review of #58, M2)."""
ENDED_IDLE = timedelta(hours=2)
"""How long nothing of a session or an agent's temp may have moved before it is ended."""
EVIDENCE_TTL = timedelta(days=7)
"""How long an ended session's small files (logs, notes) are kept before its directory goes."""
LOCK_AGE = timedelta(hours=24)
"""How old a home-migration lock must be before it is taken for left behind."""
LOCK_NAME = re.compile(r"rlx-home-migration-[0-9a-f]{16}\.lock")
WORKTREES_DIR = ".aisquare-worktrees"
"""The folder under each project's root where the fleet makes its agents' trees."""
_DARWIN_USER_TEMP_DIR = 65537
"""``_CS_DARWIN_USER_TEMP_DIR``, which ``os.confstr`` takes by number on macOS."""


def _now() -> datetime:
    return datetime.now(UTC)


def _settings() -> TidySettings:
    """``[tidy]``; with a config that cannot be read, the coder rule and the periodic job OFF
    (review of #58): a rule that deletes fails closed."""
    try:
        return load_config().tidy
    except Exception:
        return TidySettings(idle_build_hours=0, every_minutes=0)


ROOT_TAG = (
    "a CACHEDIR.TAG at the root of what tidy judges authorises nothing (CARGO_TARGET_DIR or "
    "UV_CACHE_DIR there, pytest -o cache_dir=.): ignored, git alone judges it"
)
"""What a plan says of the tags it ignores (review of #58, C2): :attr:`Plan.ignored`."""
NEVER_A_REPOSITORY = "rlx won't delete a repository: remove it by hand if you don't need it"
"""Why every repository in a session's temp is kept (manager #3585: ``rlx tidy`` never deletes
a git repository in 0.10.2)."""
DELETES = sys.platform != "win32"
"""Whether tidy deletes here. Not on native Windows in 0.10.2 (manager #3647): it reports and
lists, and removes nothing; the fleet there runs in WSL, where tidy deletes."""
REPORTS_ONLY = (
    "tidy only reports on native Windows in 0.10.2: the fleet runs in WSL, so run it there"
)
"""What a report on native Windows says of each item it would free (:data:`DELETES`)."""


# --- what may be deleted -------------------------------------------------------------------


class Refused(Exception):
    """A delete :func:`_remove` would not do: the path is outside rlx's roots, or a link."""


def _real(path: Path) -> Path | None:
    try:
        return path.resolve()
    except (OSError, RuntimeError):
        return None


@dataclass(frozen=True)
class Roots:
    """Where :func:`_remove` may delete, resolved when the plan was made."""

    trees: frozenset[Path] = frozenset()
    """Each project's ``.aisquare-worktrees``."""
    temp: frozenset[Path] = frozenset()
    """The accounts' temp root and the agents' temp root."""
    locks: frozenset[Path] = frozenset()
    """The system temp directories the migration locks are looked for in, for those alone."""
    checkouts: frozenset[Path] = frozenset()
    """Every project's own root, forgotten ones included: never deleted, nor anything that
    holds one, nor anything inside one but its ``.aisquare-worktrees``."""

    def allows(self, path: Path) -> bool:
        """Whether ``path`` (its real place) lies strictly inside a tree or temp root (or is a
        migration lock's name in a lock directory), holds no project's root, and lies inside no
        project's root but through its ``.aisquare-worktrees`` (review of #58, M-c: containment,
        not equality)."""
        parent = _real(path.parent)
        if parent is None:
            return False
        here = parent / path.name
        if here in self.trees or here in self.temp:
            return False
        if parent in self.locks and LOCK_NAME.fullmatch(path.name):
            return True
        in_trees = any(here.is_relative_to(root) for root in self.trees)
        for checkout in self.checkouts:
            if checkout.is_relative_to(here):
                return False  # is, or holds, a project's own checkout
            if here.is_relative_to(checkout) and not in_trees:
                return False
        return in_trees or any(here.is_relative_to(root) for root in self.temp)


def _remove(path: Path, roots: Roots) -> None:
    """THE one delete: ``path`` (a directory and all it holds, or a file), when ``roots``
    allows it; :class:`Refused` for a link or a path they do not. Links inside a directory are
    unlinked, never followed (``cachedir.remove_dir``)."""
    info = os.lstat(path)
    if stat.S_ISLNK(info.st_mode):
        raise Refused(f"{path} is a link: tidy deletes no link's target, nor the link")
    if not roots.allows(path):
        raise Refused(f"{path} is outside rlx's own folders")
    parent = _real(path.parent)
    if parent is None:
        raise Refused(f"{path}'s folder cannot be read")
    if not shutil.rmtree.avoids_symlink_attacks:  # pragma: no cover - not on POSIX
        if stat.S_ISDIR(info.st_mode):
            cachedir.remove_dir(path)
        else:
            os.unlink(path)
        return
    # The folder checked, pinned: opened without following a link and compared, so a parent
    # swapped for a link after the check deletes nothing elsewhere (review of #58, the race).
    fd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try:
        if os.fstat(fd).st_ino != os.lstat(parent).st_ino:
            raise Refused(f"{path}'s folder changed while tidy looked")
        here = os.lstat(path.name, dir_fd=fd)
        if (here.st_dev, here.st_ino) != (info.st_dev, info.st_ino) or stat.S_ISLNK(here.st_mode):
            raise Refused(f"{path} changed while tidy looked")
        if stat.S_ISDIR(here.st_mode):
            cachedir.remove_dir(path.name, dir_fd=fd)
        else:
            os.unlink(path.name, dir_fd=fd)
    finally:
        os.close(fd)


def _there(path: Path) -> bool:
    try:
        os.lstat(path)
    except OSError:
        return False
    return True


# --- the plan --------------------------------------------------------------------------------


@dataclass(frozen=True)
class Item:
    """One path a plan frees, the rule that picked it, and what it takes on disk."""

    rule: str
    path: Path
    bytes: int
    board: str | None = None
    """The board it counts to (#59's receipt): its agent's, or its session's; a lock's none."""


@dataclass(frozen=True)
class Kept:
    """One path a rule looked at and keeps, with why, and its size where that helps a person
    decide (a repository rlx won't delete)."""

    rule: str
    path: Path
    why: str
    bytes: int | None = None


@dataclass
class _Action:
    items: list[Item]
    run: Callable[[], str | None]
    """Carries the items out; ``None`` when it did, else why it did not (re-checked first)."""


@dataclass
class Plan:
    """What :func:`apply` frees, in order, and what the rules keep, with why."""

    items: list[Item] = field(default_factory=list)
    kept: list[Kept] = field(default_factory=list)
    roots: Roots = field(default_factory=Roots)
    actions: list[_Action] = field(default_factory=list)
    branches: list[tuple[Path, str]] = field(default_factory=list)
    """``(repo, branch)``: the ``fleet/…`` branches reap's sweep deletes once the planned trees
    are gone (``fleet.sweep_plan``), shown in the dry run and deleted, those alone, by apply."""
    sweeps: list[Callable[[fleet_service.ReapReport], None]] = field(default_factory=list)
    ignored: list[Path] = field(default_factory=list)
    """Roots whose CACHEDIR.TAG authorises nothing (:data:`ROOT_TAG`), said apart from what is
    kept, so a freed tree is never also named under ``kept`` (tester #3577, m1)."""

    @property
    def bytes(self) -> int:
        return sum(item.bytes for item in self.items)

    def counts(self) -> dict[str, int]:
        found = {rule: 0 for rule in RULES}
        for item in self.items:
            found[item.rule] += 1
        return {rule: n for rule, n in found.items() if n}

    def add(self, items: list[Item], run: Callable[[], str | None]) -> None:
        if items:
            self.items.extend(items)
            self.actions.append(_Action(items, run))


@dataclass
class Report:
    """What :func:`apply` did: each item freed, or not and why, and what reap's branch sweep
    deleted beside the trees."""

    freed: list[Item] = field(default_factory=list)
    not_freed: list[tuple[Item, str]] = field(default_factory=list)
    kept: list[Kept] = field(default_factory=list)
    branches_deleted: list[tuple[Path, str]] = field(default_factory=list)

    @property
    def bytes(self) -> int:
        return sum(item.bytes for item in self.freed)


def plan(*, now: datetime | None = None) -> Plan:
    """Every path ``rlx tidy`` would free now, with its rule and size, and every path a rule
    keeps, with why. Reads only: nothing is deleted, and no lock is taken."""
    at = now or _now()
    config = _settings()
    with store_session() as store:
        projects = store.list_projects(all=True)
        # Liveness from every row there is, a forgotten project's too (review of #58, M2):
        # forgetting refuses only live rows, so a paused agent can sit in one.
        every = _every_row(store)
        result = Plan(roots=_roots(projects, store.list_projects(all=True, include_forgotten=True)))
        commands = _command_lines()
        for project in projects:
            _ended_worktrees(store, project, result)
            if config.idle_build_hours > 0:
                rows = [row for row in every if row.project_id == project.id]
                _idle_build_output(store, project, rows, config, at, result)
        _session_trees(store, every, commands, at, result)
    _stale_locks(at, result)
    return result


def _every_row(store: ContextStore) -> list[FleetAgent]:
    return [
        row
        for project in store.list_projects(all=True, include_forgotten=True)
        for row in store.fleet_agents(project.id)
    ]


def apply(made: Plan) -> Report:
    """Carry out ``made``, exactly, and decide nothing again: each action re-checks its items
    first and never frees more than planned (a tree planned kept loses its caches alone), and an
    item still there afterwards is reported with why. Then the planned branches, those alone."""
    report = Report(kept=list(made.kept))
    if not DELETES:
        report.not_freed = [(item, REPORTS_ONLY) for item in made.items]
        return report
    for action in made.actions:
        try:
            why = action.run()
        except (OSError, Refused, ValueError, fleet_service.FleetError) as exc:
            why = str(exc)
        for item in action.items:
            if _there(item.path):
                report.not_freed.append((item, why or "still there"))
            else:
                report.freed.append(item)
    reaped = fleet_service.ReapReport()
    for sweep in made.sweeps:
        try:
            sweep(reaped)
        except fleet_service.FleetError:
            continue
    report.branches_deleted = list(reaped.branches_deleted)
    return report


def _roots(projects: Iterable[ProjectInfo], every: Iterable[ProjectInfo]) -> Roots:
    def real(paths_: Iterable[Path]) -> frozenset[Path]:
        return frozenset(found for found in (_real(path) for path in paths_) if found is not None)

    return Roots(
        trees=real(project.root / WORKTREES_DIR for project in projects),
        temp=real((claude_accounts.tmp_root(),)),
        locks=real(_system_temp_dirs()),
        checkouts=real(project.root for project in every),
    )


# --- 1. ended agents' worktrees: reap's worktree half ---------------------------------------


def _untracked_caches(tree: Path) -> list[Path]:
    """The caches among ``tree``'s untracked entries, as reap deletes them: each entry that is
    one, or the caches inside one that holds nothing else."""
    _failed, untracked, _other = fleet_service._untracked_caches(tree)
    return [
        found
        for entry in untracked
        for found in ([entry] if cachedir.is_cache(entry) else cachedir.cache_dirs(entry))
    ]


def _ended_worktrees(store: ContextStore, project: ProjectInfo, result: Plan) -> None:
    trees = fleet_service.ended_trees(store, project)
    if trees is None:
        return
    if isinstance(trees, fleet_service.RootUnseen):
        result.kept.append(Kept(ENDED_WORKTREE, trees.root, trees.why))
        return
    going: list[Path] = []
    for agent in trees.candidates:
        tree = agent.cwd
        try:
            if not fleet_service._present(tree):
                continue
        except OSError as exc:
            result.kept.append(Kept(ENDED_WORKTREE, tree, f"not looked at: {exc.strerror}"))
            continue
        if not result.roots.allows(tree):
            result.kept.append(
                Kept(ENDED_WORKTREE, tree, f"not under {WORKTREES_DIR}: reap judges it")
            )
            continue
        if os.path.islink(tree):
            result.kept.append(Kept(ENDED_WORKTREE, tree, fleet_service.LINKED_TREE))
            continue
        if not _there(tree / ".git"):
            # Asked there, git would answer for the repository above it (review of #58).
            result.kept.append(Kept(ENDED_WORKTREE, tree, "it has no .git of its own: kept"))
            continue
        if cachedir.is_cache_dir(tree):
            result.ignored.append(tree)
        try:
            why = fleet_service._why_kept(trees.root, tree, trees.default)
            caches = _untracked_caches(tree)
        except fleet_service.FleetError as exc:
            result.kept.append(Kept(ENDED_WORKTREE, tree, f"git could not be asked ({exc})"))
            continue
        board = agent.board or agent.project_id
        if why is None:
            going.append(tree)
            items = [Item(ENDED_WORKTREE, tree, cachedir.allocated_bytes(tree), board)]
        else:
            result.kept.append(Kept(ENDED_WORKTREE, tree, why))
            items = [
                Item(ENDED_WORKTREE_CACHE, c, cachedir.allocated_bytes(c), board) for c in caches
            ]
        result.add(
            items,
            _reap_one(project, agent, trees, remove=why is None, caches=caches, roots=result.roots),
        )
    unplaced = fleet_service._unplaced_trees_kept(trees.placed, trees.owners, trees.in_use)
    result.kept.extend(Kept(ENDED_WORKTREE, tree, why) for tree, why in unplaced)
    root, common, default = trees.root, trees.common, trees.default
    rows = store.fleet_agents(project.id)
    try:
        done, _kept = fleet_service.sweep_plan(root, common, default, rows, gone=going)
    except fleet_service.FleetError:
        return
    if not done:
        return
    result.branches.extend((root, branch) for branch in done)

    def sweep(report: fleet_service.ReapReport) -> None:
        fleet_service._delete_swept(root, common, done, report)

    result.sweeps.append(sweep)


def _reap_one(
    project: ProjectInfo,
    agent: FleetAgent,
    trees: fleet_service.EndedTrees,
    *,
    remove: bool,
    caches: list[Path],
    roots: Roots,
) -> Callable[[], str | None]:
    def run() -> str | None:
        with store_session() as store:
            now = store.get_fleet_agent(agent.id)
            if now is None or now.paused_at is not None or now.ended_at is None:
                return "skipped: the agent is paused or back since the plan"
            removed, kept = fleet_service._reap_tree(
                store,
                project,
                agent,
                trees.default,
                trees.common,
                trees.read_at,
                remove=remove,
                caches=caches,
                allows=roots.allows,
            )
        if removed or not remove:
            return None if removed else kept
        return kept or "another agent is at the tree now"

    return run


# --- 2. ended sessions' temp trees ------------------------------------------------------------


def _command_lines() -> list[str] | None:
    """Every running process's command line, every user's and every terminal's or none (``ps
    -A``, POSIX), read once; ``None`` where they cannot be read (no ``ps``, as on Windows, or it
    did not answer): then no session is taken for ended."""
    if sys.platform == "win32":
        return None
    listed = process_table._run(["ps", "-A", "-ww", "-o", "args="])
    if listed is None or listed.returncode != 0:
        return None
    return [line for line in listed.stdout.splitlines() if line.strip()]


def _named_running(identifier: str, commands: list[str] | None) -> bool | None:
    """Whether a running process names ``identifier`` on its command line (``--session-id``,
    ``--resume``); ``None`` when the processes could not be read."""
    if commands is None:
        return None
    return any(identifier in line for line in commands)


def _mtime(path: Path) -> datetime | None:
    try:
        return datetime.fromtimestamp(os.lstat(path).st_mtime, UTC)
    except OSError:
        return None


def _last_moved(directory: Path, *more: datetime | None) -> datetime | None:
    """When anything of ``directory`` last moved: its own and its entries' modification times,
    and ``more`` (a board session's last hook, its transcript's time)."""
    times = [_mtime(directory), *more]
    try:
        with os.scandir(directory) as listing:
            times.extend(_mtime(Path(entry.path)) for entry in listing)
    except OSError:
        pass
    known = [moment for moment in times if moment is not None]
    return max(known) if known else None


def _held_by(rows: Iterable[FleetAgent]) -> bool:
    """Whether any of ``rows`` is live or paused: its session is not ended."""
    return any(row.ended_at is None or row.paused_at is not None for row in rows)


def _is_session_id(name: str) -> bool:
    try:
        uuid.UUID(name)
    except ValueError:
        return False
    return True


def _session_dirs(root: Path) -> list[Path]:
    """``<slot>/claude-<uid>/<project>/<session id>/`` under the accounts' temp root, links
    never followed, and only a directory named by a session id: anything else there is not
    ours to judge."""
    found: list[Path] = []
    for slot in _subdirs(root):
        for user in _subdirs(slot):
            if not user.name.startswith("claude-"):
                continue
            for project in _subdirs(user):
                found.extend(path for path in _subdirs(project) if _is_session_id(path.name))
    return found


def _subdirs(path: Path) -> list[Path]:
    try:
        with os.scandir(path) as listing:
            return sorted(
                Path(entry.path) for entry in listing if entry.is_dir(follow_symlinks=False)
            )
    except OSError:
        return []


@dataclass(frozen=True)
class _Liveness:
    """What says whether a session is over, read once for the plan and again before each act."""

    held: bool
    """A live or paused row names it (its own id, or one it held before a ``/clear``:
    ``fleet-held:``), or works in its directory (review of #58, M-c)."""
    ended_at: datetime | None
    """CURRENT positive evidence it ended (review of #58, M1 and M-a): its board row's
    ``ended_at``, set and not cleared by a resume, every row naming it ended, and none made
    after it ended (a resume names it again). ``None``: not ended, whatever its idleness."""
    running: bool | None
    """A running process names it; ``None`` when the processes could not be read."""
    wrote: datetime | None
    """When the session itself last wrote: its board row's last hook, its transcript."""
    board: str | None = None
    """The board its own row is on, which what tidy frees of it counts to (#59)."""


def _naming(store: ContextStore, session_id: str, every: list[FleetAgent]) -> list[FleetAgent]:
    """The rows naming ``session_id``: its own, and the row it was before a ``/clear`` or a
    hand-over moved that row to another session (``store.FLEET_HELD``)."""
    rows = [row for row in every if row.session_id == session_id]
    known = {row.id for row in rows}
    # Read by its id too, whatever project lists it: a purged project's rows are listed by none.
    found = store.fleet_agent_for_session(session_id)
    if found is not None and found.id not in known:
        rows.append(found)
        known.add(found.id)
    holder = store.get_meta(f"{FLEET_HELD}{session_id}")
    if holder:
        row = store.get_fleet_agent(holder)
        if row is None:
            raise _HeldUnread(holder)
        if row.id not in known:
            rows.append(row)
    return rows


class _HeldUnread(Exception):
    """A ``fleet-held:`` row that cannot be read: it HOLDS its session (review of #58)."""


def _liveness(
    store: ContextStore,
    session_id: str,
    directory: Path,
    every: list[FleetAgent],
    commands: list[str] | None,
) -> _Liveness:
    try:
        rows = _naming(store, session_id, every)
    except _HeldUnread:
        return _Liveness(held=True, ended_at=None, running=None, wrote=None)
    inside = _real(directory)
    working_there = inside is not None and any(
        (cwd := _real(row.cwd)) is not None and cwd.is_relative_to(inside)
        for row in every
        if row.ended_at is None or row.paused_at is not None
    )
    session = store.get_session(session_id)
    wrote: list[datetime | None] = []
    ended_at = None
    if session is not None:
        transcript = _mtime(Path(session.transcript_path)) if session.transcript_path else None
        wrote = [session.last_seen_at, transcript]
        end = session.ended_at
        # An explicit end alone (review of #58, M2): every row naming it ended by rlx's word
        # (ended_by: a stop, the agent's exit, reap), none made after the board's end. A board
        # end with no such row (``team prune``'s idleness end, which records no cause) is none.
        if (
            end is not None
            and rows
            and all(
                row.ended_at is not None and row.ended_by in EXPLICIT_ENDS and row.created_at <= end
                for row in rows
            )
        ):
            ended_at = end
    known = [moment for moment in wrote if moment is not None]
    return _Liveness(
        held=_held_by(rows) or working_there,
        ended_at=ended_at,
        running=_named_running(session_id, commands),
        wrote=max(known) if known else None,
        board=session.project_id if session is not None else None,
    )


def _session_trees(
    store: ContextStore,
    every: list[FleetAgent],
    commands: list[str] | None,
    at: datetime,
    result: Plan,
) -> None:
    root = claude_accounts.tmp_root()
    sessions = _session_dirs(root)
    if sessions and commands is None:
        result.kept.append(
            Kept(SESSION_DIR, root, "the running processes could not be read: no session ended")
        )
        return
    for directory in sessions:
        session_id = directory.name
        alive = _liveness(store, session_id, directory, every, commands)
        if alive.held or alive.running is not False or alive.ended_at is None:
            continue  # without current evidence it ended, nothing of it is touched (M1)
        moved = _last_moved(directory, alive.wrote)
        if moved is None or at - moved < ENDED_IDLE:
            continue
        _one_session(directory, session_id, alive.ended_at, at, result, board=alive.board)


def _repositories(directory: Path) -> list[Path]:
    """Every git repository in ``directory``, ``directory`` itself among them when it is one:
    each folder holding a ``.git`` (a clone's, a worktree's or a submodule's) and each bare
    repository, and each bundle (``cachedir.is_bundle``: a repository in one file, manager
    #3697), the outermost only, links not followed and no mount walked (M3: one holds what no
    rule judges, :func:`_one_session`). A tag hides none (review of #58, C2)."""
    found: list[Path] = []
    pending = [directory]
    device = cachedir._device(directory)
    while pending:
        here = pending.pop()
        if _there(here / ".git") or cachedir._is_bare_repository(here):
            found.append(here)
            continue
        try:
            with os.scandir(here) as listing:
                entries = list(listing)
        except OSError:
            continue
        for entry in entries:
            inner = Path(entry.path)
            if entry.is_dir(follow_symlinks=False):
                if cachedir._device(inner) == device:
                    pending.append(inner)
            elif entry.is_file(follow_symlinks=False) and cachedir.is_bundle(inner):
                found.append(inner)
    return sorted(found)


def _git(cwd: Path, *args: str) -> fleet_service._GitResult:
    return fleet_service._git(cwd, *args)


def _tracked_inside(checkout: Path, tagged: Path) -> bool:
    """Whether git tracks anything at or under ``tagged`` in ``checkout`` (true where it cannot
    say): tracked content is git's to judge, never a tag's (review of #58)."""
    try:
        listed = _git(checkout, "ls-files", "--", os.path.relpath(tagged, checkout))
    except fleet_service.FleetError:
        return True
    return listed.returncode != 0 or bool(listed.stdout.strip())


def _enclosing_checkout(path: Path, directory: Path) -> Path | None:
    """The nearest folder at or above ``path``, up to ``directory``, that is a checkout."""
    for here in (path, *path.parents):
        if _there(here / ".git"):
            return here
        if here == directory:
            return None
    return None


def _session_cache(tagged: Path, directory: Path) -> bool:
    """Whether ``tagged`` is a cache tidy may free in the session ``directory`` (asked at the plan
    and again just before, review of #58 M-d): tagged, holding no repository and crossing no
    mount (``cachedir.is_cache``); not the session's own folder, nor one of its top folders (a
    scratchpad), nor a checkout's root, whose tags authorise nothing; and tracking nothing in
    the checkout it lies in."""
    if tagged == directory or tagged.parent == directory or _there(tagged / ".git"):
        return False
    if not cachedir.is_cache(tagged) or not cachedir.same_filesystem(tagged, directory):
        return False
    checkout = _enclosing_checkout(tagged.parent, directory)
    return checkout is None or not _tracked_inside(checkout, tagged)


def _session_caches(directory: Path, result: Plan) -> list[Path]:
    """The caches in a session ``directory`` (:func:`_session_cache`), outermost, found through
    every folder but a bare repository and a mount, checkouts included: their ignored or
    untracked build output is most of the space (the brief). Ignored roots are said."""
    found: list[Path] = []
    pending = [directory]
    device = cachedir._device(directory)
    if device is None:
        return []
    while pending:
        here = pending.pop()
        top = here == directory or here.parent == directory or _there(here / ".git")
        if top and cachedir.is_cache_dir(here):
            result.ignored.append(here)
        elif _session_cache(here, directory):
            found.append(here)
            continue
        if cachedir._is_bare_repository(here):
            continue
        for inner in _subdirs(here):
            if cachedir._device(inner) != device:
                continue  # a mount (or unreadable): not walked, not freed (review of #58, M3)
            pending.append(inner)
    return sorted(found)


def _one_session(
    directory: Path,
    session_id: str,
    ended_at: datetime,
    at: datetime,
    result: Plan,
    *,
    board: str | None = None,
) -> None:
    """An ENDED session (current evidence, no process, idle): its repositories are kept and
    listed with their size, never removed (manager #3585); its caches holding no repository go;
    and once it ended :data:`EVIDENCE_TTL` ago, the whole directory, if it holds no repository
    at all."""
    repositories = _repositories(directory)
    for repository in repositories:
        result.kept.append(
            Kept(
                SESSION_CHECKOUT,
                repository,
                NEVER_A_REPOSITORY,
                cachedir.allocated_bytes(repository),
            )
        )
    roots = result.roots
    if (
        not repositories
        and at - ended_at >= EVIDENCE_TTL
        and _settled_a_week(directory, at)
        and not cachedir.holds_repository(directory)  # nor a mount, which no rule crosses (M3)
    ):
        whole = Item(SESSION_DIR, directory, cachedir.allocated_bytes(directory), board)

        def drop() -> str | None:
            if cachedir.holds_repository(directory):
                return "skipped: a repository is there now"
            if not _settled_a_week(directory, _now()):
                return "skipped: something in it moved this week"
            _remove(directory, roots)
            return None

        result.add([whole], _guarded(session_id, directory, drop, at_least=EVIDENCE_TTL))
        return
    for tagged in _session_caches(directory, result):
        item = Item(SESSION_CACHE, tagged, cachedir.allocated_bytes(tagged), board)

        def free(path: Path = tagged) -> str | None:
            if not _session_cache(path, directory):
                return "skipped: it is no longer a cache tidy may free"
            _remove(path, roots)
            return None

        result.add([item], _guarded(session_id, directory, free))


def _settled_a_week(directory: Path, at: datetime) -> bool:
    """Whether nothing anywhere under ``directory`` moved for :data:`EVIDENCE_TTL` (a bounded,
    link-free walk, ``cachedir.newest_mtime``; one it cannot finish is taken to have moved:
    review of #58, M2), asked at the plan and again just before the directory goes."""
    newest = cachedir.newest_mtime(directory)
    return newest is not None and at - datetime.fromtimestamp(newest, UTC) >= EVIDENCE_TTL


def _guarded(
    session_id: str,
    directory: Path,
    run: Callable[[], str | None],
    *,
    at_least: timedelta = timedelta(0),
) -> Callable[[], str | None]:
    """``run``, once the session is still as planned, every guard asked again (review of #58,
    M-d): no row took it up or works in it, no process names it, the evidence it ended stands
    (and is ``at_least`` old), and it wrote nothing since. Its directory's own times are not
    asked again: this run's deletes move them. ``run`` re-runs its own scan before it acts."""

    def checked() -> str | None:
        now = _now()
        with store_session() as store:
            alive = _liveness(store, session_id, directory, _every_row(store), _command_lines())
        if alive.held or alive.running is not False or alive.ended_at is None:
            return "skipped: the session is no longer ended"
        if now - alive.ended_at < at_least:
            return "skipped: the session is no longer ended"
        if alive.wrote is not None and now - alive.wrote < ENDED_IDLE:
            return "skipped: the session is no longer ended"
        return run()

    return checked


# --- 3. idle live coders' build output -------------------------------------------------------


def _idle_coder(
    store: ContextStore, project: ProjectInfo, row: FleetAgent, hours: float, at: datetime
) -> bool:
    """A live, unpaused coder whose board session has waited ``hours`` and holds no task in
    doing on its board (``row.board``): nothing it works on now needs its build output."""
    if row.ended_at is not None or row.paused_at is not None or row.role != "coder":
        return False
    if not row.session_id:
        return False
    session = store.get_session(row.session_id)
    if session is None or session.state != "waiting" or session.ended_at is not None:
        return False
    if at - session.last_seen_at < timedelta(hours=hours):
        return False
    doing = store.team_tasks(row.board or project.id, status="doing")
    return not any(task.claimed_by == row.session_id for task in doing)


def _idle_build_output(
    store: ContextStore,
    project: ProjectInfo,
    rows: list[FleetAgent],
    config: TidySettings,
    at: datetime,
    result: Plan,
) -> None:
    roots = result.roots
    for row in rows:
        if not _idle_coder(store, project, row, config.idle_build_hours, at):
            continue
        placed = fleet_service.resolve_place(row)
        if not isinstance(placed, fleet_service.Placed):
            continue
        tree = fleet_service._in_its_recorded_tree(placed).cwd
        if not roots.allows(tree):
            continue  # the project's own checkout, or a tree outside the fleet's folder
        if cachedir.is_cache_dir(tree):
            result.ignored.append(tree)
        for tagged in _build_output(tree):
            item = Item(
                IDLE_BUILD_CACHE, tagged, cachedir.allocated_bytes(tagged), row.board or project.id
            )
            result.add(
                [item], _while_idle(project, row, config.idle_build_hours, tree, tagged, roots)
            )


def _build_output(tree: Path) -> list[Path]:
    """The caches STRICTLY inside ``tree`` (never its root, nor one holding a repository or
    crossing a mount), none of them an environment, inside one or holding one at any depth (a
    live agent runs its commands from it), and none that git tracks anything of."""
    return [
        tagged
        for tagged in cachedir.cache_dirs(tree, skip=cachedir.is_environment)
        if _idle_output(tree, tagged)
    ]


def _idle_output(tree: Path, tagged: Path) -> bool:
    """:func:`_build_output`'s test for one cache, asked again just before it goes (M-d)."""
    return (
        tagged != tree
        and cachedir.is_cache(tagged)
        and cachedir.same_filesystem(tagged, tree)
        and not cachedir.holds_environment(tagged)
        and not _tracked_inside(tree, tagged)
    )


def _while_idle(
    project: ProjectInfo, row: FleetAgent, hours: float, tree: Path, tagged: Path, roots: Roots
) -> Callable[[], str | None]:
    def run() -> str | None:
        with store_session() as store:
            now_row = store.get_fleet_agent(row.id)
            idle = now_row is not None and _idle_coder(store, project, now_row, hours, _now())
        if not idle:
            return "skipped: the coder is no longer idle"
        if not _idle_output(tree, tagged):
            return "skipped: it is no longer build output tidy may free"
        _remove(tagged, roots)
        return None

    return run


# --- 4. stale home-migration locks ------------------------------------------------------------


def _system_temp_dirs() -> list[Path]:
    """The system's temp directory, whatever this process's ``TMPDIR`` says (each fleet agent
    has its own): macOS's per-user one, else the platform's default."""
    if sys.platform == "darwin":
        try:
            found = os.confstr(_DARWIN_USER_TEMP_DIR)
        except (OSError, ValueError):
            found = None
        return [Path(found)] if found else [Path("/tmp")]
    if sys.platform == "win32":
        local = os.environ.get("LOCALAPPDATA")
        return [Path(local) / "Temp"] if local else []
    return [Path("/tmp")]


def _stale_locks(at: datetime, result: Plan) -> None:
    for directory in _system_temp_dirs():
        try:
            with os.scandir(directory) as listing:
                names = [entry.name for entry in listing if LOCK_NAME.fullmatch(entry.name)]
        except OSError:
            continue
        for name in sorted(names):
            path = directory / name
            if not _stale_lock(path, at):
                continue
            item = Item(MIGRATION_LOCK, path, cachedir.allocated_bytes(path))
            result.add([item], _free_lock(path, result.roots))


def _stale_lock(path: Path, at: datetime) -> bool:
    try:
        info = os.lstat(path)
    except OSError:
        return False
    if not stat.S_ISREG(info.st_mode):
        return False
    if hasattr(os, "getuid") and info.st_uid != os.getuid():
        return False  # another user's: never ours to judge
    return at - datetime.fromtimestamp(info.st_mtime, UTC) >= LOCK_AGE


def _free_lock(path: Path, roots: Roots) -> Callable[[], str | None]:
    def run() -> str | None:
        if not _stale_lock(path, _now()):
            return "skipped: the lock changed"
        try:
            fd = locking.open_lock_file(path, mode=0o600, create=False)
        except OSError:
            return "skipped: the lock is gone"
        try:
            with locking.held(fd, wait_s=0):
                # The file locked is the one unlinked: a lock made anew at the name meanwhile
                # is another mover's (review of #58, the lock inode race).
                if os.fstat(fd).st_ino != os.lstat(path).st_ino:
                    return "skipped: the lock was made anew"
                _remove(path, roots)
        except TimeoutError:
            return "skipped: a move holds it"
        finally:
            os.close(fd)
        return None

    return run


# --- the periodic job (#59) ---------------------------------------------------------------
#
# ``aisquare watch`` runs it from its tick; 0.10.3's rlxd scheduler is to carry the same job
# with the same setting (``[tidy] every_minutes``). Everything that makes it periodic is here,
# not in the watcher: when it is due, the one-at-a-time lock, the stamp, the receipt.

RECEIPT_BYTES = 10**9
"""What a pass must free before it says so on a board: 1 GB (the owner's brief)."""
PASS_LOCK_PREFIX = "rlx-tidy-"
"""The pass lock's name before the user id: ``rlx-tidy-<uid>.lock`` in the system temp dir."""


def size_text(count: int) -> str:
    """``count`` bytes as a person reads a disk: decimal units, one decimal place past KB."""
    value = float(count)
    for unit in ("B", "KB", "MB", "GB"):
        if value < 1000:
            return f"{value:.0f} {unit}" if unit in ("B", "KB") else f"{value:.1f} {unit}"
        value /= 1000
    return f"{value:.1f} TB"


@dataclass(frozen=True)
class PassReport:
    """What one periodic pass did, as its stamp and ``watch --once --json`` say it."""

    at: datetime
    freed_bytes: int
    freed: dict[str, int]
    """How many paths each rule freed (:data:`RULES`), the rules that freed none left out."""
    not_freed: int
    kept: int
    receipt: str | None = None
    """The line said on a board: only for a pass that freed :data:`RECEIPT_BYTES` or more."""
    board: str | None = None
    """The board it was said on: the one most of the freed bytes count to."""

    def as_json(self) -> dict[str, Any]:
        return {
            "at": self.at.isoformat(timespec="seconds"),
            "freed_bytes": self.freed_bytes,
            "freed": dict(self.freed),
            "not_freed": self.not_freed,
            "kept": self.kept,
            "receipt": self.receipt,
            "board": self.board,
        }


def stamp_path() -> Path:
    """This home's record of its last pass: when (the cadence's clock) and what it did."""
    return paths.watch_dir() / "tidy.json"


def read_stamp() -> dict[str, Any]:
    try:
        data = json.loads(stamp_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def _last_pass_at(stamp: dict[str, Any]) -> datetime | None:
    try:
        found = datetime.fromisoformat(str(stamp["last_pass_at"]))
    except (KeyError, ValueError):
        return None
    return found if found.tzinfo is not None else None


def is_due(at: datetime, every_minutes: float) -> bool:
    """Whether a pass is due at ``at``: none yet in this home, or the last one began
    ``every_minutes`` ago or more (or, by a clock set back, after ``at``)."""
    last = _last_pass_at(read_stamp())
    return last is None or last > at or at - last >= timedelta(minutes=every_minutes)


def pass_lock() -> Path | None:
    """``rlx-tidy-<uid>.lock`` in the system temp dir, found the way rule 4 finds it, whatever
    this process's ``TMPDIR`` says: one pass at a time on the machine, every home's watcher
    alike. ``None`` where there is no such dir."""
    found = _system_temp_dirs()
    if not found:
        return None
    uid = os.getuid() if hasattr(os, "getuid") else 0
    return found[0] / f"{PASS_LOCK_PREFIX}{uid}.lock"


def periodic_pass(now: datetime | None = None) -> PassReport | None:
    """Tidy's periodic job: once due (:func:`is_due`, ``[tidy] every_minutes``; ``0`` is off),
    and while no other pass on the machine holds :func:`pass_lock` (one that does is waited
    for not at all: the pass stays due), the same rules as ``rlx tidy``, :func:`plan` then
    :func:`apply`. Its stamp is written under the lock, whether the pass finished or raised,
    so a pass that keeps failing is tried once per interval, never every tick. Quiet: it wakes
    and nudges no one; a pass that freed :data:`RECEIPT_BYTES` or more says so in one line on
    one board (:func:`_say_it`). ``None``: not due, off, or another pass is running. Never on
    native Windows, where tidy deletes nothing (:data:`DELETES`)."""
    if not DELETES:
        return None
    every = _settings().every_minutes
    if every <= 0:
        return None
    at = now or _now()
    if not is_due(at, every):
        return None
    lock = pass_lock()
    if lock is None:
        return None
    with contextlib.ExitStack() as stack:
        try:
            stack.enter_context(locking.holding(lock, wait_s=0, mode=0o600))
        except TimeoutError:
            return None  # another pass runs now; this one stays due
        except OSError:
            # No lock to be had (the temp dir unwritable): said once per interval, not per tick.
            _write_stamp({"last_pass_at": at.isoformat(timespec="seconds"), "failed": True})
            raise
        if not is_due(at, every):
            return None  # one ran between the first look and the lock
        try:
            made = plan(now=at)
            done = apply(made)
        except BaseException:
            _write_stamp({"last_pass_at": at.isoformat(timespec="seconds"), "failed": True})
            raise
        report = _pass_report(at, done)
        _write_stamp({"last_pass_at": report.at.isoformat(timespec="seconds"), **_totals(report)})
    if report.freed_bytes < RECEIPT_BYTES:
        return report
    return _say_it(report, done)


def _totals(report: PassReport) -> dict[str, Any]:
    found = report.as_json()
    return {key: found[key] for key in ("freed_bytes", "freed", "not_freed", "kept")}


def _write_stamp(data: dict[str, Any]) -> None:
    path = stamp_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    atomic.write_replacing(path, json.dumps(data), durable=False)


def _pass_report(at: datetime, done: Report) -> PassReport:
    freed = {rule: 0 for rule in RULES}
    for item in done.freed:
        freed[item.rule] += 1
    return PassReport(
        at=at,
        freed_bytes=done.bytes,
        freed={rule: n for rule, n in freed.items() if n},
        not_freed=len(done.not_freed),
        kept=len(done.kept),
    )


_RECEIPT_PARTS = (
    ((ENDED_WORKTREE,), "worktree", "worktrees"),
    ((SESSION_DIR, SESSION_CACHE), "session temp path", "session temp paths"),
    ((ENDED_WORKTREE_CACHE, IDLE_BUILD_CACHE), "build cache", "build caches"),
    ((MIGRATION_LOCK,), "lock", "locks"),
)


def receipt_text(report: PassReport) -> str:
    """``rlx tidy freed 14.2 GB: 3 worktrees, 30 session temp paths, 8 build caches, 1 lock``:
    the pass's totals, never a path (a manager pays for every character in its context)."""
    parts = []
    for rules, one, many in _RECEIPT_PARTS:
        count = sum(report.freed.get(rule, 0) for rule in rules)
        if count:
            parts.append(f"{count} {one if count == 1 else many}")
    said = f"{prog.word()} tidy freed {size_text(report.freed_bytes)}"
    return f"{said}: {', '.join(parts)}" if parts else said


def receipt_board(done: Report) -> str | None:
    """The board most of what the pass freed counts to (each item's :attr:`Item.board`), the
    first by id on a tie; ``None`` when none of it counts to one."""
    shares: dict[str, int] = {}
    for item in done.freed:
        if item.board is not None:
            shares[item.board] = shares.get(item.board, 0) + item.bytes
    if not shares:
        return None
    return min(shares, key=lambda board: (-shares[board], board))


def _say_it(report: PassReport, done: Report) -> PassReport:
    """The receipt, one plain ``note`` on one board (:func:`receipt_board`): never a
    ``result`` nor addressed to the manager, which would wake one (``team.wake_class``), and
    no nudge. Said nowhere when no board is the pass's, and never raised out of it."""
    said = replace(report, receipt=receipt_text(report))
    board = receipt_board(done)
    if board is None:
        return said
    from aisquare.core import orchestrator
    from aisquare.services import team as team_service

    try:
        if not orchestrator.team_enabled():
            return said
        with store_session() as store:
            team_service._emit(store, board, "note", said.receipt or "")
    except Exception:
        return said  # the pass is done; a board that would not take its line costs it nothing
    return replace(said, board=board)
