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

1. **Ended agents' worktrees**: reap's worktree half, the trees ``fleet.ended_trees`` chooses,
   each judged by ``fleet._why_kept`` and removed by ``fleet._reap_tree`` under reap's branch
   lock. A tree's untracked ``CACHEDIR.TAG`` output keeps it no longer; it is deleted first,
   kept tree or not.
2. **Ended sessions' temp trees**, under the ``CLAUDE_CODE_TMPDIR`` rlx gives each managed
   Claude slot (``claude-<uid>/<project>/<session id>/``; slot 1 and Codex have none of
   rlx's), and the per-agent ``TMPDIR`` (``paths.agent_tmp_dir``). A session is ENDED when all
   three hold: no live or paused fleet row in any project names it, no running process names
   it on its command line, and nothing of it moved for :data:`ENDED_IDLE`. Then its git
   checkouts go (through ``git worktree remove`` for a registered worktree, without
   ``--force``) unless one holds something found nowhere else, which is kept and named; its
   tagged directories go; and once nothing of it moved for :data:`EVIDENCE_TTL` the whole
   session directory goes, unless it holds a kept checkout.
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

import os
import re
import stat
import sys
import uuid
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from functools import partial
from pathlib import Path

from aisquare.core import cachedir, claude_accounts, paths, process_table
from aisquare.core.config import TidySettings, load_config
from aisquare.core.locking import holding
from aisquare.core.store import ContextStore, store_session
from aisquare.models import FleetAgent, ProjectInfo
from aisquare.services import fleet as fleet_service

ENDED_WORKTREE = "ended worktree"
ENDED_WORKTREE_CACHE = "ended worktree cache"
SESSION_CHECKOUT = "session checkout"
SESSION_CACHE = "session cache"
SESSION_DIR = "session dir"
AGENT_TMPDIR = "agent tmpdir"
IDLE_BUILD_CACHE = "idle build cache"
MIGRATION_LOCK = "migration lock"
RULES = (
    ENDED_WORKTREE,
    ENDED_WORKTREE_CACHE,
    SESSION_CHECKOUT,
    SESSION_CACHE,
    SESSION_DIR,
    AGENT_TMPDIR,
    IDLE_BUILD_CACHE,
    MIGRATION_LOCK,
)
"""Every rule, in the order a plan lists them."""

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
    try:
        return load_config().tidy
    except Exception:
        return TidySettings()


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
    """Every project's own root: never deleted, nor anything that resolves to one."""

    def allows(self, path: Path) -> bool:
        """Whether ``path`` lies strictly inside a tree or temp root (a migration lock's name in a
        lock directory), is no project's root, and its parent holds no link out of the root."""
        parent = _real(path.parent)
        if parent is None:
            return False
        here = parent / path.name
        if here in self.checkouts or here in self.trees or here in self.temp:
            return False
        if parent in self.locks and LOCK_NAME.fullmatch(path.name):
            return True
        return any(here.is_relative_to(root) for root in (*self.trees, *self.temp))


def _remove(path: Path, roots: Roots) -> None:
    """THE one delete: ``path`` (a directory and all it holds, or a file), when ``roots``
    allows it; :class:`Refused` for a link or a path they do not. Links inside a directory are
    unlinked, never followed (``cachedir.remove_dir``)."""
    info = os.lstat(path)
    if stat.S_ISLNK(info.st_mode):
        raise Refused(f"{path} is a link: tidy deletes no link's target, nor the link")
    if not roots.allows(path):
        raise Refused(f"{path} is outside rlx's own folders")
    if stat.S_ISDIR(info.st_mode):
        cachedir.remove_dir(path)
    else:
        os.unlink(path)


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


@dataclass(frozen=True)
class Kept:
    """One path a rule looked at and keeps, with why: what in it is found nowhere else."""

    rule: str
    path: Path
    why: str


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
    branches: list[Callable[[fleet_service.ReapReport], None]] = field(default_factory=list)
    """Reap's branch sweep, per project whose trees it judged, run after their trees."""

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
        rows = {project.id: store.fleet_agents(project.id) for project in projects}
        every = [row for project_rows in rows.values() for row in project_rows]
        result = Plan(roots=_roots(projects))
        commands = _command_lines()
        for project in projects:
            _ended_worktrees(store, project, result)
            if config.idle_build_hours > 0:
                _idle_build_output(store, project, rows[project.id], config, at, result)
        _session_trees(store, every, commands, at, result)
        _agent_temps(every, commands, at, result)
    _stale_locks(at, result)
    return result


def apply(made: Plan) -> Report:
    """Carry out ``made``, exactly: each action re-checks its items first, and an item still
    there afterwards is reported with why. Then reap's branch sweep for each project whose
    trees were judged."""
    report = Report(kept=list(made.kept))
    for action in made.actions:
        try:
            why = action.run()
        except (OSError, Refused, fleet_service.FleetError) as exc:
            why = str(exc)
        for item in action.items:
            if _there(item.path):
                report.not_freed.append((item, why or "still there"))
            else:
                report.freed.append(item)
    reaped = fleet_service.ReapReport()
    for sweep in made.branches:
        try:
            sweep(reaped)
        except fleet_service.FleetError:
            continue
    report.branches_deleted = list(reaped.branches_deleted)
    return report


def _roots(projects: Iterable[ProjectInfo]) -> Roots:
    def real(paths_: Iterable[Path]) -> frozenset[Path]:
        return frozenset(found for found in (_real(path) for path in paths_) if found is not None)

    return Roots(
        trees=real(project.root / WORKTREES_DIR for project in projects),
        temp=real((claude_accounts.tmp_root(), paths.agent_tmp_root())),
        locks=real(_system_temp_dirs()),
        checkouts=real(project.root for project in projects),
    )


# --- 1. ended agents' worktrees: reap's worktree half ---------------------------------------


def _ended_worktrees(store: ContextStore, project: ProjectInfo, result: Plan) -> None:
    trees = fleet_service.ended_trees(store, project)
    if trees is None:
        return
    if isinstance(trees, fleet_service.RootUnseen):
        result.kept.append(Kept(ENDED_WORKTREE, trees.root, trees.why))
        return
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
        try:
            why = fleet_service._why_kept(trees.root, tree, trees.default)
        except fleet_service.FleetError as exc:
            result.kept.append(Kept(ENDED_WORKTREE, tree, f"git could not be asked ({exc})"))
            continue
        _failed, untracked, _other = fleet_service._untracked_caches(tree)
        caches = [found for entry in untracked for found in cachedir.cache_dirs(entry)]
        if why is None:
            items = [Item(ENDED_WORKTREE, tree, cachedir.allocated_bytes(tree))]
        else:
            result.kept.append(Kept(ENDED_WORKTREE, tree, why))
            items = [Item(ENDED_WORKTREE_CACHE, c, cachedir.allocated_bytes(c)) for c in caches]
        result.add(items, _reap_one(project, agent, trees))
    unplaced = fleet_service._unplaced_trees_kept(trees.placed, trees.owners, trees.in_use)
    result.kept.extend(Kept(ENDED_WORKTREE, tree, why) for tree, why in unplaced)
    root, common, default = trees.root, trees.common, trees.default

    def sweep(report: fleet_service.ReapReport) -> None:
        with store_session() as again:
            rows = again.fleet_agents(project.id)
        fleet_service._sweep_fleet_branches(root, common, default, rows, report)

    result.branches.append(sweep)


def _reap_one(
    project: ProjectInfo, agent: FleetAgent, trees: fleet_service.EndedTrees
) -> Callable[[], str | None]:
    def run() -> str | None:
        with store_session() as store:
            removed, kept = fleet_service._reap_tree(
                store, project, agent, trees.default, trees.common, trees.read_at
            )
        if removed:
            return None
        return kept or "another agent is at the tree now"

    return run


# --- 2. ended sessions' temp trees ------------------------------------------------------------


def _command_lines() -> list[str] | None:
    """Every running process's command line, read once; ``None`` where they cannot be read
    (no ``ps``, as on Windows, or it did not answer): then no session is taken for ended."""
    if sys.platform == "win32":
        return None
    listed = process_table._run(["ps", "-axww", "-o", "args="])
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
    by_session: dict[str, list[FleetAgent]] = {}
    for row in every:
        if row.session_id:
            by_session.setdefault(row.session_id, []).append(row)
    for directory in sessions:
        session_id = directory.name
        moved = _session_moved(store, session_id, directory)
        if not _session_ended(by_session.get(session_id, ()), session_id, commands, moved, at):
            continue
        assert moved is not None
        _one_session(directory, session_id, moved, at, result)


def _session_signals(store: ContextStore, session_id: str) -> list[datetime | None]:
    """What the session itself last wrote: its board row's last hook, its transcript's time."""
    session = store.get_session(session_id)
    if session is None:
        return []
    transcript = _mtime(Path(session.transcript_path)) if session.transcript_path else None
    return [session.last_seen_at, transcript]


def _session_moved(store: ContextStore, session_id: str, directory: Path) -> datetime | None:
    return _last_moved(directory, *_session_signals(store, session_id))


def _session_ended(
    rows: Iterable[FleetAgent],
    identifier: str,
    commands: list[str] | None,
    moved: datetime | None,
    at: datetime,
) -> bool:
    """All three: no live or paused row names it, no running process names it, and nothing of
    it moved for :data:`ENDED_IDLE`."""
    if _held_by(rows):
        return False
    if _named_running(identifier, commands) is not False:
        return False
    return moved is not None and at - moved >= ENDED_IDLE


def _checkouts(directory: Path) -> list[Path]:
    """The git checkouts in ``directory``: each directory holding a ``.git`` (a linked worktree's
    file or a clone's directory), the outermost only, never through a link or a tagged
    directory."""
    found: list[Path] = []
    pending = [directory]
    while pending:
        here = pending.pop()
        if here != directory and _there(here / ".git"):
            found.append(here)
            continue
        if cachedir.is_cache_dir(here):
            continue
        pending.extend(_subdirs(here))
    return sorted(found)


def _git(cwd: Path, *args: str) -> fleet_service._GitResult:
    return fleet_service._git(cwd, *args)


def _checkout_kept(checkout: Path) -> str | None:
    """Why ``checkout`` stays, or ``None``: changed or untracked work outside tagged directories,
    or commits reachable from its HEAD that no branch, tag or remote has (#58: kept and named on
    any doubt, git's word being the only one taken)."""
    try:
        failed, _caches, other = fleet_service._untracked_caches(checkout)
        if failed is not None:
            return failed
        if other:
            return "uncommitted changes"
        head = _git(checkout, "rev-parse", "--verify", "--quiet", "HEAD")
        if head.returncode != 0 or not head.stdout.strip():
            return None  # no commit at all: nothing of it lives only here
        alone = _git(checkout, "rev-list", "HEAD", "--not", "--branches", "--tags", "--remotes")
        if alone.returncode != 0:
            return f"git could not say what only it has ({alone.stderr.strip()})"
        shas = alone.stdout.split()
        if shas:
            many = "commit" if len(shas) == 1 else "commits"
            return (
                f"{len(shas)} {many} no branch, tag or remote has (newest {shas[0][:12]}): "
                "branch or push them, then tidy again"
            )
        if (checkout / ".git").is_dir():
            stash = _git(checkout, "rev-parse", "--verify", "--quiet", "refs/stash")
            if stash.returncode == 0 and stash.stdout.strip():
                return "a stash"
    except fleet_service.FleetError as exc:
        return f"git could not be asked ({exc})"
    return None


def _common_dir(checkout: Path) -> Path | None:
    """A linked worktree's repository (``git rev-parse --git-common-dir``); ``None`` for a clone
    or where git cannot say."""
    if not (checkout / ".git").is_file():
        return None
    try:
        found = _git(checkout, "rev-parse", "--path-format=absolute", "--git-common-dir")
    except fleet_service.FleetError:
        return None
    if found.returncode != 0 or not found.stdout.strip():
        return None
    return Path(found.stdout.strip())


def _remove_checkout(checkout: Path, roots: Roots) -> str | None:
    """``checkout`` gone: its tagged output first, then through its repository's ``git worktree
    remove`` (no ``--force``: one git will not remove stays, and says why) for a linked
    worktree, or deleted for a clone. A linked one whose repository is gone is deleted, and the
    repository's list pruned where there is one."""
    if not roots.allows(checkout):
        raise Refused(f"{checkout} is outside rlx's own folders")
    for tagged in cachedir.cache_dirs(checkout):
        _remove(tagged, roots)
    common = _common_dir(checkout)
    if common is None:
        if (checkout / ".git").is_file():
            return "its repository could not be found: kept"
        _remove(checkout, roots)
        return None
    removal = _git(common, "worktree", "remove", str(checkout))
    if removal.returncode != 0:
        return f"git worktree remove refused: {removal.stderr.strip() or 'no reason'}"
    return None


def _one_session(
    directory: Path, session_id: str, moved: datetime, at: datetime, result: Plan
) -> None:
    kept: list[Path] = []
    gone: list[Path] = []
    for checkout in _checkouts(directory):
        why = _checkout_kept(checkout)
        if why is None and _common_dir(checkout) is None and (checkout / ".git").is_file():
            why = "a worktree whose repository could not be found: git can't say what it holds"
        if why is None:
            gone.append(checkout)
        else:
            kept.append(checkout)
            result.kept.append(Kept(SESSION_CHECKOUT, checkout, why))
    roots = result.roots
    if at - moved >= EVIDENCE_TTL and not kept:
        whole = Item(SESSION_DIR, directory, cachedir.allocated_bytes(directory))

        def drop() -> str | None:
            for checkout in gone:
                why = _remove_checkout(checkout, roots)
                if why is not None:
                    return why
            _remove(directory, roots)
            return None

        result.add([whole], _guarded(session_id, directory, drop))
        return
    for checkout in gone:
        item = Item(SESSION_CHECKOUT, checkout, cachedir.allocated_bytes(checkout))
        result.add(
            [item], _guarded(session_id, directory, partial(_remove_checkout, checkout, roots))
        )
    outside = [
        tagged
        for tagged in cachedir.cache_dirs(directory)
        if not any(tagged.is_relative_to(checkout) for checkout in gone)
    ]
    for tagged in outside:
        item = Item(SESSION_CACHE, tagged, cachedir.allocated_bytes(tagged))
        result.add([item], _guarded(session_id, directory, partial(_removed, tagged, roots)))


def _removed(path: Path, roots: Roots) -> str | None:
    _remove(path, roots)
    return None


def _guarded(
    session_id: str, directory: Path, run: Callable[[], str | None]
) -> Callable[[], str | None]:
    """``run``, once the session is still ended: no row took it up, no process names it now,
    and it wrote nothing since (its board row, its transcript). Its directory's own times are
    not asked again: this run's deletes move them."""

    def checked() -> str | None:
        with store_session() as store:
            rows = [
                row
                for project in store.list_projects(all=True)
                for row in store.fleet_agents(project.id)
                if row.session_id == session_id
            ]
            wrote = [moment for moment in _session_signals(store, session_id) if moment]
        now = _now()
        if _held_by(rows) or _named_running(session_id, _command_lines()) is not False:
            return "skipped: the session is no longer ended"
        if wrote and now - max(wrote) < ENDED_IDLE:
            return "skipped: the session is no longer ended"
        return run()

    return checked


# --- the per-agent TMPDIR ---------------------------------------------------------------------


def _agent_temps(
    every: list[FleetAgent], commands: list[str] | None, at: datetime, result: Plan
) -> None:
    root = paths.agent_tmp_root()
    roots = result.roots
    owners: dict[Path, list[FleetAgent]] = {}
    for row in every:
        owners.setdefault(paths.agent_tmp_dir(row.id), []).append(row)
    for directory in _subdirs(root):
        rows = owners.get(directory, [])
        if not rows:
            continue  # no row names it: not ours to judge (#57 keeps its own owner records)
        if not _agent_temp_ended(rows, commands, directory, at):
            continue
        item = Item(AGENT_TMPDIR, directory, cachedir.allocated_bytes(directory))

        def drop(d: Path = directory, r: list[FleetAgent] = rows) -> str | None:
            with store_session() as store:
                now_rows = [store.get_fleet_agent(row.id) or row for row in r]
            if not _agent_temp_ended(now_rows, _command_lines(), d, _now()):
                return "skipped: its agent is no longer ended"
            _remove(d, roots)
            return None

        result.add([item], drop)


def _agent_temp_ended(
    rows: list[FleetAgent], commands: list[str] | None, directory: Path, at: datetime
) -> bool:
    if _held_by(rows):
        return False
    for row in rows:
        if row.session_id and _named_running(row.session_id, commands) is not False:
            return False
    moved = _last_moved(directory)
    return moved is not None and at - moved >= ENDED_IDLE


# --- 3. idle live coders' build output -------------------------------------------------------


def _idle_coder(
    store: ContextStore, project: ProjectInfo, row: FleetAgent, hours: float, at: datetime
) -> bool:
    """A live, unpaused coder whose board session has waited ``hours`` and holds no task in
    doing: nothing it works on now needs its build output."""
    if row.ended_at is not None or row.paused_at is not None or row.role != "coder":
        return False
    if not row.session_id:
        return False
    session = store.get_session(row.session_id)
    if session is None or session.state != "waiting" or session.ended_at is not None:
        return False
    if at - session.last_seen_at < timedelta(hours=hours):
        return False
    doing = store.team_tasks(project.id, status="doing")
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
        for tagged in _build_output(tree):
            item = Item(IDLE_BUILD_CACHE, tagged, cachedir.allocated_bytes(tagged))
            result.add([item], _while_idle(project, row, config.idle_build_hours, tagged, roots))


def _build_output(tree: Path) -> list[Path]:
    """The tagged directories in ``tree``, outermost, never one that is an environment nor any
    inside one (a live agent runs its commands from it)."""
    found: list[Path] = []
    pending = [tree]
    while pending:
        here = pending.pop()
        if cachedir.is_environment(here):
            continue
        if cachedir.is_cache_dir(here):
            found.append(here)
            continue
        pending.extend(_subdirs(here))
    return sorted(found)


def _while_idle(
    project: ProjectInfo, row: FleetAgent, hours: float, tagged: Path, roots: Roots
) -> Callable[[], str | None]:
    def run() -> str | None:
        with store_session() as store:
            now_row = store.get_fleet_agent(row.id)
            idle = now_row is not None and _idle_coder(store, project, now_row, hours, _now())
        if not idle:
            return "skipped: the coder is no longer idle"
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
            with holding(path, wait_s=0, mode=0o600):
                _remove(path, roots)
        except TimeoutError:
            return "skipped: a move holds it"
        return None

    return run
