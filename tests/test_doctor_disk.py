"""The Doctor's ``disk`` row (#60, the owner's disk tidy, item 4): what rlx's folders take on
disk and what tidy would free, in a sandbox home.

It reads tidy's dry run, or the last periodic pass's stamp while fresh, and never frees. It
looks for ``tidy.DISK_BUDGET_S`` at most, saying "at least" where the time ran out. asq says
rlx's own way under it, never a command (R13, manager #3817), and runs it on the Doctor's
worker, never on its UI thread. Every look is in the suite's sandbox: its home, its system
temp dir, its process table.
"""

from __future__ import annotations

import asyncio
import os
import sys
import threading
import time
from collections.abc import Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from textual.pilot import Pilot
from textual.worker import WorkerState

from aisquare.cli.ui import app as ui_app
from aisquare.cli.ui.views.doctor import COMMAND, ui_way
from aisquare.core import cachedir
from aisquare.core.config import load_config, save_config
from aisquare.core.store import store_session
from aisquare.models import CheckStatus, DoctorCheck, ProjectInfo
from aisquare.services import diagnostics, tidy
from aisquare.services import fleet as fleet_service
from aisquare.services import watch as watch_service
from tests.test_ui_shell import drive, script  # noqa: F401 — `script` is a fixture
from tests.ui_workers import settle_until

GB = 10**9
PERIODIC_PASS = tidy.periodic_pass  # the real ones: conftest stands in for the pass, and
APPLY = tidy.apply  # ``never_frees`` for apply, in every test
TAG = cachedir.SIGNATURE + b"\n# a cache, by its own word\n"

posix_only = pytest.mark.skipif(
    sys.platform == "win32",
    reason="tidy deletes nothing on native Windows in 0.10.2: no pass, so no stamp",
)


@pytest.fixture(autouse=True)
def never_frees(monkeypatch: pytest.MonkeyPatch) -> None:
    """The row looks; it never frees."""

    def forbidden(made: tidy.Plan) -> tidy.Report:
        raise AssertionError("the Doctor's disk row called tidy.apply")

    monkeypatch.setattr(tidy, "apply", forbidden)
    with store_session():  # the home made: the row looks only at one that is there
        pass


@pytest.fixture
def plans(monkeypatch: pytest.MonkeyPatch) -> list[datetime | None]:
    """The dry runs the row asks for, the real one each time."""
    asked: list[datetime | None] = []
    real = tidy.plan

    def spy(
        *, now: datetime | None = None, size_kept: bool = True, into: tidy.Plan | None = None
    ) -> tidy.Plan:
        asked.append(now)
        assert size_kept is False, "the Doctor sizes no kept repository"
        return real(now=now, size_kept=size_kept, into=into)

    monkeypatch.setattr(tidy, "plan", spy)
    return asked


def _use(**fields: Any) -> tidy.DiskUse:
    given: dict[str, Any] = {
        "worktrees": 20 * GB,
        "build_caches": 6 * GB,
        "session_temp": 8 * GB,
        "use_whole": True,
        "would_free": 0,
        "free_whole": True,
    }
    return tidy.DiskUse(**{**given, **fields})


def _row(
    monkeypatch: pytest.MonkeyPatch,
    use: tidy.DiskUse,
    *,
    watching: bool = True,
    every: float = 60,
    deletes: bool = True,
) -> DoctorCheck:
    monkeypatch.setattr(tidy, "disk_use", lambda **kwargs: use)
    monkeypatch.setattr(tidy, "DELETES", deletes)
    monkeypatch.setattr(
        watch_service,
        "watch_status",
        lambda: watch_service.Status(4242 if watching else None, None),
    )
    config = load_config()
    config.tidy.every_minutes = every
    save_config(config)
    return diagnostics._check_disk()


