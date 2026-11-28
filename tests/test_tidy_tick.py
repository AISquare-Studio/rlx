"""#59: tidy's periodic job, which ``aisquare watch`` runs from its tick (disk tidy 3/5).

At most every ``[tidy] every_minutes`` (60; ``0`` is off), one pass at a time on the machine,
quiet: one plain note on one board, and only when a pass frees 1 GB or more. Its own module
holds all of it (``services.tidy.periodic_pass``), written as a periodic job that 0.10.3's
rlxd scheduler can carry with the same setting.

Every test is in the suite's sandbox: the home, the system temp dir (``no_tidy_of_the_machine``)
and the process table. Where the rules don't matter, ``tidy.plan`` is faked with items whose
files are in ``tmp_path`` and whose sizes are said, not written; ``tidy.apply`` is the real one.
"""

from __future__ import annotations

import contextlib
import errno
import json
import os
import signal
import sys
import threading
import time
from collections.abc import Iterator
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from pydantic import ValidationError
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import locking, paths
from aisquare.core.config import TidySettings, WatchSettings, load_config, save_config
from aisquare.core.store import store_session
from aisquare.models import ProjectInfo, TeamSession
from aisquare.services import fleet as fleet_service
from aisquare.services import home_migration, remote_needs, tidy
from aisquare.services import team as team_service
from aisquare.services import watch as watch_service
from tests.test_remote_needs import Fleet, _sources

PERIODIC_PASS = tidy.periodic_pass
PASS_DUE = tidy.pass_due
"""The real job, kept before the suite's fixture turns it off for every other test's tick."""
T0 = datetime(2026, 11, 27, 12, 0, tzinfo=UTC)
GB = 10**9
BOARD_A = "prj_tidytickaaaa01"
BOARD_B = "prj_tidytickbbbb01"

posix_only = pytest.mark.skipif(
    sys.platform == "win32",
    reason="tidy deletes nothing on native Windows in 0.10.2 (manager #3647): no pass runs there",
)


@dataclass
class Plans:
    """The faked ``tidy.plan``: what each plan holds (rule, bytes said, board), and when each
    was asked for."""

    items: list[tuple[str, int, str | None]] = field(default_factory=list)
    calls: list[datetime | None] = field(default_factory=list)
    sized: list[bool] = field(default_factory=list)
    """Each plan's ``size_kept``."""


