"""``rlx tidy`` (#58, the owner's disk tidy): every rule, pinned in a sandbox.

Every test runs in the suite's isolated home (``isolated_home``, ``isolated_agent_home``), with
real git repositories under ``tmp_path``, fleet rows and board sessions in the sandbox store,
the running processes and the system temp directory behind seams pointed into ``tmp_path``.
:func:`planned` asserts every root a plan may delete under is in ``tmp_path``: nothing here
reads, let alone deletes, this machine's homes, trees, caches or temp directory.
"""

from __future__ import annotations

import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import cachedir, claude_accounts, locking, paths, process_table
from aisquare.core.config import TidySettings
from aisquare.core.locking import holding
from aisquare.core.store import FLEET_HELD, store_session
from aisquare.models import EndedBy, FleetAgent, ProjectInfo, TeamSession
from aisquare.services import fleet as fleet_service
from aisquare.services import tidy
from tests import test_fleet_service as fleet_suite
from tests.test_fleet_pause_resume import _claim
from tests.test_fleet_service import _add_task, _git, _stale_board_session

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason=(
        "POSIX file semantics (os.utime on links, symlinks, unlinking a held lock, sandbox paths "
        "past MAX_PATH), and rlx tidy deletes nothing on native Windows in 0.10.2 (manager "
        "#3647): tests/test_tidy_on_every_platform.py runs there"
    ),
)

tmux = fleet_suite.tmux
claude_on_path = fleet_suite.claude_on_path
repo = fleet_suite.repo
project = fleet_suite.project

TAG = cachedir.SIGNATURE + b"\n# a cache, by its own word\n"
SYSTEM_TEMP_DIRS = tidy._system_temp_dirs
"""The real lookup, kept before :func:`sandboxed` replaces it: asked only for a path."""
COMMAND_LINES = tidy._command_lines
"""The real reader, kept before the suite's fixture replaces it: run here only with a fake
runner, never the real ``ps``."""
BOARD = "prj_tidyboard0001"
"""The sandbox board a session's own row is on (:func:`session_dir`)."""
HOUR = timedelta(hours=1)


@pytest.fixture(autouse=True)
def board(tmp_path: Path) -> ProjectInfo:
    """The board the sessions' own rows are on: a sandbox project, no checkout."""
    root = tmp_path / "board"
    root.mkdir()
    with store_session() as store:
        return store.onboard_project(ProjectInfo(id=BOARD, root=root))


@pytest.fixture(autouse=True)
def sandboxed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[list[str]]:
    """No process list and no system temp directory of this machine's: the running command
    lines are the test's (empty unless it adds one), and the system temp is ``tmp_path``'s."""
    running: list[str] = []
    system = tmp_path / "system-temp"
    system.mkdir(exist_ok=True)  # the suite's own fixture made it (no_tidy_of_the_machine)
    monkeypatch.setattr(tidy, "_command_lines", lambda: list(running))
    monkeypatch.setattr(tidy, "_system_temp_dirs", lambda: [system])
    yield running


def planned(tmp_path: Path) -> tidy.Plan:
    """``tidy.plan()``, once every root it may delete under is shown to be in the sandbox."""
    made = tidy.plan()
    roots = made.roots
    for root in (*roots.trees, *roots.temp, *roots.locks, *roots.checkouts):
        assert root.is_relative_to(tmp_path.resolve()), f"{root} is outside the sandbox"
    for item in made.items:
        assert item.path.resolve().is_relative_to(tmp_path.resolve()), item
    return made


def tag(directory: Path, *, files: int = 2) -> Path:
    """``directory`` made a tagged cache holding ``files`` small files."""
    directory.mkdir(parents=True, exist_ok=True)
    (directory / cachedir.TAG).write_bytes(TAG)
    for n in range(files):
        (directory / f"out-{n}.bin").write_bytes(b"x" * 4096)
    return directory


def aged(path: Path, ago: timedelta) -> None:
    """``path`` and every entry under it last changed ``ago``."""
    when = time.time() - ago.total_seconds()
    for here, dirs, files in os.walk(path, topdown=False):
        for name in [*files, *dirs]:
            os.utime(Path(here) / name, (when, when), follow_symlinks=False)
    os.utime(path, (when, when))


def session_dir(
    slot: int = 2,
    session: str | None = None,
    *,
    ended: timedelta | None = 3 * HOUR,
    fleet_row: bool = True,
    ended_by: EndedBy | None = "stopped",
) -> Path:
    """A Claude session's temp under the accounts' temp root, as Claude Code lays it out under
    the ``CLAUDE_CODE_TMPDIR`` rlx gives a managed slot. ``ended``: how long ago its board row
    says it ended, its last hook then too (the current positive evidence tidy needs, review of
    #58 M1 and M-a); ``None``: no row and no word that it ended. ``fleet_row``: the fleet row
    that ran it, ended then ``ended_by`` (no row, or no cause: ``team prune``'s idleness end
    alone, which is no evidence, M2)."""
    name = session or str(uuid.uuid4())
    found = claude_accounts.tmp_root() / str(slot) / "claude-501" / "-Users-someone-code-repo"
    found = found / name
    (found / "scratchpad").mkdir(parents=True)
    (found / "scratchpad" / "notes.md").write_text("what the reviewer wrote\n", encoding="utf-8")
    if ended is not None:
        when = datetime.now(UTC) - ended
        with store_session() as store:
            store.upsert_session(
                TeamSession(
                    id=name,
                    project_id=BOARD,
                    role="reviewer",
                    started_at=when,
                    last_seen_at=when,
                    ended_at=when,
                )
            )
            # The explicit end tidy needs beside the board's (review of #58, M2): the fleet
            # row that ran it, stopped on rlx's word.
            if fleet_row:
                store.upsert_fleet_agent(
                    FleetAgent(
                        id=f"agt_{uuid.uuid4().hex[:26]}",
                        project_id=BOARD,
                        label="reviewer-ended",
                        role="reviewer",
                        pane_id="%8",
                        session_id=name,
                        cwd=found,
                        created_at=when - HOUR,
                        ended_at=when,
                        ended_by=ended_by,
                    )
                )
        assert stored_ended(name), "inserted ended: end_session would stamp now"
    return found


def stored_ended(session: str) -> bool:
    with store_session() as store:
        found = store.get_session(session)
    return found is not None and found.ended_at is not None


def row(project: ProjectInfo, session: str, **fields: object) -> FleetAgent:
    """A fleet row naming ``session``, written to the sandbox store. ``created_at`` is set at
    the insert: an upsert never moves it (nor ``ended_by``, nor ``paused_at``)."""
    created = fields.pop("created_at", datetime.now(UTC) - 5 * HOUR)
    assert isinstance(created, datetime)
    agent = FleetAgent(
        id=f"agt_{uuid.uuid4().hex[:26]}",
        project_id=project.id,
        label=str(fields.pop("label", "reviewer-1")),
        role=str(fields.pop("role", "reviewer")),
        pane_id="%9",
        session_id=session,
        cwd=fields.pop("cwd", project.root),
        created_at=created,
        **fields,
    )
    with store_session() as store:
        return store.upsert_fleet_agent(agent)


def rules(made: tidy.Plan) -> dict[Path, str]:
    return {item.path: item.rule for item in made.items}


def every_path(root: Path) -> set[Path]:
    found = {root}
    for here, dirs, files in os.walk(root):
        found.update(Path(here) / name for name in (*dirs, *files))
    return found


# --- 1. live, paused and recently active sessions are untouched --------------------------------


