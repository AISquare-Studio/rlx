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
import tempfile
import time
import uuid
from collections.abc import Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import cachedir, claude_accounts, paths
from aisquare.core.config import TidySettings
from aisquare.core.locking import holding
from aisquare.core.store import store_session
from aisquare.models import FleetAgent, ProjectInfo, TeamSession
from aisquare.services import fleet as fleet_service
from aisquare.services import tidy
from tests import test_fleet_service as fleet_suite
from tests.test_fleet_pause_resume import _claim
from tests.test_fleet_service import _add_task, _git, _stale_board_session

tmux = fleet_suite.tmux
claude_on_path = fleet_suite.claude_on_path
repo = fleet_suite.repo
project = fleet_suite.project

TAG = cachedir.SIGNATURE + b"\n# a cache, by its own word\n"
SYSTEM_TEMP_DIRS = tidy._system_temp_dirs
"""The real lookup, kept before :func:`sandboxed` replaces it: asked only for a path."""
HOUR = timedelta(hours=1)


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


def session_dir(slot: int = 2, session: str | None = None) -> Path:
    """An ended Claude session's temp under the accounts' temp root, as Claude Code lays it
    out under the ``CLAUDE_CODE_TMPDIR`` rlx gives a managed slot."""
    found = (
        claude_accounts.tmp_root()
        / str(slot)
        / "claude-501"
        / "-Users-someone-code-repo"
        / (session or str(uuid.uuid4()))
    )
    (found / "scratchpad").mkdir(parents=True)
    (found / "scratchpad" / "notes.md").write_text("what the reviewer wrote\n", encoding="utf-8")
    return found


def row(project: ProjectInfo, session: str, **fields: object) -> FleetAgent:
    """A fleet row naming ``session``, written to the sandbox store."""
    agent = FleetAgent(
        id=f"agt_{uuid.uuid4().hex[:26]}",
        project_id=project.id,
        label=str(fields.pop("label", "reviewer-1")),
        role=str(fields.pop("role", "reviewer")),
        pane_id="%9",
        session_id=session,
        cwd=project.root,
        created_at=datetime.now(UTC) - 5 * HOUR,
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
    row(project, paused.name, ended_at=datetime.now(UTC) - 3 * HOUR, paused_at=datetime.now(UTC))
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


def test_a_scratch_worktree_with_a_commit_no_ref_has_is_kept_and_named_its_cache_freed(
    tmp_path: Path, repo: Path, project: ProjectInfo
) -> None:
    session = session_dir()
    checkout = _scratch_worktree(repo, session / "scratchpad" / "wt")
    (checkout / "probe.txt").write_text("a probe\n", encoding="utf-8")
    _git("add", "probe.txt", cwd=checkout)
    _git("commit", "-q", "-m", "only here", cwd=checkout)
    sha = _git("rev-parse", "HEAD", cwd=checkout)
    target = tag(checkout / "target")
    _git("config", "status.showUntrackedFiles", "no", cwd=checkout)  # hides nothing from tidy
    aged(session, 3 * HOUR)

    made = planned(tmp_path)

    assert rules(made) == {target: tidy.SESSION_CACHE}
    (kept,) = [k for k in made.kept if k.path == checkout]
    assert kept.rule == tidy.SESSION_CHECKOUT and sha[:12] in kept.why, kept
    tidy.apply(made)
    assert not target.exists() and (checkout / "probe.txt").is_file()
    assert str(checkout) in _git("worktree", "list", cwd=repo)


def test_a_clean_scratch_worktree_goes_through_git_and_leaves_the_repos_list(
    tmp_path: Path, repo: Path, project: ProjectInfo
) -> None:
    session = session_dir()
    checkout = _scratch_worktree(repo, session / "scratchpad" / "wt")
    tag(checkout / "target")  # untracked build output on a branch with no .gitignore for it
    aged(session, 3 * HOUR)

    made = planned(tmp_path)

    assert rules(made) == {checkout: tidy.SESSION_CHECKOUT}
    report = tidy.apply(made)
    assert [i.path for i in report.freed] == [checkout] and report.not_freed == []
    assert not checkout.exists()
    assert str(checkout) not in _git("worktree", "list", cwd=repo)
    assert (session / "scratchpad" / "notes.md").is_file()


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
    assert [(k.path, k.why) for k in made.kept] == [(checkout, "uncommitted changes")]


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


@pytest.mark.parametrize("age", [3 * HOUR, 8 * timedelta(days=1)], ids=["3 h", "8 d"])
def test_a_clone_with_a_commit_on_no_remote_is_kept_however_old_its_session(
    tmp_path: Path, repo: Path, age: timedelta
) -> None:
    """Tester #3507's data loss: the commit sat on the clone's own ``main``, which goes with it."""
    session = session_dir()
    clone = _clone(repo, session / "scratchpad" / "clone")
    sha = _commit(clone, "only-here.txt")
    aged(session, age)

    made = planned(tmp_path)

    assert clone not in rules(made) and session not in rules(made)
    why = _kept_why(made, clone)
    assert sha[:12] in why and "on no remote" in why and "push" in why, why
    tidy.apply(made)
    assert _git("cat-file", "-t", sha, cwd=clone) == "commit"


def test_a_clone_whose_other_branch_has_a_commit_on_no_remote_is_kept(
    tmp_path: Path, repo: Path
) -> None:
    session = session_dir()
    clone = _clone(repo, session / "scratchpad" / "clone")
    _git("switch", "-q", "-c", "side", cwd=clone)
    sha = _commit(clone, "side.txt")
    _git("switch", "-q", "main", cwd=clone)  # HEAD itself is on the remote
    aged(session, 3 * HOUR)

    why = _kept_why(planned(tmp_path), clone)

    assert sha[:12] in why and "push" in why, why


def test_a_clone_with_a_stash_is_kept(tmp_path: Path, repo: Path) -> None:
    (repo / "tracked.txt").write_text("v1\n", encoding="utf-8")
    _git("add", "tracked.txt", cwd=repo)
    _git("commit", "-q", "-m", "tracked", cwd=repo)
    session = session_dir()
    clone = _clone(repo, session / "scratchpad" / "clone")
    (clone / "tracked.txt").write_text("work in progress\n", encoding="utf-8")
    _git("stash", "-q", cwd=clone)
    aged(session, 3 * HOUR)

    assert _kept_why(planned(tmp_path), clone).startswith("a stash"), "kept for its stash"


def test_a_worktree_whose_repository_is_inside_the_session_counts_only_remotes(
    tmp_path: Path, repo: Path
) -> None:
    """Its commit is on a branch of a repository that goes with the session: no remote has it."""
    session = session_dir()
    inner = _clone(repo, session / "scratchpad" / "repo")
    tree = session / "scratchpad" / "wt"
    _git("worktree", "add", "-q", "-b", "feature", str(tree), cwd=inner)
    sha = _commit(tree, "feature.txt")
    aged(session, 8 * timedelta(days=1))

    made = planned(tmp_path)

    assert session not in rules(made) and tree not in rules(made)
    why = _kept_why(made, tree)
    assert sha[:12] in why and "on no remote" in why, why


def test_a_clone_with_everything_on_its_remote_goes(tmp_path: Path, repo: Path) -> None:
    """The control: nothing of it lives only there."""
    session = session_dir()
    clone = _clone(repo, session / "scratchpad" / "clone")
    aged(session, 3 * HOUR)

    made = planned(tmp_path)

    assert rules(made) == {clone: tidy.SESSION_CHECKOUT}
    tidy.apply(made)
    assert not clone.exists()


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
    tmp_path: Path, repo: Path, project: ProjectInfo
) -> None:
    week_old = session_dir()
    aged(week_old, 8 * timedelta(days=1))
    days_old = session_dir()
    aged(days_old, 3 * timedelta(days=1))
    holding_work = session_dir()
    checkout = _scratch_worktree(repo, holding_work / "scratchpad" / "wt")
    (checkout / "only.txt").write_text("x\n", encoding="utf-8")
    _git("add", "only.txt", cwd=checkout)
    _git("commit", "-q", "-m", "kept work", cwd=checkout)
    aged(holding_work, 8 * timedelta(days=1))

    made = planned(tmp_path)

    assert rules(made) == {week_old: tidy.SESSION_DIR}
    tidy.apply(made)
    assert not week_old.exists()
    assert (days_old / "scratchpad" / "notes.md").is_file()
    assert (checkout / "only.txt").is_file() and checkout in {k.path for k in made.kept}