def _onboarded(tmp_path: Path) -> Path:
    """A sandbox project and its worktrees folder: one tree with a tagged cache in it."""
    root = tmp_path / "proj"
    root.mkdir()
    with store_session() as store:
        store.onboard_project(ProjectInfo(id="prj_doctordisk00001", root=root))
    tree = root / tidy.WORKTREES_DIR / "coder-1"
    tree.mkdir(parents=True)
    (tree / "notes.md").write_bytes(b"n" * 8192)
    cache = tree / "target"
    cache.mkdir()
    (cache / cachedir.TAG).write_bytes(TAG)
    (cache / "out.bin").write_bytes(b"x" * 65536)
    return root / tidy.WORKTREES_DIR


def _stamp(at: datetime, **totals: Any) -> None:
    tidy._write_stamp({"last_pass_at": at.isoformat(timespec="seconds"), **totals})


# --- the row, by the real look ---------------------------------------------------------------


def test_nothing_to_free_is_ok_with_the_sizes_and_no_fix(
    tmp_path: Path, plans: list[datetime | None]
) -> None:
    worktrees = _onboarded(tmp_path)

    row = diagnostics._check_disk()

    assert row.name == "disk" and row.status is CheckStatus.ok and row.fix is None, row
    use = tidy.disk_use()
    assert use.use_whole and use.free_whole and use.would_free == 0 and use.as_of is None
    assert use.worktrees == cachedir.allocated_bytes(worktrees / "coder-1")
    assert use.build_caches == cachedir.allocated_bytes(worktrees / "coder-1" / "target") > 0
    assert row.detail == (
        f"rlx's folders take {tidy.size_text(use.total)}: worktrees "
        f"{tidy.size_text(use.worktrees)} (build caches {tidy.size_text(use.build_caches)} of "
        f"it), session and agent temp {tidy.size_text(use.session_temp)}; tidy would free 0 B"
    )
    assert len(plans) == 2, "a dry run each look: no stamp yet"


def test_what_the_dry_run_finds_is_what_the_row_says_tidy_would_free(
    tmp_path: Path, plans: list[datetime | None]
) -> None:
    """The real rules: a stale, free migration lock in the sandbox's system temp dir."""
    system = tmp_path / "system-temp"
    system.mkdir(exist_ok=True)
    lock = system / f"rlx-home-migration-{'e' * 16}.lock"
    lock.write_bytes(b"l" * 5000)
    old = time.time() - 2 * 86400
    os.utime(lock, (old, old))

    use = tidy.disk_use()

    assert use.would_free == cachedir.allocated_bytes(lock) > 0 and use.free_whole
    assert lock.exists(), "a look frees nothing"
    assert plans, "the dry run"


@pytest.mark.skipif(
    sys.platform == "win32" or not hasattr(os, "getuid") or os.getuid() == 0,
    reason="needs a mode that denies reading: not as root, nor on Windows",
)
def test_a_root_that_cannot_be_read_is_named_and_warns(tmp_path: Path) -> None:
    worktrees = _onboarded(tmp_path)
    worktrees.chmod(0)
    try:
        row = diagnostics._check_disk()
    finally:
        worktrees.chmod(0o700)

    named = os.path.realpath(worktrees)
    assert row.status is CheckStatus.warn, row
    assert f"{named} could not be read, so nothing in it was measured" in row.detail, row
    assert row.fix is not None and named in row.fix
    assert row.ui_fix == f"Make {named} readable by you: nothing in it is measured or freed"
    assert not COMMAND.search(row.ui_fix)


# --- what it says ----------------------------------------------------------------------------