def test_live_paused_running_and_recent_sessions_are_untouched_and_an_ended_one_is_not(
    tmp_path: Path, project: ProjectInfo, sandboxed: list[str]
) -> None:
    live, paused, running, recent, ended = (session_dir() for _ in range(5))
    row(project, live.name)
    # A pause's real shape (review #3696): its row ended ``stopped`` (stop()'s default), paused.
    row(
        project,
        paused.name,
        ended_at=datetime.now(UTC) - 3 * HOUR,
        ended_by="stopped",
        paused_at=datetime.now(UTC),
    )
    sandboxed.append(f"claude --session-id {running.name} --model opus")
    for directory in (live, paused, running, ended):
        tag(directory / "scratchpad" / "target")
        aged(directory, 3 * HOUR)
    tag(recent / "scratchpad" / "target")
    aged(recent, 30 * timedelta(minutes=1))

    made = planned(tmp_path)

    assert rules(made) == {ended / "scratchpad" / "target": tidy.SESSION_CACHE}
    report = tidy.apply(made)
    assert [item.path for item in report.freed] == [ended / "scratchpad" / "target"]
    for directory in (live, paused, running, recent):
        assert (directory / "scratchpad" / "target" / cachedir.TAG).is_file(), directory
    assert (ended / "scratchpad" / "notes.md").is_file(), "evidence stays for 7 days"


# --- 2 and 3. checkouts in an ended session ---------------------------------------------------


def _scratch_worktree(repo: Path, where: Path) -> Path:
    _git("worktree", "add", "-q", "--detach", str(where), cwd=repo)
    return where


def test_a_scratch_worktree_is_kept_with_its_size_and_only_its_build_output_goes(
    tmp_path: Path, repo: Path, project: ProjectInfo
) -> None:
    """rlx won't delete a repository (manager #3585): a session's checkout is listed under kept
    with its size, whatever it holds; its untracked build output goes."""
    session = session_dir()
    checkout = _scratch_worktree(repo, session / "scratchpad" / "wt")
    (checkout / "probe.txt").write_text("a probe\n", encoding="utf-8")
    _git("add", "probe.txt", cwd=checkout)
    _git("commit", "-q", "-m", "only here", cwd=checkout)
    target = tag(checkout / "target")
    _git("config", "status.showUntrackedFiles", "no", cwd=checkout)  # hides nothing from tidy
    aged(session, 3 * HOUR)

    made = planned(tmp_path)

    assert rules(made) == {target: tidy.SESSION_CACHE}
    (kept,) = [k for k in made.kept if k.path == checkout]
    assert kept.rule == tidy.SESSION_CHECKOUT and kept.why == tidy.NEVER_A_REPOSITORY
    assert kept.bytes is not None and kept.bytes > 0
    tidy.apply(made)
    assert not target.exists() and (checkout / "probe.txt").is_file()
    assert str(checkout) in _git("worktree", "list", cwd=repo)


def test_even_a_clean_scratch_worktree_is_kept(
    tmp_path: Path, repo: Path, project: ProjectInfo
) -> None:
    session = session_dir()
    checkout = _scratch_worktree(repo, session / "scratchpad" / "wt")
    aged(session, 3 * HOUR)

    made = planned(tmp_path)

    assert made.items == [] and checkout in {k.path for k in made.kept}
    tidy.apply(made)
    assert checkout.is_dir() and str(checkout) in _git("worktree", "list", cwd=repo)


def test_a_scratch_checkout_with_a_tracked_change_is_kept(
    tmp_path: Path, repo: Path, project: ProjectInfo
) -> None:
    (repo / "kept.txt").write_text("v1\n", encoding="utf-8")
    _git("add", "kept.txt", cwd=repo)
    _git("commit", "-q", "-m", "a file", cwd=repo)
    session = session_dir()
    checkout = _scratch_worktree(repo, session / "scratchpad" / "wt")
    (checkout / "kept.txt").write_text("a mutant left in\n", encoding="utf-8")
    aged(session, 3 * HOUR)

    made = planned(tmp_path)

    assert made.items == []
    assert [(k.path, k.why) for k in made.kept] == [(checkout, tidy.NEVER_A_REPOSITORY)]


# --- F1 of #3507: a checkout whose repository goes with it keeps what no REMOTE has ------------


def _clone(repo: Path, where: Path) -> Path:
    """A clone of ``repo``'s bare remote at ``where``: its ``main`` is on the remote."""
    remote = repo.parent / "remote.git"
    if not remote.exists():
        _git("clone", "-q", "--bare", str(repo), str(remote), cwd=repo.parent)
    _git("clone", "-q", str(remote), str(where), cwd=repo.parent)
    return where


def _commit(checkout: Path, name: str) -> str:
    (checkout / name).write_text(f"{name}\n", encoding="utf-8")
    _git("add", name, cwd=checkout)
    _git("commit", "-q", "-m", name, cwd=checkout)
    return _git("rev-parse", "HEAD", cwd=checkout)


def _kept_why(made: tidy.Plan, path: Path) -> str:
    (why,) = [k.why for k in made.kept if k.path == path]
    return why


@pytest.mark.parametrize(
    "form", ["unpushed", "another branch", "a stash", "all on its remote", "8 days"]
)
def test_a_clone_in_a_session_is_never_deleted_whatever_it_holds(
    tmp_path: Path, repo: Path, form: str
) -> None:
    """Round 2's F1 forms, and the control that went then: every clone is kept now, with its size
    and the way to remove it by hand, and the session around it stays."""
    (repo / "tracked.txt").write_text("v1\n", encoding="utf-8")
    _git("add", "tracked.txt", cwd=repo)
    _git("commit", "-q", "-m", "tracked", cwd=repo)
    session = session_dir(ended=8 * timedelta(days=1) if form == "8 days" else 3 * HOUR)
    clone = _clone(repo, session / "scratchpad" / "clone")
    if form in ("unpushed", "8 days"):
        _commit(clone, "only-here.txt")
    elif form == "another branch":
        _git("switch", "-q", "-c", "side", cwd=clone)
        _commit(clone, "side.txt")
        _git("switch", "-q", "main", cwd=clone)
    elif form == "a stash":
        (clone / "tracked.txt").write_text("work in progress\n", encoding="utf-8")
        _git("stash", "-q", cwd=clone)
    aged(session, 8 * timedelta(days=1) if form == "8 days" else 3 * HOUR)
    before = every_path(clone)

    made = planned(tmp_path)
    tidy.apply(made)

    assert made.items == [] and _kept_why(made, clone) == tidy.NEVER_A_REPOSITORY
    assert every_path(clone) == before


def test_a_repository_inside_a_session_and_its_worktree_are_both_kept(
    tmp_path: Path, repo: Path
) -> None:
    session = session_dir(ended=8 * timedelta(days=1))
    inner = _clone(repo, session / "scratchpad" / "repo")
    tree = session / "scratchpad" / "wt"
    _git("worktree", "add", "-q", "-b", "feature", str(tree), cwd=inner)
    _commit(tree, "feature.txt")
    aged(session, 8 * timedelta(days=1))

    made = planned(tmp_path)

    assert made.items == []
    assert {inner, tree} <= {k.path for k in made.kept}


# --- C2 of review #3522: a tag never authorises a root, nor anything holding a .git ----------


@pytest.mark.parametrize("by", ["CARGO_TARGET_DIR=scratchpad", "UV_CACHE_DIR=scratchpad"])
def test_a_tagged_scratchpad_authorises_nothing_and_hides_no_checkout(
    tmp_path: Path, repo: Path, by: str
) -> None:
    """A build pointed at the scratchpad tags its root: the tag is ignored, said apart from what
    is kept, and the checkouts in it are found and kept."""
    session = session_dir()
    scratchpad = tag(session / "scratchpad", files=1)
    unpushed = _clone(repo, scratchpad / "unpushed")
    _commit(unpushed, "only-here.txt")
    clean = _clone(repo, scratchpad / "clean")
    aged(session, 3 * HOUR)

    made = planned(tmp_path)

    assert made.items == [], by
    assert {unpushed, clean} <= {k.path for k in made.kept}
    assert scratchpad in made.ignored and scratchpad not in {k.path for k in made.kept}
    tidy.apply(made)
    assert (scratchpad / "notes.md").is_file() and (unpushed / "only-here.txt").is_file()