# --- 7. the per-agent TMPDIR ------------------------------------------------------------------


def test_an_ended_agents_tmpdir_goes_and_a_live_paused_or_unnamed_one_stays(
    tmp_path: Path, project: ProjectInfo
) -> None:
    gone = row(project, str(uuid.uuid4()), ended_at=datetime.now(UTC) - 3 * HOUR)
    live = row(project, str(uuid.uuid4()), label="coder-1", role="coder")
    paused = row(
        project, str(uuid.uuid4()), ended_at=datetime.now(UTC), paused_at=datetime.now(UTC)
    )
    unnamed = paths.agent_tmp_root() / "abc234"  # a dir no row names: never judged
    for directory in (*(paths.agent_tmp_dir(a.id) for a in (gone, live, paused)), unnamed):
        tag(directory / "pytest-of-someone")
        aged(directory, 3 * HOUR)

    made = planned(tmp_path)

    assert rules(made) == {paths.agent_tmp_dir(gone.id): tidy.AGENT_TMPDIR}
    tidy.apply(made)
    assert not paths.agent_tmp_dir(gone.id).exists()
    assert paths.agent_tmp_dir(live.id).is_dir() and paths.agent_tmp_dir(paused.id).is_dir()
    assert unnamed.is_dir()


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
    checkout = _scratch_worktree(repo, ended / "scratchpad" / "wt")
    cache = tag(ended / "scratchpad" / "pytest-cache")
    aged(ended, 3 * HOUR)
    old = session_dir()
    aged(old, 8 * timedelta(days=1))
    lock = tmp_path / "system-temp" / f"rlx-home-migration-{'f' * 16}.lock"
    lock.write_bytes(b"")
    aged(lock, 25 * HOUR)
    watched = (claude_accounts.tmp_root(), paths.agent_tmp_root(), tmp_path / "system-temp")
    before = set().union(*(every_path(root) for root in watched))

    dry = planned(tmp_path)
    report = tidy.apply(dry)

    listed = [item.path for item in dry.items]
    assert sorted(listed) == sorted([checkout, cache, old, lock])
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


# --- 11. links and roots ----------------------------------------------------------------------


def test_a_link_out_of_a_session_is_never_followed(tmp_path: Path) -> None:
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "precious.txt").write_text("not rlx's\n", encoding="utf-8")
    old = session_dir()
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
    assert made.roots.temp == {
        claude_accounts.tmp_root().resolve(),
        paths.agent_tmp_root().resolve(),
    }


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
    assert paths.agent_tmp_root().resolve().is_relative_to(tmp_path.resolve())
