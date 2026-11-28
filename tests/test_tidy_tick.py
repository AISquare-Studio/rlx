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

import json
import os
import sys
import threading
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
def the_real_job(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """The job back, and the sandbox's system temp dir made, where its lock is."""
    monkeypatch.setattr(tidy, "periodic_pass", PERIODIC_PASS)
    system = tmp_path / "system-temp"
    system.mkdir(exist_ok=True)
    return system


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
    first = _watcher().tick(T0)
    again = _watcher().tick(T0 + timedelta(minutes=30))
    later = _watcher().tick(T0 + timedelta(minutes=61))

    assert first.tidied is not None and again.tidied is None and later.tidied is not None
    assert len(plans.calls) == 2


@posix_only
def test_zero_turns_the_job_off(plans: Plans) -> None:
    config = load_config()
    config.tidy.every_minutes = 0
    save_config(config)

    assert tidy.periodic_pass(T0) is None and _watcher().tick(T0).tidied is None
    assert plans.calls == [] and not tidy.stamp_path().exists()


def test_on_native_windows_no_pass_runs(plans: Plans, monkeypatch: pytest.MonkeyPatch) -> None:
    """Tidy deletes nothing there in 0.10.2 (``DELETES``): an hourly walk that frees nothing
    would be all cost. No stamp either."""
    monkeypatch.setattr(tidy, "DELETES", False)

    assert tidy.periodic_pass(T0) is None
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
    results["second"] = _watcher().tick(T0).tidied
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

    def stale_first(at: datetime, every_minutes: float) -> bool:
        looks.append(True)
        return True if len(looks) == 1 else real(at, every_minutes)

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

    assert tidy.periodic_pass(T0) is None and _watcher().tick(T0).tidied is None
    assert plans.calls == [] and not tidy.stamp_path().exists()
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

    looked = _watcher().tick(T0)
    again = _watcher().tick(T0 + timedelta(minutes=5))

    assert looked.tidied is None
    assert looked.notes == ["the tidy pass failed: RuntimeError: the store is busy"]
    assert again.notes == [] and plans.calls == [T0]
    assert tidy.read_stamp() == {"last_pass_at": T0.isoformat(), "failed": True}


@posix_only
def test_no_lock_to_be_had_is_said_once_per_interval(the_real_job: Path, plans: Plans) -> None:
    the_real_job.rmdir()  # no system temp dir: the lock cannot be made

    looked = _watcher().tick(T0)
    again = _watcher().tick(T0 + timedelta(minutes=5))

    assert len(looked.notes) == 1 and looked.notes[0].startswith("the tidy pass failed")
    assert again.notes == [] and plans.calls == []


# --- the real rules, once ------------------------------------------------------------------


@posix_only
def test_a_real_pass_frees_what_rlx_tidy_would_and_watch_once_json_says_so(
    the_real_job: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No fake: a stale, free migration lock in the sandbox's system temp dir is what tidy's
    rules free, and ``watch --once --json`` shows the pass."""
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

    shown = CliRunner().invoke(app, ["--json", "watch", "--once"])

    assert shown.exit_code == 0, shown.output
    tidied = json.loads(shown.stdout.strip().splitlines()[-1])["tidied"]
    assert tidied["freed"] == {tidy.MIGRATION_LOCK: 1} and tidied["receipt"] is None
    assert not lock.exists()