def test_a_tag_at_a_live_coders_tree_root_never_takes_the_tree(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    """``pytest -o cache_dir=.`` tags a tree's root: only the build output strictly inside goes,
    and the tree, its files and its .venv stay. The root's tag is said apart from ``kept``."""
    _agent, tree = _idle_coder_tree(project, waited=25 * HOUR)
    (tree / cachedir.TAG).write_bytes(TAG)

    made = planned(tmp_path)

    assert rules(made) == {tree / "target": tidy.IDLE_BUILD_CACHE}
    assert tree in made.ignored and tree not in {k.path for k in made.kept}
    tidy.apply(made)
    assert (tree / "notes.txt").is_file() and (tree / ".venv").is_dir()


def test_a_tracked_tagged_folder_is_never_build_output(tmp_path: Path, repo: Path) -> None:
    """A repository that tracks a CACHEDIR.TAG at its root and a tagged folder of tracked files:
    in a session's checkout of it, neither goes; the checkout is kept."""
    (repo / cachedir.TAG).write_bytes(TAG)
    vendored = tag(repo / "vendor" / "cache", files=1)
    _git("add", "-f", cachedir.TAG, str(vendored.relative_to(repo)), cwd=repo)
    _git("commit", "-q", "-m", "tracked tags", cwd=repo)
    session = session_dir()
    checkout = _scratch_worktree(repo, session / "scratchpad" / "wt")
    aged(session, 3 * HOUR)

    made = planned(tmp_path)

    assert made.items == [] and checkout in made.ignored
    assert _kept_why(made, checkout) == tidy.NEVER_A_REPOSITORY


def test_a_tagged_folder_holding_a_repository_is_no_build_output(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    """Reap's rule too: a tagged untracked folder with a nested repository in it keeps the tree."""
    agent = _ended_coder(project, "coder-nested")
    nested = tag(agent.cwd / "target")
    _git("init", "-q", str(nested / "repo"), cwd=agent.cwd)

    report = fleet_service.reap(project)

    assert (agent.cwd, "uncommitted changes") in report.worktrees_kept
    assert (nested / "repo" / ".git").is_dir()


def test_cachedir_never_counts_a_root_or_a_tagged_folder_holding_a_git(tmp_path: Path) -> None:
    root = tag(tmp_path / "root", files=1)
    inner = tag(root / "inner", files=1)
    holding_git = tag(root / "has-git", files=1)
    (holding_git / "deep" / ".git").mkdir(parents=True)
    assert cachedir.cache_dirs(root) == [inner]
    assert not cachedir.is_cache(holding_git) and not cachedir.only_caches(holding_git)


# --- M1 and M2 of review #3522: positive evidence; every project's rows --------------------


def test_a_session_with_no_word_that_it_ended_is_never_ended_by_idleness(
    tmp_path: Path,
) -> None:
    quiet = session_dir(ended=None)
    tag(quiet / "scratchpad" / "target")
    aged(quiet, 3 * HOUR)

    assert planned(tmp_path).items == []


def test_only_a_session_ended_a_week_ago_with_no_repository_goes_whole(
    tmp_path: Path, repo: Path
) -> None:
    """The whole directory goes on evidence that it ended a week ago, and only when no repository
    is in it; with no evidence, never."""
    week = 8 * timedelta(days=1)
    empty = session_dir(ended=week)
    aged(empty, week)
    silent = session_dir(ended=None)
    aged(silent, week)
    holding = session_dir(ended=week)
    clone = _clone(repo, holding / "scratchpad" / "clone")
    cache = tag(holding / "scratchpad" / "target")
    aged(holding, week)

    made = planned(tmp_path)

    assert rules(made) == {empty: tidy.SESSION_DIR, cache: tidy.SESSION_CACHE}
    assert clone in {k.path for k in made.kept}


@pytest.mark.parametrize("how", ["rows ended, no board word", "a row made after its end"])
def test_stale_or_missing_evidence_ends_no_session(
    tmp_path: Path, project: ProjectInfo, how: str
) -> None:
    """Review of #58, M-a: ended rows alone are no evidence, and a row made after the board's
    end (a resume) means it is not ended now."""
    if how == "rows ended, no board word":
        session = session_dir(ended=None)
        row(project, session.name, ended_at=datetime.now(UTC) - 3 * HOUR)
    else:
        # Ended on rlx's word, so only its being made after the board's end (a resume) says
        # the session is not ended now (an upsert never moved created_at: written at insert).
        session = session_dir(ended=5 * HOUR)
        row(
            project,
            session.name,
            ended_at=datetime.now(UTC) - 3 * HOUR,
            ended_by="stopped",
            created_at=datetime.now(UTC) - 4 * HOUR,
        )
    tag(session / "scratchpad" / "target")
    aged(session, 3 * HOUR)

    assert planned(tmp_path).items == []


def test_a_paused_row_in_a_forgotten_project_still_holds_its_session(
    tmp_path: Path, project: ProjectInfo
) -> None:
    session = session_dir()
    row(
        project,
        session.name,
        ended_at=datetime.now(UTC),
        ended_by="stopped",  # a pause's real shape (review #3696)
        paused_at=datetime.now(UTC),
    )
    with store_session() as store:
        store.forget_project(project.id)
    tag(session / "scratchpad" / "target")
    aged(session, 3 * HOUR)

    assert planned(tmp_path).items == []


# --- round 3 (review #3584, manager #3585): no repository is ever deleted --------------------


def test_a_checkouts_own_repository_is_not_another(tmp_path: Path, repo: Path) -> None:
    """A main checkout's ``.git`` is a directory shaped like a bare repository: below its root,
    it is neither counted nor walked into. A repository inside it is."""
    assert not cachedir.holds_repository(repo, below_only=True)
    assert cachedir.holds_repository(repo)
    _git("init", "-q", str(repo / "vendor" / "inner"), cwd=repo)
    assert cachedir.holds_repository(repo, below_only=True)


@pytest.mark.parametrize("nested", ["an ignored clone", "a submodule's .git file"])
def test_reap_keeps_a_tree_that_holds_any_repository_but_its_own(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo, nested: str
) -> None:
    """C-a: a repository git ignores, or a submodule whose commits live in its own git dir, is
    judged by nothing in the tree's status: the tree is kept and named."""
    agent = _ended_coder(project, "coder-nested")
    (agent.cwd / ".gitignore").write_text("vendor/\n", encoding="utf-8")
    _git("add", ".gitignore", cwd=agent.cwd)
    _git("commit", "-q", "-m", "ignore vendor", cwd=agent.cwd)
    inner = agent.cwd / "vendor" / "lib"
    inner.mkdir(parents=True)
    if nested == "an ignored clone":
        _git("init", "-q", str(inner), cwd=agent.cwd)
    else:
        (inner / ".git").write_text("gitdir: ../../.git/modules/lib\n", encoding="utf-8")

    report = fleet_service.reap(project)

    assert (agent.cwd, fleet_service.NESTED_REPOSITORY) in report.worktrees_kept
    assert (inner / ".git").exists()


def test_a_cache_holding_a_bare_repository_is_never_freed(tmp_path: Path, repo: Path) -> None:
    session = session_dir()
    cache = tag(session / "scratchpad" / "target")
    _git("init", "-q", "--bare", str(cache / "mirror.git"), cwd=tmp_path)
    plain = tag(session / "scratchpad" / "plain-cache")
    aged(session, 3 * HOUR)

    assert rules(planned(tmp_path)) == {plain: tidy.SESSION_CACHE}


def test_a_live_rows_tree_inside_a_session_holds_it(tmp_path: Path, project: ProjectInfo) -> None:
    """M-c: liveness is not by id alone: a live row working inside the session's directory."""
    session = session_dir()
    row(project, str(uuid.uuid4()), label="coder-2", role="coder", cwd=session / "scratchpad")
    tag(session / "scratchpad" / "target")
    aged(session, 3 * HOUR)

    assert planned(tmp_path).items == []


def test_a_session_a_live_row_held_before_a_clear_is_held(
    tmp_path: Path, project: ProjectInfo
) -> None:
    """M-c: ``fleet-held:<old id>``: the row moved on to another session, and is live."""
    session = session_dir()
    live = row(project, str(uuid.uuid4()), label="coder-3", role="coder")
    with store_session() as store:
        store.set_meta(f"{FLEET_HELD}{session.name}", live.id)
    tag(session / "scratchpad" / "target")
    aged(session, 3 * HOUR)

    assert planned(tmp_path).items == []


def test_the_one_delete_refuses_what_holds_a_project_or_lies_inside_one(tmp_path: Path) -> None:
    """M-c: project roots by containment, not equality, forgotten ones included."""
    temp = (tmp_path / "temp").resolve()
    project_inside = temp / "a-project"
    project_inside.mkdir(parents=True)
    roots = tidy.Roots(temp=frozenset({temp}), checkouts=frozenset({project_inside}))
    assert not roots.allows(temp / "a-project")
    assert not roots.allows(temp / "a-project" / "src")
    holder = temp / "holder"
    (holder / "nested-project").mkdir(parents=True)
    roots = tidy.Roots(
        temp=frozenset({temp}), checkouts=frozenset({(holder / "nested-project").resolve()})
    )
    assert not roots.allows(holder) and roots.allows(temp / "elsewhere")


@pytest.mark.parametrize("what", ["a cache", "the session dir"])
def test_a_repository_that_appears_after_the_plan_is_never_deleted(
    tmp_path: Path, what: str
) -> None:
    """M-d: apply runs the repository scan again just before each removal."""
    if what == "a cache":
        session = session_dir()
        target = tag(session / "scratchpad" / "target")
        aged(session, 3 * HOUR)
        made = planned(tmp_path)
        assert rules(made) == {target: tidy.SESSION_CACHE}
        _git("init", "-q", str(target / "late"), cwd=tmp_path)
        path = target
    else:
        session = session_dir(ended=8 * timedelta(days=1))
        aged(session, 8 * timedelta(days=1))
        made = planned(tmp_path)
        assert rules(made) == {session: tidy.SESSION_DIR}
        _git("init", "-q", str(session / "scratchpad" / "late"), cwd=tmp_path)
        path = session

    report = tidy.apply(made)

    assert report.freed == [] and path.is_dir()
    assert report.not_freed and report.not_freed[0][1].startswith("skipped")


@pytest.mark.skipif(not hasattr(os, "getuid") or os.getuid() == 0, reason="needs a mode to bite")
def test_a_folder_rmtree_cannot_open_is_loosened_never_a_traceback(tmp_path: Path) -> None:
    """The rmtree hook made the failed ``os.open`` again without its flags: a TypeError that
    escaped apply and reap. It loosens the folder and the second pass removes it."""
    tree = tmp_path / "cache"
    shut = tree / "shut"
    shut.mkdir(parents=True)
    (shut / "inside.bin").write_bytes(b"x")
    shut.chmod(0)

    cachedir.remove_dir(tree)

    assert not tree.exists()


def test_an_agent_taken_up_again_after_the_plan_keeps_its_tree(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    """A spawn of the same label after the plan works in the same tree: the real run re-asks the
    rows, under reap's lock, and removes nothing under the new agent."""
    built = _ended_coder(project, "coder-built")
    made = planned(tmp_path)
    assert rules(made) == {built.cwd: tidy.ENDED_WORKTREE}
    again = fleet_service.spawn(project, "coder", label="coder-built").agent
    assert again.cwd == built.cwd and again.ended_at is None

    report = tidy.apply(made)

    assert built.cwd.is_dir() and report.freed == []


def test_a_cache_holding_an_environment_deeper_is_not_an_idle_coders_build_output(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    _agent, tree = _idle_coder_tree(project, waited=25 * HOUR)
    tools = tag(tree / "tools-cache")
    (tools / "venv").mkdir()
    (tools / "venv" / cachedir.ENVIRONMENT_MARKER).write_text("home = /usr\n", encoding="utf-8")

    assert rules(planned(tmp_path)) == {tree / "target": tidy.IDLE_BUILD_CACHE}


def test_a_freed_tree_with_a_root_tag_is_never_also_said_kept(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    """Tester #3577, m1: the root-tag note is its own list."""
    built = _ended_coder(project, "coder-built")
    exclude = Path(
        _git("rev-parse", "--path-format=absolute", "--git-path", "info/exclude", cwd=built.cwd)
    )
    exclude.parent.mkdir(parents=True, exist_ok=True)
    exclude.write_text(f"/{cachedir.TAG}\n", encoding="utf-8")  # git ignores it: the tree is clean
    (built.cwd / cachedir.TAG).write_bytes(TAG)

    made = planned(tmp_path)

    assert rules(made) == {built.cwd: tidy.ENDED_WORKTREE}
    assert built.cwd in made.ignored and built.cwd not in {k.path for k in made.kept}


# --- the minors of review #3522 ----------------------------------------------------------------


def test_a_tree_planned_kept_is_never_removed_by_the_real_run(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    """``apply`` decides nothing again: the change that kept the tree is gone by the real run,
    and still only its build output goes."""
    changed = _ended_coder(project, "coder-changed")
    (changed.cwd / "wip.txt").write_text("unfinished\n", encoding="utf-8")
    cache = tag(changed.cwd / "target")
    made = planned(tmp_path)
    assert rules(made) == {cache: tidy.ENDED_WORKTREE_CACHE}
    (changed.cwd / "wip.txt").unlink()

    tidy.apply(made)

    assert changed.cwd.is_dir() and not cache.exists()


def test_the_branch_sweep_is_in_the_dry_run_and_the_real_run_deletes_those_alone(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    agent = _ended_coder(project, "coder-merged")
    branch = _git("rev-parse", "--abbrev-ref", "HEAD", cwd=agent.cwd)

    made = planned(tmp_path)

    assert made.branches == [(project.root, branch)]
    shown = json.loads(CliRunner().invoke(app, ["--json", "tidy", "--dry-run"]).stdout)
    assert shown["branches"] == [{"repo": str(project.root), "branch": branch}]
    report = tidy.apply(made)
    assert report.branches_deleted == [(project.root, branch)]
    assert _git("branch", "--list", branch, cwd=project.root) == ""


def test_an_idle_coders_task_in_doing_is_read_on_its_own_board(
    tmp_path: Path,
    tmux: object,
    claude_on_path: Path,
    project: ProjectInfo,
    board: ProjectInfo,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A coder spawned under a hub reads the hub's board (``row.board``), not its project's: its
    task in doing is there."""
    monkeypatch.setenv("AISQUARE_TEAM_HUB", str(board.root))
    agent, _tree = _idle_coder_tree(project, waited=25 * HOUR)
    assert agent.board is not None and agent.board != project.id, (agent.board, project.id)
    with store_session() as store:
        hub = store.get_project(agent.board) or store.onboard_project(
            ProjectInfo(id=agent.board, root=board.root)
        )
    _claim(agent, _add_task(hub, "keep building, on the hub"))

    assert planned(tmp_path).items == []


def test_a_config_that_cannot_be_read_turns_the_coder_rule_off(
    tmp_path: Path,
    tmux: object,
    claude_on_path: Path,
    project: ProjectInfo,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _idle_coder_tree(project, waited=25 * HOUR)

    def broken() -> object:
        raise ValueError("config.toml: not TOML")

    monkeypatch.setattr(tidy, "load_config", broken)

    assert tidy._settings().idle_build_hours == 0
    assert planned(tmp_path).items == []


@pytest.mark.skipif(not hasattr(os, "getuid") or os.getuid() == 0, reason="needs a mode to bite")
def test_a_delete_never_changes_the_mode_of_what_a_link_points_at(tmp_path: Path) -> None:
    """The removal hook loosens what refused inside the tree alone: a read-only folder holding a
    link out goes, and the link's target keeps its mode."""
    outside = tmp_path / "outside.txt"
    outside.write_text("not ours\n", encoding="utf-8")
    outside.chmod(0o400)
    stuck = tmp_path / "stuck"
    stuck.mkdir()
    (stuck / "link").symlink_to(outside)
    stuck.chmod(0o500)

    cachedir.remove_dir(stuck)

    assert not stuck.exists() and outside.is_file()
    assert oct(outside.stat().st_mode & 0o777) == oct(0o400)


def test_reap_deletes_nothing_through_a_tree_that_is_a_link(tmp_path: Path) -> None:
    real = tmp_path / "real-tree"
    _git("init", "-q", str(real), cwd=tmp_path)
    target = tag(real / "target")
    link = tmp_path / "linked-tree"
    link.symlink_to(real, target_is_directory=True)

    assert fleet_service._remove_untracked_caches(link) == []
    assert target.is_dir()


def test_the_process_list_is_every_process_not_only_a_terminals(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    asked: list[list[str]] = []

    def fake(argv: list[str]) -> subprocess.CompletedProcess[str]:
        asked.append(argv)
        return subprocess.CompletedProcess(argv, 0, "claude --session-id abc\n", "")

    monkeypatch.setattr(process_table, "_run", fake)
    if sys.platform == "win32":
        assert COMMAND_LINES() is None
        return
    assert COMMAND_LINES() == ["claude --session-id abc"]
    assert asked == [["ps", "-A", "-ww", "-o", "args="]]


def test_a_session_whose_root_is_a_checkout_is_kept(tmp_path: Path, repo: Path) -> None:
    session = session_dir(ended=8 * timedelta(days=1))
    shutil.rmtree(session / "scratchpad")
    _clone(repo, session)
    aged(session, 8 * timedelta(days=1))

    made = planned(tmp_path)

    assert made.items == [] and _kept_why(made, session) == tidy.NEVER_A_REPOSITORY


def test_git_runs_with_no_variable_pointing_it_at_another_repository() -> None:
    from aisquare.core import spawn

    env = spawn.git_env({"GIT_DIR": "/elsewhere/.git", "GIT_WORK_TREE": "/elsewhere", "A": "1"})

    assert "GIT_DIR" not in env and "GIT_WORK_TREE" not in env and env["A"] == "1"


# --- 4. ended agents' worktrees: reap's worktree half, with the tag rule -----------------------


def _ended_coder(project: ProjectInfo, label: str) -> FleetAgent:
    agent = fleet_service.spawn(project, "coder", label=label).agent
    fleet_service.stop(project, label, force=True)
    return agent


def test_reap_removes_an_ended_tree_whose_only_untracked_content_is_build_output(
    tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    """The new rule in reap itself (``fleet._why_kept``): an untracked ``target/`` holding its
    CACHEDIR.TAG kept a reviewer's tree forever (5.7 GB on the owner's Mac). It is deleted, then
    the tree is judged, and goes. A tree with a real change stays, as it always did, and still
    loses its build output."""
    built = _ended_coder(project, "coder-built")
    tag(built.cwd / "rlxd" / "target")
    changed = _ended_coder(project, "coder-changed")
    (changed.cwd / "wip.txt").write_text("unfinished\n", encoding="utf-8")
    cache = tag(changed.cwd / "target")

    report = fleet_service.reap(project)

    assert report.worktrees_removed == [built.cwd] and not built.cwd.exists()
    assert (changed.cwd, "uncommitted changes") in report.worktrees_kept
    assert (changed.cwd / "wip.txt").is_file() and not cache.exists()


def test_tidy_lists_and_frees_the_ended_trees_reap_would_and_names_the_one_it_keeps(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    built = _ended_coder(project, "coder-built")
    tag(built.cwd / "target")
    changed = _ended_coder(project, "coder-changed")
    (changed.cwd / "wip.txt").write_text("unfinished\n", encoding="utf-8")
    cache = tag(changed.cwd / "target")

    made = planned(tmp_path)

    assert rules(made) == {built.cwd: tidy.ENDED_WORKTREE, cache: tidy.ENDED_WORKTREE_CACHE}
    assert tidy.Kept(tidy.ENDED_WORKTREE, changed.cwd, "uncommitted changes") in made.kept
    report = tidy.apply(made)
    assert {i.path for i in report.freed} == {built.cwd, cache} and report.not_freed == []
    assert (changed.cwd / "wip.txt").is_file() and not built.cwd.exists()


# --- 5. idle live coders' build output ---------------------------------------------------------


def _idle_coder_tree(project: ProjectInfo, *, waited: timedelta) -> tuple[FleetAgent, Path]:
    agent = fleet_service.spawn(project, "coder", label="coder-idle").agent
    _stale_board_session(agent, "waiting", seen_ago=waited)
    tree = agent.cwd
    (tree / "notes.txt").write_text("untracked, the coder's\n", encoding="utf-8")
    venv = tag(tree / ".venv", files=1)
    (venv / cachedir.ENVIRONMENT_MARKER).write_text("home = /usr/bin\n", encoding="utf-8")
    tag(tree / "target")
    return agent, tree


def test_an_idle_coder_past_the_threshold_loses_only_its_build_output(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    _agent, tree = _idle_coder_tree(project, waited=25 * HOUR)
    tracked = _git("ls-files", cwd=tree).split()

    made = planned(tmp_path)

    assert rules(made) == {tree / "target": tidy.IDLE_BUILD_CACHE}
    tidy.apply(made)
    assert not (tree / "target").exists()
    assert (tree / ".venv" / cachedir.ENVIRONMENT_MARKER).is_file(), "a live coder's .venv stays"
    assert (tree / "notes.txt").is_file()
    assert all((tree / name).exists() for name in tracked)


@pytest.mark.parametrize("why", ["a task in doing", "under the threshold", "rule off"])
def test_an_idle_coder_loses_nothing_while_it_holds_a_task_is_recent_or_the_rule_is_off(
    tmp_path: Path,
    tmux: object,
    claude_on_path: Path,
    project: ProjectInfo,
    monkeypatch: pytest.MonkeyPatch,
    why: str,
) -> None:
    agent, _tree = _idle_coder_tree(
        project, waited=(2 if why == "under the threshold" else 25) * HOUR
    )
    if why == "a task in doing":
        _claim(agent, _add_task(project, "keep building"))
    if why == "rule off":
        monkeypatch.setattr(tidy, "_settings", lambda: TidySettings(idle_build_hours=0))

    assert planned(tmp_path).items == []


def test_a_coder_working_in_the_projects_own_checkout_is_never_touched(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    agent = fleet_service.spawn(project, "coder", label="coder-main", worktree=False).agent
    assert agent.cwd.resolve() == project.root.resolve()
    _stale_board_session(agent, "waiting", seen_ago=25 * HOUR)
    tag(project.root / "target")

    assert planned(tmp_path).items == []


# --- 6. evidence: small files until the TTL, then the whole session ----------------------------


def test_an_ended_sessions_small_files_stay_a_week_then_its_directory_goes(
    tmp_path: Path, repo: Path
) -> None:
    week_old = session_dir(ended=8 * timedelta(days=1))
    aged(week_old, 8 * timedelta(days=1))
    days_old = session_dir(ended=3 * timedelta(days=1))
    aged(days_old, 3 * timedelta(days=1))
    holding_work = session_dir(ended=8 * timedelta(days=1))
    checkout = _scratch_worktree(repo, holding_work / "scratchpad" / "wt")
    aged(holding_work, 8 * timedelta(days=1))

    made = planned(tmp_path)

    assert rules(made) == {week_old: tidy.SESSION_DIR}
    tidy.apply(made)
    assert not week_old.exists()
    assert (days_old / "scratchpad" / "notes.md").is_file()
    assert checkout.is_dir() and checkout in {k.path for k in made.kept}


# --- 8. stale home-migration locks in the system temp directory --------------------------------


def test_only_a_stale_free_migration_lock_of_the_exact_name_goes(tmp_path: Path) -> None:
    system = tmp_path / "system-temp"
    stale, held, fresh = (system / f"rlx-home-migration-{n * 16}.lock" for n in "abc")
    other = system / "rlx-home-migration-notours.lock"
    for path in (stale, held, fresh, other):
        path.write_bytes(b"")
    for path in (stale, held, other):
        aged(path, 25 * HOUR)

    made = planned(tmp_path)

    assert rules(made) == {stale: tidy.MIGRATION_LOCK, held: tidy.MIGRATION_LOCK}
    with holding(held, wait_s=0):  # a move running now
        report = tidy.apply(made)
    assert [i.path for i in report.freed] == [stale]
    assert [(i.path, why) for i, why in report.not_freed] == [(held, "skipped: a move holds it")]
    assert held.exists() and fresh.exists() and other.exists()


# --- 9. a real run frees exactly the dry run's list -------------------------------------------


def test_a_real_run_frees_exactly_the_dry_runs_list_and_nothing_else(
    tmp_path: Path, repo: Path, project: ProjectInfo
) -> None:
    ended = session_dir()
    _scratch_worktree(repo, ended / "scratchpad" / "wt")
    cache = tag(ended / "scratchpad" / "pytest-cache")
    aged(ended, 3 * HOUR)
    old = session_dir(ended=8 * timedelta(days=1))
    aged(old, 8 * timedelta(days=1))
    lock = tmp_path / "system-temp" / f"rlx-home-migration-{'f' * 16}.lock"
    lock.write_bytes(b"")
    aged(lock, 25 * HOUR)
    watched = (claude_accounts.tmp_root(), tmp_path / "system-temp")
    before = set().union(*(every_path(root) for root in watched))

    dry = planned(tmp_path)
    report = tidy.apply(dry)

    listed = [item.path for item in dry.items]
    assert sorted(listed) == sorted([cache, old, lock])
    assert [item.path for item in report.freed] == listed and report.not_freed == []
    gone = before - set().union(*(every_path(root) for root in watched))
    assert all(any(p == item or p.is_relative_to(item) for item in listed) for p in gone)
    assert report.bytes == dry.bytes > 0


@pytest.mark.parametrize("how", ["a row takes it up", "its process runs", "it writes again"])
def test_a_session_taken_up_after_the_plan_is_skipped_and_said_so(
    tmp_path: Path, project: ProjectInfo, sandboxed: list[str], how: str
) -> None:
    """``apply`` asks each session again just before: one that was ended at the plan and is not
    now keeps everything, and the report says why, never another path in its place."""
    ended = session_dir()
    cache = tag(ended / "scratchpad" / "target")
    aged(ended, 3 * HOUR)
    made = planned(tmp_path)
    assert rules(made) == {cache: tidy.SESSION_CACHE}

    if how == "a row takes it up":
        row(project, ended.name)
    elif how == "its process runs":
        sandboxed.append(f"claude --resume {ended.name}")
    else:
        with store_session() as store:
            store.upsert_session(
                TeamSession(
                    id=ended.name,
                    project_id=project.id,
                    role="reviewer",
                    started_at=datetime.now(UTC),
                    last_seen_at=datetime.now(UTC),
                )
            )
    report = tidy.apply(made)

    assert report.freed == []
    assert [(i.path, why) for i, why in report.not_freed] == [
        (cache, "skipped: the session is no longer ended")
    ]
    assert (cache / cachedir.TAG).is_file()


# --- 10. what it prints -----------------------------------------------------------------------


def test_the_dry_run_prints_each_path_with_its_size_and_rule_and_json_holds_the_same(
    tmp_path: Path, project: ProjectInfo
) -> None:
    ended = session_dir()
    cache = tag(ended / "scratchpad" / "target", files=3)
    aged(ended, 3 * HOUR)
    runner = CliRunner()

    text = runner.invoke(app, ["tidy", "--dry-run"])
    shown = runner.invoke(app, ["--json", "tidy", "--dry-run"])

    assert text.exit_code == 0 and shown.exit_code == 0, text.output + shown.output
    assert "would free" in text.output and tidy.SESSION_CACHE in text.output
    assert cache.name in text.output and cache.exists(), "a dry run deletes nothing"
    data = json.loads(shown.stdout)
    assert data["dry_run"] is True and data["counts"] == {tidy.SESSION_CACHE: 1}
    (entry,) = data["paths"]
    assert entry["rule"] == tidy.SESSION_CACHE and Path(entry["path"]) == cache
    assert entry["bytes"] == cachedir.allocated_bytes(cache) == data["bytes"] > 0
    real = json.loads(runner.invoke(app, ["--json", "tidy"]).stdout)
    assert real["freed"] == data["paths"] and not cache.exists()


@pytest.mark.parametrize("columns", ["80", "120"])
def test_a_kept_paths_why_is_printed_whole_at_any_width(
    tmp_path: Path, repo: Path, columns: str
) -> None:
    """Tester #3507, F3: the ``kept:`` table dropped its WHY at 80 and 120 columns, the line that
    says what to push. Each kept path is two lines now, and the why is never cut."""
    session = session_dir()
    clone = _clone(repo, session / "scratchpad" / "clone")
    _commit(clone, "only-here.txt")
    aged(session, 3 * HOUR)
    why = _kept_why(planned(tmp_path), clone)

    shown = CliRunner().invoke(app, ["tidy", "--dry-run"], env={"COLUMNS": columns})

    assert shown.exit_code == 0, shown.output
    assert f"      {why}" in shown.output.splitlines(), shown.output


# --- 11. links and roots ----------------------------------------------------------------------


def test_a_link_out_of_a_session_is_never_followed(tmp_path: Path) -> None:
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "precious.txt").write_text("not rlx's\n", encoding="utf-8")
    old = session_dir(ended=8 * timedelta(days=1))
    (old / "scratchpad" / "link").symlink_to(outside, target_is_directory=True)
    aged(old, 8 * timedelta(days=1))

    tidy.apply(planned(tmp_path))

    assert not old.exists() and (outside / "precious.txt").is_file()


def test_the_one_delete_refuses_a_path_outside_the_roots_a_root_itself_and_a_link(
    tmp_path: Path,
) -> None:
    roots = tidy.Roots(temp=frozenset({(tmp_path / "temp").resolve()}))
    (tmp_path / "temp").mkdir()
    outside = tmp_path / "elsewhere"
    outside.mkdir()
    link = tmp_path / "temp" / "link"
    link.symlink_to(outside, target_is_directory=True)
    for path in (outside, tmp_path / "temp", link):
        with pytest.raises(tidy.Refused):
            tidy._remove(path, roots)
    assert outside.is_dir() and link.is_symlink()


def test_a_cachedir_tag_without_the_specs_signature_tags_nothing(tmp_path: Path) -> None:
    """The spec's word is its signature: a file that only bears the name marks no cache."""
    ended = session_dir()
    signed = tag(ended / "scratchpad" / "target")
    unsigned = ended / "scratchpad" / "results"
    unsigned.mkdir()
    (unsigned / cachedir.TAG).write_text("not the signature\n", encoding="utf-8")
    (unsigned / "evidence.log").write_text("a test run's log\n", encoding="utf-8")
    aged(ended, 3 * HOUR)

    assert rules(planned(tmp_path)) == {signed: tidy.SESSION_CACHE}


# --- 12. never touched ------------------------------------------------------------------------


def test_slot_ones_claude_home_and_files_beside_the_session_layout_are_never_looked_at(
    tmp_path: Path,
) -> None:
    """Slot 1 is the shell's own Claude home, never rlx's: no temp root of rlx's is there. And
    under rlx's root, only directories named by a session id are judged."""
    beside = claude_accounts.tmp_root() / "2" / "claude-501" / "-proj" / "not-a-session"
    tag(beside / "target")
    aged(beside, 30 * timedelta(days=1))

    made = planned(tmp_path)

    assert made.items == [] and (beside / "target").is_dir()
    assert made.roots.temp == {claude_accounts.tmp_root().resolve()}


def test_a_process_list_that_cannot_be_read_ends_no_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ended = session_dir()
    tag(ended / "scratchpad" / "target")
    aged(ended, 3 * HOUR)
    monkeypatch.setattr(tidy, "_command_lines", lambda: None)

    made = planned(tmp_path)

    assert made.items == []
    assert [k.why for k in made.kept] == [
        "the running processes could not be read: no session ended"
    ]


def test_the_system_temp_dir_is_found_without_this_processs_tmpdir(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """Each fleet agent runs with its own TMPDIR: the locks a mover left are looked for where
    the system keeps temp files, never where this process's TMPDIR points. The real lookup,
    asked only for the path: nothing there is listed or read."""
    own = tmp_path / "an-agents-own"
    own.mkdir()
    monkeypatch.setattr(tempfile, "tempdir", None)  # tempfile's own answer, asked afresh
    monkeypatch.setenv("TMPDIR", str(own))
    monkeypatch.setenv("TEMP", str(own))
    monkeypatch.setenv("TMP", str(own))
    found = SYSTEM_TEMP_DIRS()
    assert all(not path.is_relative_to(tmp_path) for path in found), found


def test_every_root_these_tests_plan_under_is_the_sandboxs(tmp_path: Path) -> None:
    """A guard on the guards: the home every test here plans in is the sandbox's."""
    assert paths.aisquare_home().resolve().is_relative_to(tmp_path.resolve())
    assert claude_accounts.tmp_root().resolve().is_relative_to(tmp_path.resolve())


# --- round 4 (review #3646, manager #3647) --------------------------------------------------


@pytest.mark.parametrize(
    "how", ["team prune's idleness end, no fleet row", "a fleet row ended with no cause"]
)
def test_a_board_end_with_no_explicit_end_beside_it_is_no_evidence(
    tmp_path: Path, how: str
) -> None:
    """M2: ``team prune`` ends an idle board session and records no cause; only an explicit end
    (a stop, a death, on rlx's word) beside the board's counts. A week on, nothing goes."""
    week = 8 * timedelta(days=1)
    if how.startswith("team prune"):
        session = session_dir(ended=week, fleet_row=False)
    else:
        session = session_dir(ended=week, ended_by=None)
    cache = tag(session / "scratchpad" / "target")
    aged(session, week)

    made = planned(tmp_path)

    assert made.items == []
    tidy.apply(made)
    assert cache.is_dir() and (session / "scratchpad" / "notes.md").is_file()


def test_the_whole_directory_waits_a_week_from_its_newest_file_anywhere(tmp_path: Path) -> None:
    """M2: the session ended a week ago, but a file deep inside moved two days ago: the whole
    directory waits (and its notes stay); its caches may still go."""
    week = 8 * timedelta(days=1)
    session = session_dir(ended=week)
    deep = session / "scratchpad" / "a" / "b" / "c"
    deep.mkdir(parents=True)
    (deep / "late.log").write_text("written later\n", encoding="utf-8")
    aged(session, week)
    aged(deep / "late.log", 2 * timedelta(days=1))

    made = planned(tmp_path)

    assert session not in rules(made)
    tidy.apply(made)
    assert (deep / "late.log").is_file() and (session / "scratchpad" / "notes.md").is_file()


def test_the_whole_directory_is_dated_again_just_before_it_goes(tmp_path: Path) -> None:
    week = 8 * timedelta(days=1)
    session = session_dir(ended=week)
    aged(session, week)
    made = planned(tmp_path)
    assert rules(made) == {session: tidy.SESSION_DIR}
    (session / "scratchpad" / "deeper").mkdir()
    (session / "scratchpad" / "deeper" / "now.txt").write_text("now\n", encoding="utf-8")
    os.utime(session / "scratchpad", (time.time() - week.total_seconds(),) * 2)

    report = tidy.apply(made)

    assert report.freed == [] and session.is_dir()
    assert report.not_freed[0][1] == "skipped: something in it moved this week"


def _mounted_at(monkeypatch: pytest.MonkeyPatch, *mounts: Path) -> None:
    """Each of ``mounts``, and all under it, read as on another filesystem (``cachedir._device``,
    the one seam every walk and every delete asks)."""
    real = cachedir._device

    def device(path: Path | str) -> int | None:
        found = real(path)
        inside = any(Path(path) == m or Path(path).is_relative_to(m) for m in mounts)
        return None if found is None else found + (7 if inside else 0)

    monkeypatch.setattr(cachedir, "_device", device)


def test_no_session_rule_crosses_a_mount(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """M3: a mount in an ended session is not walked, nor freed, whether it is tagged itself or
    holds a cache; and the whole directory holding one, a week on, stays."""
    recent = session_dir()
    mounted = tag(recent / "scratchpad" / "mnt")
    under = tag(recent / "scratchpad" / "other-mnt" / "target")
    ours = tag(recent / "scratchpad" / "target")
    aged(recent, 3 * HOUR)
    week = 8 * timedelta(days=1)
    old = session_dir(ended=week)
    (old / "scratchpad" / "mnt").mkdir()
    aged(old, week)
    mounts = (mounted, under.parent, old / "scratchpad" / "mnt")
    _mounted_at(monkeypatch, *mounts)
    listed: list[Path] = []
    real_scandir = os.scandir

    def scandir(path: Any = ".") -> Any:
        """Every folder any walk lists, tidy's and ``cachedir``'s alike."""
        if isinstance(path, (str, os.PathLike)):
            listed.append(Path(path))
        return real_scandir(path)

    monkeypatch.setattr(os, "scandir", scandir)

    made = planned(tmp_path)
    monkeypatch.setattr(os, "scandir", real_scandir)

    assert rules(made) == {ours: tidy.SESSION_CACHE}
    assert not [p for p in listed if any(p == m or p.is_relative_to(m) for m in mounts)]
    tidy.apply(made)
    assert (mounted / cachedir.TAG).is_file() and (under / cachedir.TAG).is_file()
    assert old.is_dir()


def test_reap_neither_frees_nor_removes_across_a_mount(
    tmp_path: Path,
    tmux: object,
    claude_on_path: Path,
    project: ProjectInfo,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """M3, reap's paths: a tagged mount in a tree, untracked, is not build output: it is the
    tree's untracked content, which keeps the tree."""
    agent = _ended_coder(project, "coder-mount")
    mounted = tag(agent.cwd / "mnt")
    _mounted_at(monkeypatch, mounted)

    assert fleet_service._untracked_caches(agent.cwd)[1] == []
    assert fleet_service._remove_untracked_caches(agent.cwd) == []
    with pytest.raises(ValueError):
        cachedir.remove_within(mounted, agent.cwd)
    report = fleet_service.reap(project)

    assert (agent.cwd, "uncommitted changes") in report.worktrees_kept
    assert (mounted / cachedir.TAG).is_file()


def test_an_ended_tree_whose_recorded_path_is_now_a_link_is_kept_by_tidy_and_reap(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    """The symlinked recorded tree: the tree moved and a link left at its path. ``git worktree
    remove`` given the link would act on wherever it points; nothing goes through it."""
    agent = _ended_coder(project, "coder-linked")
    moved = agent.cwd.parent / "coder-linked-moved"
    agent.cwd.rename(moved)
    agent.cwd.symlink_to(moved, target_is_directory=True)
    cache = tag(moved / "target")

    made = planned(tmp_path)

    assert agent.cwd not in rules(made) and cache not in rules(made)
    assert _kept_why(made, agent.cwd) == fleet_service.LINKED_TREE
    tidy.apply(made)
    report = fleet_service.reap(project)
    assert (agent.cwd, fleet_service.LINKED_TREE) in report.worktrees_kept
    assert (cache / cachedir.TAG).is_file() and (moved / ".git").is_file()


def test_a_tree_whose_status_cannot_be_read_is_kept_and_said(
    tmp_path: Path,
    tmux: object,
    claude_on_path: Path,
    project: ProjectInfo,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``_untracked_caches`` inside the try: git failing there keeps the tree, never a
    traceback."""
    agent = _ended_coder(project, "coder-unread")

    def broken(tree: Path) -> list[Path]:
        raise fleet_service.FleetError("git is not there")

    monkeypatch.setattr(tidy, "_untracked_caches", broken)

    made = planned(tmp_path)

    assert made.items == []
    assert _kept_why(made, agent.cwd) == "git could not be asked (git is not there)"


def test_a_fleet_held_row_that_cannot_be_read_holds_its_session(tmp_path: Path) -> None:
    session = session_dir()
    with store_session() as store:
        store.set_meta(f"{FLEET_HELD}{session.name}", f"agt_{'0' * 26}")
    tag(session / "scratchpad" / "target")
    aged(session, 3 * HOUR)

    assert planned(tmp_path).items == []


def test_a_live_row_no_project_lists_still_holds_its_session(tmp_path: Path) -> None:
    """A purged project's live row is listed by no project: it is read by the session's id."""
    session = session_dir()
    orphan = FleetAgent(
        id=f"agt_{uuid.uuid4().hex[:26]}",
        project_id="prj_purgedproject01",
        label="coder-orphan",
        role="coder",
        pane_id="%5",
        session_id=session.name,
        cwd=tmp_path,
        created_at=datetime.now(UTC) - 5 * HOUR,
    )
    with store_session() as store:
        store.upsert_fleet_agent(orphan)
        assert all(r.id != orphan.id for r in tidy._every_row(store))
    tag(session / "scratchpad" / "target")
    aged(session, 3 * HOUR)

    assert planned(tmp_path).items == []


def test_a_lock_made_anew_while_tidy_took_it_is_not_unlinked(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The lock inode race: the file tidy locked is the one it unlinks; one made anew at the name
    meanwhile is another mover's."""
    lock = tmp_path / "system-temp" / f"rlx-home-migration-{'d' * 16}.lock"
    lock.write_bytes(b"")
    aged(lock, 25 * HOUR)
    made = planned(tmp_path)
    assert rules(made) == {lock: tidy.MIGRATION_LOCK}
    real_held = locking.held
    fresh = tmp_path / "fresh.lock"
    fresh.write_bytes(b"another mover's\n")

    @contextmanager
    def swapping(fd: int, *, wait_s: float) -> Iterator[None]:
        with real_held(fd, wait_s=wait_s):
            os.replace(fresh, lock)
            yield

    monkeypatch.setattr(locking, "held", swapping)

    report = tidy.apply(made)

    assert report.freed == []
    assert report.not_freed == [(made.items[0], "skipped: the lock was made anew")]
    assert lock.read_bytes() == b"another mover's\n"


def test_a_folder_swapped_for_a_link_after_the_check_deletes_nothing_elsewhere(
    tmp_path: Path,
) -> None:
    """The parent swap race: the folder is pinned once checked, and what it holds then is
    compared with what was checked."""
    temp = tmp_path / "temp"
    target = tag(temp / "sub" / "target")
    elsewhere = tag(tmp_path / "elsewhere" / "sub" / "target")

    class Swapping(tidy.Roots):
        def allows(self, path: Path) -> bool:
            allowed = super().allows(path)
            os.rename(temp / "sub", temp / "sub.checked")
            (temp / "sub").symlink_to(tmp_path / "elsewhere" / "sub", target_is_directory=True)
            return allowed

    roots = Swapping(temp=frozenset({temp.resolve()}))

    with pytest.raises(tidy.Refused):
        tidy._remove(target, roots)

    assert (elsewhere / cachedir.TAG).is_file()
    assert (temp / "sub.checked" / "target" / cachedir.TAG).is_file()


def test_a_cache_inside_a_tree_reap_judges_honours_the_callers_roots(tmp_path: Path) -> None:
    """Reap's delete, run for tidy, asks tidy's roots too."""
    tree = tmp_path / "tree"
    _git("init", "-q", str(tree), cwd=tmp_path)
    target = tag(tree / "target")

    assert fleet_service._remove_untracked_caches(tree, allows=lambda path: False) == []
    assert target.is_dir()
    assert fleet_service._remove_untracked_caches(tree, allows=lambda path: True) == [target]


# --- round 5 (review #3696, manager #3697) --------------------------------------------------


def test_a_real_pause_ends_its_row_stopped_and_the_pause_alone_holds_the_session(
    tmp_path: Path, tmux: object, claude_on_path: Path, project: ProjectInfo
) -> None:
    """A fleet pause ends its row ``stopped`` (stop()'s default) with ``paused_at``. With the
    board's end and that explicit end both there, the pause alone (``_held_by``) keeps the
    session's build output."""
    agent = fleet_service.spawn(project, "coder", label="coder-paused").agent
    fleet_service.pause_agent(project, "coder-paused", grace=0)
    with store_session() as store:
        paused = store.get_fleet_agent(agent.id)
    assert paused is not None and paused.session_id
    assert paused.ended_by == "stopped" and paused.paused_at is not None
    session = session_dir(session=paused.session_id)
    _made_at(paused, datetime.now(UTC) - 5 * HOUR)  # before the board's end: no resume after
    cache = tag(session / "scratchpad" / "target")
    aged(session, 3 * HOUR)

    made = planned(tmp_path)

    assert made.items == []
    tidy.apply(made)
    assert (cache / cachedir.TAG).is_file()


def _made_at(agent: FleetAgent, when: datetime) -> None:
    """``agent``'s row made at ``when``: written in the store itself, as an upsert never moves
    ``created_at``."""
    connection = sqlite3.connect(str(paths.db_path()))
    try:
        connection.execute(
            "UPDATE fleet_agent SET created_at = ? WHERE id = ?", (when.isoformat(), agent.id)
        )
        connection.commit()
    finally:
        connection.close()
    with store_session() as store:
        found = store.get_fleet_agent(agent.id)
    assert found is not None and found.created_at == when


def _bundle(where: Path, repo: Path) -> Path:
    where.parent.mkdir(parents=True, exist_ok=True)
    _git("bundle", "create", str(where), "--all", cwd=repo)
    return where


def test_a_git_bundle_is_a_repository_tidy_never_deletes(tmp_path: Path, repo: Path) -> None:
    """A bundle is a repository in one file (manager #3697). A session holding only one is kept
    whole a week on, the bundle listed with its size; a cache holding one is no cache; a file
    that only looks like one keeps nothing."""
    week = 8 * timedelta(days=1)
    holding = session_dir(ended=week)
    bundle = _bundle(holding / "scratchpad" / "work.bundle", repo)
    lookalike = session_dir(ended=week)
    (lookalike / "scratchpad" / "notes.bundle").write_bytes(b"# v2 git bundles are files\n")
    recent = session_dir()
    cache = tag(recent / "scratchpad" / "target")
    inside = _bundle(cache / "deep" / "x.bundle", repo)
    plain = tag(recent / "scratchpad" / "plain")
    for directory, ago in ((holding, week), (lookalike, week), (recent, 3 * HOUR)):
        aged(directory, ago)

    made = planned(tmp_path)

    assert rules(made) == {lookalike: tidy.SESSION_DIR, plain: tidy.SESSION_CACHE}
    assert _kept_why(made, bundle) == tidy.NEVER_A_REPOSITORY
    assert _kept_why(made, inside) == tidy.NEVER_A_REPOSITORY
    assert [k.bytes for k in made.kept if k.path == bundle] == [cachedir.allocated_bytes(bundle)]
    tidy.apply(made)
    assert bundle.is_file() and inside.is_file() and not lookalike.exists()


def test_a_plan_without_kept_sizes_walks_no_kept_repository(tmp_path: Path, repo: Path) -> None:
    """#59's periodic job plans without them: a repository tidy never deletes is never walked
    again each pass. A person's dry run still shows them."""
    session = session_dir()
    clone = _clone(repo, session / "scratchpad" / "clone")
    aged(session, 3 * HOUR)

    sized = planned(tmp_path)
    unsized = tidy.plan(size_kept=False)

    [with_size] = [k.bytes for k in sized.kept if k.path == clone]
    assert with_size is not None and with_size > 0
    assert [k.bytes for k in unsized.kept if k.path == clone] == [None]
