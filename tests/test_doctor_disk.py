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
from aisquare.services import watch as watch_service
from tests.test_ui_shell import drive, script  # noqa: F401 — `script` is a fixture
from tests.ui_workers import settle_until

GB = 10**9
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
    """The last pass began within twice ``every_minutes``: what it could not free is what
    tidy would free, as of then, in local time."""
    last = datetime.now(UTC).replace(microsecond=0) - timedelta(minutes=30)
    _stamp(last, freed_bytes=3 * GB, not_freed_bytes=2 * GB)

    row = diagnostics._check_disk()

    when = last.astimezone().strftime("%H:%M")
    assert when != last.strftime("%H:%M"), "control: local time is not UTC here"
    assert row.detail.endswith(
        f"tidy would free 2.0 GB as of its last pass at {when}, which freed 3.0 GB"
    ), row.detail
    assert row.status is CheckStatus.warn and plans == []


@posix_only
@pytest.mark.parametrize(
    ("ago", "every", "totals"),
    [
        (timedelta(minutes=121), 60, {"freed_bytes": 0, "not_freed_bytes": 0}),
        (timedelta(minutes=10), 0, {"freed_bytes": 0, "not_freed_bytes": 0}),
        (timedelta(minutes=10), 60, {"failed": True}),
        (timedelta(minutes=10), 60, {"freed_bytes": 0}),
        (-timedelta(minutes=10), 60, {"freed_bytes": 0, "not_freed_bytes": 0}),
    ],
    ids=["stale", "the job off", "failed", "from before #60", "from the future"],
)
def test_a_stamp_that_is_not_fresh_is_not_read(
    plans: list[datetime | None], ago: timedelta, every: float, totals: dict[str, Any]
) -> None:
    """Stale past twice ``every_minutes``; ``0`` = off counts as stale (manager #3817); a
    failed pass, a stamp that does not say what was left, a clock set back: a dry run."""
    config = load_config()
    config.tidy.every_minutes = every
    save_config(config)
    _stamp(datetime.now(UTC) - ago, **totals)

    use = tidy.disk_use()

    assert use.as_of is None and len(plans) == 1


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
    assert "tidy would free at least 3.0 GB" in row.detail, row.detail
    assert "rlx's folders take at least" in row.detail, "no time left to size them"


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