def test_gbs_to_free_warn_with_tidy_for_the_shell_and_rlxs_own_way_in_asq(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    row = _row(monkeypatch, _use(would_free=int(9.4 * GB)))

    assert row.status is CheckStatus.warn and row.fix == "rlx tidy", row
    assert row.detail == (
        "rlx's folders take 28.0 GB: worktrees 20.0 GB (build caches 6.0 GB of it), session "
        "and agent temp 8.0 GB; tidy would free 9.4 GB"
    )
    assert row.ui_fix == "rlx frees it by itself within the hour"
    assert ui_way(row, {}) == row.ui_fix
    assert _row(monkeypatch, _use(would_free=GB - 1)).status is CheckStatus.ok
    assert _row(monkeypatch, _use(would_free=GB)).status is CheckStatus.warn


@pytest.mark.parametrize(
    ("fields", "said"),
    [
        (
            {
                "use_whole": False,
                "trees_whole": False,
                "worktrees": 3 * GB,
                "build_caches": GB,
                "session_temp": 0,
            },
            "rlx's folders take at least 3.0 GB: worktrees at least 3.0 GB (build caches at "
            "least 1.0 GB of it), session and agent temp not reached in time; tidy would free 0 B",
        ),
        (
            {
                "use_whole": False,
                "trees_whole": False,
                "worktrees": 3 * GB,
                "build_caches": 0,
                "session_temp": 0,
            },
            "rlx's folders take at least 3.0 GB: worktrees at least 3.0 GB, session and agent "
            "temp not reached in time; tidy would free 0 B",
        ),
        (
            {"use_whole": False, "session_temp": 2 * GB},
            "rlx's folders take at least 22.0 GB: worktrees 20.0 GB (build caches 6.0 GB of it), "
            "session and agent temp at least 2.0 GB; tidy would free 0 B",
        ),
        (
            {"use_whole": False, "session_temp": 0},
            "rlx's folders take at least 20.0 GB: worktrees 20.0 GB (build caches 6.0 GB of it), "
            "session and agent temp not reached in time; tidy would free 0 B",
        ),
        (
            {"use_whole": False, "worktrees": 0, "build_caches": 0, "session_temp": 0},
            "rlx's folders: worktrees 0 B (build caches 0 B of it), session and agent temp not "
            "reached in time; tidy would free 0 B",
        ),
        (
            {
                "use_whole": False,
                "trees_whole": False,
                "worktrees": 0,
                "build_caches": 0,
                "session_temp": 0,
                "free_whole": False,
            },
            "rlx's folders: not reached in time; what tidy would free was not reached in time",
        ),
    ],
    ids=[
        "in the worktrees",
        "in the worktrees, no cache yet",
        "in the temp",
        "before the temp",
        "before the temp, no worktree",
        "before anything",
    ],
)
def test_a_look_cut_short_says_what_it_never_reached_and_never_at_least_0_b(
    monkeypatch: pytest.MonkeyPatch, fields: dict[str, Any], said: str
) -> None:
    """Reviewer #4085, NIT 1: past the budget, what was measured is "at least" so much, and
    what the look never reached (the temp roots are measured after the worktrees) is said so.
    Never "at least 0 B"."""
    row = _row(monkeypatch, _use(**fields))

    assert row.detail == said
    assert "at least 0 B" not in row.detail


def test_the_disk_row_waits_with_the_late_rows_beside_the_watcher() -> None:
    """Reviewer #4085, NIT 2: it can warn (1 GB to free), and the not-ok rows are read in
    doctor order (the fleet sidebar showed the first three), so it comes after the actionable
    machine checks, with the late rows, right after the watcher whose periodic tidy frees what
    it counts."""
    names = [check.name for check in diagnostics.doctor()]

    disk = names.index("disk")
    for actionable in ("claude-code", "tmux", "gh", "fleet", "fleet terminal"):
        assert names.index(actionable) < disk, (actionable, names)
    assert names[disk - 1] == "fleet watcher" and names[disk + 1] == "asq instances", names


@pytest.mark.parametrize(
    ("watching", "every", "deletes", "said"),
    [
        (True, 60, True, "rlx frees it by itself within the hour"),
        (True, 30, True, "rlx frees it by itself within 30 minutes"),
        (True, 120, True, "rlx frees it by itself within 2 hours"),
        (True, 90, True, "rlx frees it by itself within 90 minutes"),
        (False, 60, True, "The fleet watcher isn't running, so rlx doesn't free it by itself"),
        (True, 0, True, "rlx's own tidy is off in its settings, so nothing frees it by itself"),
        (True, 60, False, "rlx only reports this here: on native Windows it frees nothing"),
    ],
)
def test_asq_says_rlxs_own_way_and_never_a_command(
    monkeypatch: pytest.MonkeyPatch, watching: bool, every: float, deletes: bool, said: str
) -> None:
    """R13 (manager #3817): never a shell, never a command, in the app. The watcher frees it;
    while it isn't running, or the job is off, that is said; native Windows only reports,
    with no WSL advice in the app (the shell's fix says it)."""
    row = _row(
        monkeypatch, _use(would_free=3 * GB), watching=watching, every=every, deletes=deletes
    )

    assert row.ui_fix == said and ui_way(row, {}) == said
    assert not COMMAND.search(said) and not COMMAND.search(row.detail)
    assert row.fix is not None and COMMAND.search(row.fix), "the shell keeps its command"
    if not deletes:
        assert row.fix == "rlx tidy in WSL, where the fleet runs: on native Windows it only reports"


# --- fast: a fresh stamp, or a budget --------------------------------------------------------


@pytest.fixture
def india() -> Iterator[None]:
    """Local time five and a half hours off UTC, so a time said in UTC shows; put back, and
    the process's zone read again, after."""
    if not hasattr(time, "tzset"):
        pytest.skip("time.tzset is POSIX's")
    before = os.environ.get("TZ")
    os.environ["TZ"] = "Asia/Kolkata"
    time.tzset()
    try:
        yield
    finally:
        if before is None:
            os.environ.pop("TZ", None)
        else:
            os.environ["TZ"] = before
        time.tzset()


@posix_only
def test_a_fresh_stamp_is_read_and_no_dry_run_runs(
    plans: list[datetime | None], india: None
) -> None:
    """The last pass began within twice ``every_minutes`` and freed all it planned: tidy
    would free nothing more as of then, said in local time."""
    last = datetime.now(UTC).replace(microsecond=0) - timedelta(minutes=30)
    _stamp(last, freed_bytes=3 * GB, not_freed=0)

    row = diagnostics._check_disk()

    when = last.astimezone().strftime("%H:%M")
    assert when != last.strftime("%H:%M"), "control: local time is not UTC here"
    assert row.detail.endswith(
        f"tidy would free 0 B as of its last pass at {when}, which freed 3.0 GB"
    ), row.detail
    assert row.status is CheckStatus.ok and plans == []


@posix_only
@pytest.mark.parametrize(
    ("ago", "every", "totals"),
    [
        (timedelta(minutes=121), 60, {"freed_bytes": 0, "not_freed": 0}),
        (timedelta(minutes=10), 0, {"freed_bytes": 0, "not_freed": 0}),
        (timedelta(minutes=10), 60, {"failed": True}),
        (timedelta(minutes=10), 60, {"freed_bytes": 0}),
        (timedelta(minutes=10), 60, {"freed_bytes": 0, "not_freed": 1}),
        (timedelta(minutes=10), 60, {"freed_bytes": 0, "not_freed": False}),
        (-timedelta(minutes=10), 60, {"freed_bytes": 0, "not_freed": 0}),
    ],
    ids=[
        "stale",
        "the job off",
        "failed",
        "says not what it left",
        "left something",
        "not a count",
        "from the future",
    ],
)
def test_a_stamp_that_is_not_fresh_is_not_read(
    plans: list[datetime | None], ago: timedelta, every: float, totals: dict[str, Any]
) -> None:
    """Stale past twice ``every_minutes``; ``0`` = off counts as stale (manager #3817); a
    failed pass, a stamp that does not say what was left or says something was (reviewer
    #4085, MINOR 2), a clock set back: a dry run."""
    config = load_config()
    config.tidy.every_minutes = every
    save_config(config)
    _stamp(datetime.now(UTC) - ago, **totals)

    use = tidy.disk_use()

    assert use.as_of is None and len(plans) == 1


@posix_only
def test_what_the_last_pass_declined_is_not_what_tidy_would_free(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, plans: list[datetime | None]
) -> None:
    """Reviewer #4085, MINOR 2, its probe: the last pass's re-check declined a 5 GB tree (its
    agent back since the plan), so the pass left it, and tidy would not free it. That pass's
    stamp is not fresh: a dry run says what tidy would free now, never 5 GB "as of" the pass,
    nor a warning to free it."""
    (tmp_path / "system-temp").mkdir(exist_ok=True)  # the sandbox's, where the pass's lock is
    tree = tmp_path / "proj" / tidy.WORKTREES_DIR / "coder-2"
    tree.mkdir(parents=True)
    declined = tidy.Plan()
    declined.add(
        [tidy.Item(tidy.ENDED_WORKTREE, tree, 5 * GB, None)],
        lambda: "skipped: the agent is paused or back since the plan",
    )
    with monkeypatch.context() as passing:  # the pass alone frees; the row's look never does
        passing.setattr(tidy, "plan", lambda **kwargs: declined)
        passing.setattr(tidy, "apply", APPLY)
        passed = PERIODIC_PASS(datetime.now(UTC))
    assert passed is not None and passed.freed_bytes == 0 and passed.not_freed == 1
    assert tree.exists() and tidy.read_stamp()["not_freed"] == 1

    row = diagnostics._check_disk()

    assert len(plans) == 1, "a dry run, not the stamp"
    assert row.status is CheckStatus.ok and row.fix is None, row
    assert row.detail.endswith("; tidy would free 0 B"), row.detail


def test_the_look_stops_at_its_budget_and_says_at_least(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A dry run slowed (a sleep, never load) past the budget ends there: what it had found
    stands as "at least", and the row returns within the budget."""
    _onboarded(tmp_path)
    monkeypatch.setattr(tidy, "DISK_BUDGET_S", 0.3)

    def slowed(
        *, now: datetime | None = None, size_kept: bool = True, into: tidy.Plan | None = None
    ) -> tidy.Plan:
        assert into is not None
        into.add([tidy.Item(tidy.SESSION_CACHE, tmp_path / "found", 3 * GB, None)], lambda: None)
        for _ in range(300):  # 3 s, unless the budget ends it: a look with none never hangs
            time.sleep(0.01)
            cachedir.in_time()
        return into

    monkeypatch.setattr(tidy, "plan", slowed)

    began = time.monotonic()
    row = diagnostics._check_disk()
    took = time.monotonic() - began

    assert took < 0.3 + 1.0, took
    assert row.status is CheckStatus.warn
    assert row.detail == ("rlx's folders: not reached in time; tidy would free at least 3.0 GB"), (
        "no time left to size them, and never 'at least 0 B'"
    )


@pytest.mark.parametrize("trees", [False, True], ids=["no ended trees", "none to take"])
def test_the_budget_bounds_the_real_dry_runs_git_steps(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, trees: bool
) -> None:
    """Reviewer #4085, MINOR 1: the budget bounds the dry run's git steps, not only its walks.
    The REAL plan over three projects, each project's git steps slowed (a sleep, never load)
    until the look's deadline has passed: that one finishes, and nothing after it starts,
    neither the next project's steps nor the branch sweep. It fails with tidy's own
    ``in_time()`` gone: the one before each project (no ended trees, so no sweep comes
    between), or the one before the sweep (ended trees, none to take)."""
    for n in range(3):
        root = tmp_path / f"proj-{n}"
        root.mkdir()
        with store_session() as store:
            store.onboard_project(ProjectInfo(id=f"prj_doctordiskgit{n:02}", root=root))
    steps: list[str] = []

    def slowed(store: Any, project: ProjectInfo) -> fleet_service.EndedTrees | None:
        steps.append(f"the ended trees of {project.id}")
        deadline = cachedir._DEADLINE.get()
        assert deadline is not None, "asked within the look's deadline"
        for _ in range(500):  # 5 s at most: a look with no deadline never hangs
            if time.monotonic() > deadline:
                break
            time.sleep(0.01)
        if not trees:
            return None
        return fleet_service.EndedTrees(
            root=project.root,
            read_at=datetime.now(UTC),
            default="main",
            common=project.root / ".git",
            candidates=[],
            placed=[],
            owners={},
            in_use=set(),
        )

    def sweep(*args: Any, **kwargs: Any) -> tuple[dict[str, str], list[str]]:
        steps.append("the branch sweep")
        return {}, []

    monkeypatch.setattr(fleet_service, "ended_trees", slowed)
    monkeypatch.setattr(fleet_service, "sweep_plan", sweep)
    monkeypatch.setattr(tidy, "_command_lines", lambda: [])  # no ps before the first project
    monkeypatch.setattr(tidy, "DISK_BUDGET_S", 1.0)

    use = tidy.disk_use()

    assert len(steps) == 1 and steps[0].startswith("the ended trees of "), steps
    assert not use.free_whole


def test_a_walk_past_its_deadline_stops_in_this_thread_alone(tmp_path: Path) -> None:
    """``cachedir.walking_until`` is a context variable: another thread (the watcher's pass)
    walks on, with no deadline."""
    tree = tmp_path / "tree"
    tree.mkdir()
    (tree / "a").write_bytes(b"x")
    found: list[int] = []
    with cachedir.walking_until(time.monotonic() - 1):
        with pytest.raises(cachedir.OutOfTime):
            cachedir.allocated_bytes(tree)
        other = threading.Thread(target=lambda: found.append(cachedir.allocated_bytes(tree)))
        other.start()
        other.join(10)
    assert found and found[0] > 0
    assert cachedir.allocated_bytes(tree) == found[0], "no deadline once the block ends"


# --- asq: on the Doctor's worker, never its UI thread ----------------------------------------


def test_asq_looks_on_the_doctors_worker_and_its_view_never_waits(
    script: Any,  # noqa: F811 — pytest resolves fixtures by NAME, so the import must keep it
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Manager #3817 (2): the look (2 s at most) runs where the Doctor's other rows do, on its
    worker. Held for the test (a wait, never load), the view goes on: its timer ticks."""
    started, release = threading.Event(), threading.Event()
    on_main: list[bool] = []

    def held(**kwargs: Any) -> tidy.DiskUse:
        on_main.append(threading.current_thread() is threading.main_thread())
        started.set()
        release.wait(10)
        return _use(would_free=3 * GB)

    monkeypatch.setattr(tidy, "disk_use", held)

    async def fn(pilot: Pilot[None]) -> tuple[int, bool, list[DoctorCheck] | None]:
        app = pilot.app
        assert isinstance(app, ui_app.FleetApp)
        ticks: list[float] = []
        app._doctor = lambda: [diagnostics._check_disk()]
        try:
            app.run_doctor()
            assert await asyncio.to_thread(started.wait, 10)
            app.set_interval(0.02, lambda: ticks.append(time.monotonic()))
            await asyncio.sleep(0.5)
            worker = app._doctor_worker
            still = worker is not None and worker.state is WorkerState.RUNNING
        finally:
            release.set()
        await settle_until(
            app,
            lambda: (
                app._doctor_worker is not None and app._doctor_worker.state is WorkerState.SUCCESS
            ),
        )
        worker = app._doctor_worker
        result = worker.result if worker is not None else None
        return len(ticks), still, result[1] if result else None

    ticked, still, rows = drive(fn)

    assert still, "the look was still held"
    assert ticked >= 5, "the view's own timer ran meanwhile"
    assert on_main == [False], "never on the UI thread"
    assert rows is not None and [row.name for row in rows] == ["disk"]