@pytest.fixture(autouse=True)
def the_real_job(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    """The job back, and the sandbox's system temp dir made, where its lock is. No pass a
    watcher started outlives its test: each is let finish."""
    monkeypatch.setattr(tidy, "periodic_pass", PERIODIC_PASS)
    monkeypatch.setattr(tidy, "pass_due", PASS_DUE)
    system = tmp_path / "system-temp"
    system.mkdir(exist_ok=True)
    yield system
    for thread in threading.enumerate():
        if thread.name == watch_service.TIDY_THREAD:
            thread.join(10)
            assert not thread.is_alive(), "a pass a watcher started outlived its test"


@pytest.fixture
def plans(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Plans:
    made = Plans()

    def plan(*, now: datetime | None = None, size_kept: bool = True) -> tidy.Plan:
        made.calls.append(now)
        made.sized.append(size_kept)
        result = tidy.Plan()
        for n, (rule, size, board) in enumerate(made.items):
            path = tmp_path / "freed" / f"pass-{len(made.calls)}-{n}"
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(b"x")
            result.add([tidy.Item(rule, path, size, board)], _unlinker(path))
        return result

    monkeypatch.setattr(tidy, "plan", plan)
    return made


def _unlinker(path: Path) -> Any:
    def run() -> str | None:
        path.unlink()
        return None

    return run


@pytest.fixture
def nudges(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    calls: list[str] = []

    def record(project_id: str, *, reason: str, board: str | None = None) -> bool:
        calls.append(project_id)
        return True

    monkeypatch.setattr(fleet_service, "nudge_manager", record)
    return calls


@pytest.fixture
def boards(tmp_path: Path) -> list[str]:
    with store_session() as store:
        for board in (BOARD_A, BOARD_B):
            root = tmp_path / board
            root.mkdir()
            store.onboard_project(ProjectInfo(id=board, root=root))
    return [BOARD_A, BOARD_B]


def _notes(board: str) -> list[Any]:
    with store_session() as store:
        return [e for e in store.recent_events(board, limit=50) if e.kind == "note"]


def _watcher() -> watch_service.Watcher:
    fleet = Fleet(agents=[])
    return watch_service.Watcher(
        WatchSettings(auto_stop=False),
        needs_sources=lambda: replace(_sources(fleet), list_projects=list),
        stale=False,
    )


def _settle(watcher: watch_service.Watcher) -> None:
    """The pass ``watcher`` started, if any, let finish."""
    job = watcher.tidy_job
    if job is not None and job.thread is not None:
        job.thread.join(10)
        assert not job.thread.is_alive()


def _pass_of(watcher: watch_service.Watcher, now: datetime) -> watch_service.TickReport:
    """A tick, which starts a pass when one is due, that pass let finish, and the tick after
    (at the same time) that reads what it did; any pass that one started is let finish too."""
    watcher.tick(now)
    _settle(watcher)
    read = watcher.tick(now)
    _settle(watcher)
    return read


# --- the setting ---------------------------------------------------------------------------


def test_every_minutes_is_60_by_default_never_negative_and_shown_by_config() -> None:
    assert TidySettings().every_minutes == 60
    with pytest.raises(ValidationError):
        TidySettings(every_minutes=-1)
    runner = CliRunner()
    assert runner.invoke(app, ["config", "set", "tidy.every_minutes", "15"]).exit_code == 0
    shown = runner.invoke(app, ["config", "get", "tidy.every_minutes"])
    assert shown.exit_code == 0 and "15" in shown.output
    assert load_config().tidy.every_minutes == 15
    refused = runner.invoke(app, ["config", "set", "tidy.every_minutes", "-5"])
    assert refused.exit_code != 0 and load_config().tidy.every_minutes == 15


@pytest.mark.parametrize("given", [float("inf"), float("nan"), 10081, 1e13])
def test_every_minutes_is_a_number_of_at_most_a_week(given: float) -> None:
    """Reviewer #3894's NIT: an infinite one overflowed the cadence's clock in ``is_due``, so
    every tick said the pass failed and none ever ran. Refused, and a week is the most."""
    with pytest.raises(ValidationError):
        TidySettings(every_minutes=given)
    assert TidySettings(every_minutes=10080).every_minutes == 10080
    refused = CliRunner().invoke(app, ["config", "set", "tidy.every_minutes", str(given)])
    assert refused.exit_code != 0 and load_config().tidy.every_minutes == 60


def test_a_config_that_cannot_be_read_turns_the_job_off(monkeypatch: pytest.MonkeyPatch) -> None:
    """A rule that deletes fails closed (review of #58): so does the job that runs them."""

    def unreadable() -> Any:
        raise ValueError("config.toml: not TOML")

    monkeypatch.setattr(tidy, "load_config", unreadable)

    assert tidy._settings().every_minutes == 0


# --- the cadence ---------------------------------------------------------------------------


@posix_only
def test_a_pass_runs_at_most_every_every_minutes(plans: Plans) -> None:
    """None yet: the first look runs one (an upgrade frees the backlog at once). Then none
    until the interval is up, measured from when the last one began."""
    assert tidy.periodic_pass(T0) is not None
    assert tidy.periodic_pass(T0 + timedelta(minutes=59)) is None
    assert tidy.periodic_pass(T0 + timedelta(minutes=60)) is not None
    assert plans.calls == [T0, T0 + timedelta(minutes=60)]
    assert tidy.read_stamp()["last_pass_at"] == (T0 + timedelta(minutes=60)).isoformat()
    assert plans.sized == [False, False], "no pass walks the repositories tidy keeps"


@posix_only
def test_the_cadence_is_the_homes_so_a_new_watcher_does_not_run_it_again(plans: Plans) -> None:
    """The stamp is on disk: a watcher started again (or ``--once`` beside the loop) reads it."""
    first = _pass_of(_watcher(), T0)
    again = _watcher()
    looked = again.tick(T0 + timedelta(minutes=30))
    later = _pass_of(_watcher(), T0 + timedelta(minutes=61))

    assert first.tidied is not None and later.tidied is not None
    assert looked.tidied is None and again.tidy_job is not None and again.tidy_job.thread is None
    assert len(plans.calls) == 2


@posix_only
def test_zero_turns_the_job_off(plans: Plans) -> None:
    config = load_config()
    config.tidy.every_minutes = 0
    save_config(config)

    watcher = _watcher()
    assert tidy.periodic_pass(T0) is None and watcher.tick(T0).tidied is None
    assert watcher.tidy_job is not None and watcher.tidy_job.thread is None, "no thread started"
    assert plans.calls == [] and not tidy.stamp_path().exists()


def test_on_native_windows_no_pass_runs(plans: Plans, monkeypatch: pytest.MonkeyPatch) -> None:
    """Tidy deletes nothing there in 0.10.2 (``DELETES``): an hourly walk that frees nothing
    would be all cost. No stamp either."""
    monkeypatch.setattr(tidy, "DELETES", False)

    assert tidy.periodic_pass(T0) is None and not tidy.pass_due(T0)
    assert plans.calls == [] and not tidy.stamp_path().exists()


# --- one at a time -------------------------------------------------------------------------


@posix_only
def test_a_pass_that_finds_the_lock_held_skips_and_stays_due(plans: Plans) -> None:
    lock = tidy.pass_lock()
    assert lock is not None and lock.parent.name == "system-temp"  # the sandbox's, never TMPDIR
    uid = os.getuid() if hasattr(os, "getuid") else 0
    assert lock.name == f"{tidy.PASS_LOCK_PREFIX}{uid}.lock"

    with locking.holding(lock, wait_s=0):
        assert tidy.periodic_pass(T0) is None
    assert plans.calls == [] and not tidy.stamp_path().exists()
    assert tidy.periodic_pass(T0) is not None


@posix_only
def test_two_watchers_at_once_run_one_pass(plans: Plans, monkeypatch: pytest.MonkeyPatch) -> None:
    """The first holds the lock through its pass; the second, meanwhile, waits for nothing and
    runs none. Afterwards the stamp says it ran: not due again."""
    faked = tidy.plan
    started, second_done = threading.Event(), threading.Event()

    def slow(*, now: datetime | None = None, size_kept: bool = True) -> tidy.Plan:
        started.set()
        assert second_done.wait(10)
        return faked(now=now, size_kept=size_kept)

    monkeypatch.setattr(tidy, "plan", slow)
    results: dict[str, tidy.PassReport | None] = {}
    first = threading.Thread(target=lambda: results.update(first=tidy.periodic_pass(T0)))
    first.start()
    assert started.wait(10)
    results["second"] = _pass_of(_watcher(), T0).tidied
    second_done.set()
    first.join(10)

    assert results["first"] is not None and results["second"] is None
    assert len(plans.calls) == 1
    assert tidy.periodic_pass(T0 + timedelta(minutes=1)) is None


@posix_only
def test_a_pass_that_looked_before_another_finished_asks_again_under_the_lock(
    plans: Plans, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Due when it looked, but another pass finished before it took the lock: it asks again
    there, and runs none."""
    assert tidy.periodic_pass(T0) is not None
    real = tidy.is_due
    looks: list[bool] = []

    def stale_first(
        at: datetime, every_minutes: float, *, last_attempt: datetime | None = None
    ) -> bool:
        looks.append(True)
        return True if len(looks) == 1 else real(at, every_minutes, last_attempt=last_attempt)

    monkeypatch.setattr(tidy, "is_due", stale_first)

    assert tidy.periodic_pass(T0 + timedelta(minutes=1)) is None
    assert len(looks) == 2 and len(plans.calls) == 1


# --- never during a move of the home (manager #3815) -----------------------------------------


@posix_only
def test_no_pass_runs_while_a_move_of_the_home_is_unfinished(plans: Plans) -> None:
    """The first tick after an upgrade cannot race the cutover: an unfinished journal (one
    whose steps do not say done, or that cannot be read) holds every pass, and it stays due."""
    journal = paths.aisquare_home().joinpath(*paths.MOVE_JOURNAL)
    journal.parent.mkdir(parents=True)
    journal.write_text(json.dumps({"steps": ["back up the store"]}), encoding="utf-8")
    assert paths.unfinished_home_move() == journal

    assert tidy.periodic_pass(T0) is None and _pass_of(_watcher(), T0).tidied is None
    assert plans.calls == [] and not tidy.stamp_path().exists()
    assert tidy.pass_due(T0), "it stays due"
    journal.write_text("{", encoding="utf-8")
    assert tidy.periodic_pass(T0) is None
    journal.write_text(json.dumps({"steps": ["back up the store", "done"]}), encoding="utf-8")
    assert tidy.periodic_pass(T0) is not None


@posix_only
def test_no_pass_runs_while_a_move_holds_its_lock_and_a_lock_a_crash_left_holds_none(
    tmp_path: Path, plans: Plans
) -> None:
    folder = tidy._move_lock_folder()
    assert folder.resolve().is_relative_to(tmp_path.resolve()), "the sandbox's, never ~"
    lock = folder / f"{home_migration.MOVE_LOCK_PREFIX}{'a' * 16}.lock"
    assert home_migration.move_lock_path(str(paths.aisquare_home()), named=True).parent == folder

    with locking.holding(lock, wait_s=0):
        assert tidy.a_home_move_runs()
        assert tidy.periodic_pass(T0) is None
    assert plans.calls == [] and lock.exists()  # left by a crash: held by nothing
    assert not tidy.a_home_move_runs()
    assert tidy.periodic_pass(T0) is not None


# --- its own lock, never tidied ------------------------------------------------------------


@posix_only
def test_tidys_lock_rule_never_takes_the_passs_own_lock(the_real_job: Path) -> None:
    """Rule 4 frees stale ``rlx-home-migration-<16 hex>.lock`` files alone: the pass's lock,
    however old, is never in a plan, and a pass leaves it where it is."""
    own = tidy.pass_lock()
    assert own is not None and own.parent == the_real_job
    own.write_bytes(b"")
    old = (datetime.now(UTC) - timedelta(days=30)).timestamp()
    os.utime(own, (old, old))

    assert tidy.LOCK_NAME.fullmatch(own.name) is None
    assert all(item.path != own for item in tidy.plan().items)
    assert tidy.periodic_pass() is not None and own.exists()


# --- the stamp (manager #3815) -------------------------------------------------------------


@posix_only
@pytest.mark.parametrize("torn", ["{", "", "[1, 2]", '{"last_pass_at": "yesterday"}'])
def test_a_torn_stamp_is_due_and_written_again(plans: Plans, torn: str) -> None:
    stamp = tidy.stamp_path()
    stamp.parent.mkdir(parents=True, exist_ok=True)
    stamp.write_text(torn, encoding="utf-8")

    assert tidy.periodic_pass(T0) is not None
    assert tidy.read_stamp()["last_pass_at"] == T0.isoformat()
    assert tidy.periodic_pass(T0 + timedelta(minutes=1)) is None


@posix_only
def test_a_stamp_that_is_no_file_is_set_aside_never_deleted_and_written_anew(
    plans: Plans,
) -> None:
    """A folder where the stamp goes: no replace could go over it, and every tick would run
    a pass. It is renamed aside, with what it holds, and the stamp written."""
    stamp = tidy.stamp_path()
    stamp.mkdir(parents=True)
    (stamp / "kept.txt").write_text("someone's\n", encoding="utf-8")

    assert tidy.periodic_pass(T0) is not None
    assert stamp.is_file() and tidy.read_stamp()["last_pass_at"] == T0.isoformat()
    [aside] = [p for p in stamp.parent.iterdir() if p.name.startswith("tidy.json.unreadable-")]
    assert (aside / "kept.txt").read_text(encoding="utf-8") == "someone's\n"
    assert tidy.periodic_pass(T0 + timedelta(minutes=1)) is None


@pytest.mark.skipif(
    sys.platform == "win32" or not hasattr(os, "getuid") or os.getuid() == 0,
    reason="needs a mode that denies reading: not as root, nor on Windows",
)
def test_a_stamp_that_cannot_be_read_is_due_and_written_again(plans: Plans) -> None:
    stamp = tidy.stamp_path()
    stamp.parent.mkdir(parents=True, exist_ok=True)
    stamp.write_text(json.dumps({"last_pass_at": T0.isoformat()}), encoding="utf-8")
    stamp.chmod(0)

    assert tidy.periodic_pass(T0 + timedelta(minutes=1)) is not None
    assert tidy.read_stamp()["last_pass_at"] == (T0 + timedelta(minutes=1)).isoformat()


def test_a_fifo_at_the_stamp_is_never_waited_on_and_is_due(plans: Plans) -> None:
    """Tester #3875 (a): opening a FIFO to read waits for a writer, so the tick hung. The stamp
    is opened without waiting: a FIFO is no file, so it is due, set aside and written anew."""
    if sys.platform == "win32":
        pytest.skip("a FIFO is POSIX's")
    stamp = tidy.stamp_path()
    stamp.parent.mkdir(parents=True, exist_ok=True)
    os.mkfifo(stamp)
    found: list[Any] = []
    reader = threading.Thread(target=lambda: found.append(tidy.periodic_pass(T0)), daemon=True)

    reader.start()
    reader.join(timeout=10)
    waited = reader.is_alive()
    for _ in range(20):  # a reader waits on it: a writer lets each wait go, leaving nothing
        if not reader.is_alive():
            break
        with contextlib.suppress(OSError):
            os.close(os.open(stamp, os.O_WRONLY | os.O_NONBLOCK))
        reader.join(timeout=1)
    assert not waited, "the pass waited on a FIFO at its stamp"
    assert found and found[0] is not None and plans.calls == [T0]
    assert stamp.is_file() and tidy.read_stamp()["last_pass_at"] == T0.isoformat()
    [aside] = [p for p in stamp.parent.iterdir() if p.name.startswith("tidy.json.unreadable-")]
    assert aside.is_fifo()


# --- quiet, and its one line ---------------------------------------------------------------


@posix_only
def test_a_pass_under_1_gb_says_nothing_on_any_board(
    plans: Plans, boards: list[str], nudges: list[str]
) -> None:
    plans.items = [(tidy.ENDED_WORKTREE, GB - 1, BOARD_A)]

    passed = tidy.periodic_pass(T0)

    assert passed is not None and passed.freed_bytes == GB - 1 and passed.receipt is None
    assert _notes(BOARD_A) == [] and _notes(BOARD_B) == [] and nudges == []


@posix_only
def test_a_pass_of_1_gb_or_more_says_so_once_on_the_board_most_of_it_counts_to(
    plans: Plans, boards: list[str], nudges: list[str]
) -> None:
    """One plain ``note``, the GB and the counts, on the board with the largest share; no
    nudge, and to a manager it is ``info``: no wake."""
    plans.items = [
        (tidy.ENDED_WORKTREE, GB, BOARD_A),
        (tidy.SESSION_CACHE, 300_000_000, BOARD_B),
        (tidy.SESSION_DIR, 200_000_000, BOARD_B),
        (tidy.IDLE_BUILD_CACHE, 100_000_000, BOARD_B),
        (tidy.MIGRATION_LOCK, 0, None),
    ]

    passed = tidy.periodic_pass(T0)

    said = "tidy freed 1.6 GB: 1 worktree, 2 session temp paths, 1 build cache, 1 lock"
    assert passed is not None and passed.receipt == said and passed.board == BOARD_A
    [note] = _notes(BOARD_A)
    assert note.text == said and note.to_role is None and note.session_id is None
    assert _notes(BOARD_B) == [] and nudges == []
    manager = TeamSession(
        id="aaaa1111-0000-0000-0000-000000000059",
        project_id=BOARD_A,
        role="manager",
        started_at=T0,
        last_seen_at=T0,
    )
    assert team_service.wake_class(note, manager) == "info"


@posix_only
def test_exactly_1_gb_is_said_and_a_pass_no_board_counts_to_says_it_nowhere(
    plans: Plans, boards: list[str]
) -> None:
    plans.items = [(tidy.ENDED_WORKTREE, GB, BOARD_B)]
    assert tidy.periodic_pass(T0) is not None
    assert [n.text for n in _notes(BOARD_B)] == ["tidy freed 1.0 GB: 1 worktree"]

    plans.items = [(tidy.MIGRATION_LOCK, 2 * GB, None)]
    passed = tidy.periodic_pass(T0 + timedelta(hours=1))
    assert passed is not None and passed.receipt is not None and passed.board is None
    assert len(_notes(BOARD_A)) == 0 and len(_notes(BOARD_B)) == 1


@posix_only
def test_a_board_that_will_not_take_the_line_costs_the_pass_nothing(
    plans: Plans, boards: list[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    plans.items = [(tidy.ENDED_WORKTREE, 3 * GB, BOARD_A)]

    def refuses(*args: Any, **kwargs: Any) -> Any:
        raise RuntimeError("database is locked")

    monkeypatch.setattr(team_service, "_emit", refuses)

    passed = tidy.periodic_pass(T0)

    assert passed is not None and passed.freed_bytes == 3 * GB and passed.board is None
    assert tidy.read_stamp()["freed_bytes"] == 3 * GB


# --- a pass never costs the tick ------------------------------------------------------------


@posix_only
def test_a_pass_that_raises_is_one_note_and_is_tried_again_an_interval_on(
    plans: Plans, monkeypatch: pytest.MonkeyPatch
) -> None:
    def broken(*, now: datetime | None = None, size_kept: bool = True) -> tidy.Plan:
        plans.calls.append(now)
        raise RuntimeError("the store is busy")

    monkeypatch.setattr(tidy, "plan", broken)

    looked = _pass_of(_watcher(), T0)
    again = _pass_of(_watcher(), T0 + timedelta(minutes=5))

    assert looked.tidied is None
    assert looked.notes == ["the tidy pass failed: RuntimeError: the store is busy"]
    assert again.notes == [] and plans.calls == [T0]
    assert tidy.read_stamp() == {"last_pass_at": T0.isoformat(), "failed": True}


@posix_only
@pytest.mark.parametrize("where", ["the move's probe", "the second look"])
def test_a_raise_before_the_plan_is_tried_again_an_interval_on(
    plans: Plans, monkeypatch: pytest.MonkeyPatch, where: str
) -> None:
    """Tester #3875 (c): once the lock is held, whatever raises writes the stamp, the looks
    before the plan too, so it is one note per interval, never one per tick."""
    asked: list[datetime] = []
    if where == "the move's probe":

        def probe() -> bool:
            asked.append(T0)
            raise PermissionError("the folder beside the home")

        monkeypatch.setattr(tidy, "a_home_move_runs", probe)
    else:
        real = tidy.is_due

        def is_due(
            at: datetime, every_minutes: float, *, last_attempt: datetime | None = None
        ) -> bool:
            asked.append(at)
            if len(asked) == 3:  # the tick's look and the pass's first pass; the third raises
                raise PermissionError("the watch folder")
            return real(at, every_minutes, last_attempt=last_attempt)

        monkeypatch.setattr(tidy, "is_due", is_due)

    looked = _pass_of(_watcher(), T0)
    again = _pass_of(_watcher(), T0 + timedelta(minutes=5))

    assert len(looked.notes) == 1 and looked.notes[0].startswith("the tidy pass failed")
    assert again.notes == [] and plans.calls == []
    assert tidy.read_stamp() == {"last_pass_at": T0.isoformat(), "failed": True}


@posix_only
def test_no_lock_to_be_had_is_said_once_per_interval(the_real_job: Path, plans: Plans) -> None:
    the_real_job.rmdir()  # no system temp dir: the lock cannot be made

    looked = _pass_of(_watcher(), T0)
    again = _pass_of(_watcher(), T0 + timedelta(minutes=5))

    assert len(looked.notes) == 1 and looked.notes[0].startswith("the tidy pass failed")
    assert again.notes == [] and plans.calls == []


# --- off the tick (reviewer #3894, manager #3897) ------------------------------------------


def _held_plan(monkeypatch: pytest.MonkeyPatch) -> tuple[list[datetime | None], threading.Event]:
    """``tidy.plan`` made to wait, on an event (no load), until the test lets it go: the
    times each pass entered it, and the event."""
    faked = tidy.plan
    entered: list[datetime | None] = []
    release = threading.Event()

    def held(*, now: datetime | None = None, size_kept: bool = True) -> tidy.Plan:
        entered.append(now)
        assert release.wait(30), "never let go"
        return faked(now=now, size_kept=size_kept)

    monkeypatch.setattr(tidy, "plan", held)
    return entered, release


def _entered(entered: list[datetime | None], count: int) -> None:
    deadline = time.monotonic() + 10
    while len(entered) < count and time.monotonic() < deadline:
        time.sleep(0.01)
    assert len(entered) == count


@posix_only
def test_a_slow_pass_holds_no_tick_and_the_news_flows_meanwhile(
    plans: Plans, boards: list[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    """The MAJOR: the pass ran inline, so an upgrade's backlog held the tick for minutes. Now
    it runs on the watcher's own thread: while it waits in its plan, every tick returns at
    once with its news delivered, starts no second pass, and a later tick reads what it did."""
    entered, release = _held_plan(monkeypatch)
    plans.items = [(tidy.ENDED_WORKTREE, 10, BOARD_A)]
    delivered: list[list[str]] = []
    started: list[datetime | None] = []

    def counted(
        now: datetime | None = None, *, last_attempt: datetime | None = None
    ) -> tidy.PassReport | None:
        started.append(now)
        return PERIODIC_PASS(now, last_attempt=last_attempt)

    monkeypatch.setattr(tidy, "periodic_pass", counted)

    def deliver(project_id: str, items: Any, path: Path) -> bool:
        delivered.append([item.id for item in items])
        return True

    def look_all(self: Any, now: datetime, notes: Any = None, failed: Any = None) -> Any:
        return [
            watch_service.WatchItem(
                id=f"capacity:{now:%H%M}",
                project_id=BOARD_A,
                project_name="a",
                kind="capacity_freed",
                summary="a slot came free",
            )
        ]

    monkeypatch.setattr(watch_service, "_deliver", deliver)
    monkeypatch.setattr(watch_service.Watcher, "look_all", look_all)
    watcher = _watcher()
    took: list[float] = []
    try:
        for minutes in range(3):
            began = time.monotonic()
            looked = watcher.tick(T0 + timedelta(minutes=minutes))
            took.append(time.monotonic() - began)
            _entered(entered, 1)
            assert looked.tidied is None and looked.nudged == [BOARD_A] and looked.notes == []
        assert watcher.tidy_job is not None and watcher.tidy_job.thread is not None
        assert watcher.tidy_job.thread.is_alive(), "the pass still waits in its plan"
        assert delivered == [["capacity:1200"], ["capacity:1201"], ["capacity:1202"]]
        assert started == [T0], "no tick starts a pass while one runs"
        assert entered == [T0] and max(took) < 2, took
    finally:
        release.set()
    _settle(watcher)
    read = watcher.tick(T0 + timedelta(minutes=3))
    assert read.tidied is not None and read.tidied.at == T0 and read.tidied.freed_bytes == 10
    assert plans.calls == [T0]


@posix_only
def test_a_stop_ends_the_watcher_at_once_while_a_pass_runs(
    plans: Plans, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``watch --stop`` is a SIGTERM, then the end of its tmux session: the loop ends at its
    next look at the flag, never waiting on a pass. The pass's thread is a daemon, so neither
    does the process's exit; one ended mid-delete wrote no stamp (the test below, in
    ``test_tidy.py``)."""
    entered, release = _held_plan(monkeypatch)
    handlers: list[tuple[int, Any]] = []
    monkeypatch.setattr(
        signal, "signal", lambda signum, handler: handlers.append((signum, handler))
    )
    monkeypatch.setattr(watch_service.Watcher, "look_all", lambda self, *args, **kwargs: [])
    config = load_config()
    config.watch.auto_stop = False
    save_config(config)

    def sleep(seconds: float) -> None:
        _entered(entered, 1)
        [(signum, stopping)] = handlers
        stopping(signum, None)  # what `watch --stop` sends

    monkeypatch.setattr(watch_service, "_sleep", sleep)
    try:
        began = time.monotonic()
        watch_service.run(interval=60, on_tick=lambda report: None)
        took = time.monotonic() - began
        [thread] = [t for t in threading.enumerate() if t.name == watch_service.TIDY_THREAD]
        assert thread.daemon and thread.is_alive(), "the pass was not waited for"
        assert took < 5, took
    finally:
        release.set()


@posix_only
def test_a_pass_thread_that_cannot_start_costs_no_tick_its_news(
    plans: Plans, boards: list[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    """Reviewer #3926's MINOR 1: the thread was kept before it started, so one that could not
    start ("can't start new thread") could never be joined, and every later tick raised there,
    its news with it. It is kept only once it runs: a start that fails is one note, the ticks
    after it deliver their news, and the pass is tried again once due."""
    plans.items = [(tidy.ENDED_WORKTREE, 10, BOARD_A)]
    delivered: list[list[str]] = []

    def deliver(project_id: str, items: Any, path: Path) -> bool:
        delivered.append([item.id for item in items])
        return True

    def look_all(self: Any, now: datetime, notes: Any = None, failed: Any = None) -> Any:
        return [
            watch_service.WatchItem(
                id=f"capacity:{now:%H%M}",
                project_id=BOARD_A,
                project_name="a",
                kind="capacity_freed",
                summary="a slot came free",
            )
        ]

    def cannot(self: threading.Thread) -> None:
        raise RuntimeError("can't start new thread")

    monkeypatch.setattr(watch_service, "_deliver", deliver)
    monkeypatch.setattr(watch_service.Watcher, "look_all", look_all)
    watcher = _watcher()
    with monkeypatch.context() as patched:
        patched.setattr(threading.Thread, "start", cannot)
        looked = [watcher.tick(T0 + timedelta(minutes=minutes)) for minutes in range(3)]

    assert [r.notes for r in looked] == [
        ["the tidy pass could not start: RuntimeError: can't start new thread"],
        [],
        [],
    ]
    assert delivered == [["capacity:1200"], ["capacity:1201"], ["capacity:1202"]]
    assert watcher.tidy_job is not None and watcher.tidy_job.thread is None
    assert plans.calls == []
    read = _pass_of(watcher, T0 + timedelta(minutes=60))
    assert read.tidied is not None and plans.calls == [T0 + timedelta(minutes=60)]


@posix_only
def test_a_stamp_that_cannot_be_written_is_said_and_the_watcher_waits_an_interval(
    plans: Plans, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Reviewer #3894's MINOR 1: on a full disk, every tick ran a full pass, each said to have
    failed though each freed what it planned. The pass ran: said so, and the watcher keeps its
    time in memory, so the next waits an interval."""

    def full(data: Any) -> None:
        raise OSError(errno.ENOSPC, "No space left on device")

    monkeypatch.setattr(tidy, "_write_stamp", full)
    plans.items = [(tidy.ENDED_WORKTREE, 10, BOARD_A)]
    watcher = _watcher()

    read = _pass_of(watcher, T0)

    assert read.tidied is not None and read.tidied.freed_bytes == 10
    assert read.tidied.stamp_error == "OSError: [Errno 28] No space left on device"
    assert read.notes == [
        "tidy freed what it planned, but its stamp could not be written (OSError: [Errno 28] "
        "No space left on device): this watcher waits an interval before the next pass"
    ]
    for minutes in (1, 30, 59):
        assert _pass_of(watcher, T0 + timedelta(minutes=minutes)).notes == []
    assert plans.calls == [T0]
    assert _pass_of(watcher, T0 + timedelta(minutes=60)).tidied is not None
    assert plans.calls == [T0, T0 + timedelta(minutes=60)]


@posix_only
def test_a_failed_pass_whose_stamp_cannot_be_written_says_what_failed_once(
    plans: Plans, monkeypatch: pytest.MonkeyPatch
) -> None:
    def broken(*, now: datetime | None = None, size_kept: bool = True) -> tidy.Plan:
        plans.calls.append(now)
        raise RuntimeError("the store is busy")

    def full(data: Any) -> None:
        raise OSError(errno.ENOSPC, "No space left on device")

    monkeypatch.setattr(tidy, "plan", broken)
    monkeypatch.setattr(tidy, "_write_stamp", full)
    watcher = _watcher()

    assert _pass_of(watcher, T0).notes == ["the tidy pass failed: RuntimeError: the store is busy"]
    assert _pass_of(watcher, T0 + timedelta(minutes=5)).notes == []
    assert plans.calls == [T0]


@posix_only
def test_what_cannot_tell_whether_a_move_runs_is_taken_for_one(
    tmp_path: Path, plans: Plans, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Reviewer #3894's MINOR 2: the probe failed open where the journal fails closed. A lock
    folder that cannot be listed, a lock that cannot be opened or asked: a move may run, so
    the pass waits, still due. A folder that is not there, or a lock gone before the open
    (let go), holds none."""
    folder = tidy._move_lock_folder()
    lock = folder / f"{home_migration.MOVE_LOCK_PREFIX}{'b' * 16}.lock"
    lock.write_bytes(b"")

    def no_locks(fd: int, *, wait_s: float) -> None:
        raise OSError(errno.ENOLCK, "No locks available")

    def shut(path: Path, *, mode: int = 0o644, create: bool = True) -> int:
        raise PermissionError(errno.EACCES, "Permission denied", str(path))

    def gone(path: Path, *, mode: int = 0o644, create: bool = True) -> int:
        raise FileNotFoundError(errno.ENOENT, "No such file or directory", str(path))

    a_file = tmp_path / "not-a-folder"
    a_file.write_bytes(b"")
    for name, value, moving in (
        ("acquire", no_locks, True),
        ("open_lock_file", shut, True),
        ("open_lock_file", gone, False),
    ):
        with monkeypatch.context() as patched:
            patched.setattr(locking, name, value)
            assert tidy.a_home_move_runs() is moving, (name, value)
    for where, moving in ((a_file, True), (tmp_path / "nowhere", False)):
        with monkeypatch.context() as patched:
            patched.setattr(tidy, "_move_lock_folder", lambda where=where: where)
            assert tidy.a_home_move_runs() is moving, where
    assert not tidy.a_home_move_runs(), "free, a crash left it: no move"

    monkeypatch.setattr(tidy, "_move_lock_folder", lambda: a_file)
    assert tidy.periodic_pass(T0) is None and plans.calls == []
    assert tidy.pass_due(T0), "it stays due"


@posix_only
def test_watch_once_looks_and_runs_no_pass(plans: Plans, monkeypatch: pytest.MonkeyPatch) -> None:
    """A look and its news alone: tidy's job is the loop's. One look never waits on a pass,
    nor leaves one cut short as it exits."""
    fleet = Fleet(agents=[])
    monkeypatch.setattr(
        remote_needs, "live_needs_sources", lambda: replace(_sources(fleet), list_projects=list)
    )
    config = load_config()
    config.watch.auto_stop = False
    save_config(config)

    shown = CliRunner().invoke(app, ["--json", "watch", "--once"])

    assert shown.exit_code == 0, shown.output
    assert json.loads(shown.stdout.strip().splitlines()[-1])["tidied"] is None
    assert plans.calls == [] and not tidy.stamp_path().exists()
    assert not [t for t in threading.enumerate() if t.name == watch_service.TIDY_THREAD]
    assert tidy.pass_due(datetime.now(UTC)), "still due, for the loop"


# --- the real rules, once ------------------------------------------------------------------


@posix_only
def test_a_real_pass_frees_what_rlx_tidy_would_and_watch_json_says_so(
    the_real_job: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No fake: a stale, free migration lock in the sandbox's system temp dir is what tidy's
    rules free, on the loop's own thread, and the look after it says so in ``watch --json``."""
    lock = the_real_job / f"rlx-home-migration-{'e' * 16}.lock"
    lock.write_bytes(b"")
    old = (datetime.now(UTC) - timedelta(days=2)).timestamp()
    os.utime(lock, (old, old))
    fleet = Fleet(agents=[])
    monkeypatch.setattr(
        remote_needs, "live_needs_sources", lambda: replace(_sources(fleet), list_projects=list)
    )
    config = load_config()
    config.watch.auto_stop = False
    save_config(config)
    handlers: list[tuple[int, Any]] = []
    monkeypatch.setattr(
        signal, "signal", lambda signum, handler: handlers.append((signum, handler))
    )
    slept: list[float] = []

    def sleep(seconds: float) -> None:
        """The time between two looks, in which the pass finishes; a stop after the second."""
        slept.append(seconds)
        for thread in threading.enumerate():
            if thread.name == watch_service.TIDY_THREAD:
                thread.join(10)
        if len(slept) == 2:
            [(signum, stopping)] = handlers
            stopping(signum, None)

    monkeypatch.setattr(watch_service, "_sleep", sleep)

    shown = CliRunner().invoke(app, ["--json", "watch", "--interval", "1"])

    assert shown.exit_code == 0, shown.output
    first, second = (json.loads(line)["tidied"] for line in shown.stdout.strip().splitlines())
    assert first is None, "the look that started it never waits for it"
    assert second["freed"] == {tidy.MIGRATION_LOCK: 1} and second["receipt"] is None
    assert second["stamp_error"] is None and not lock.exists()
