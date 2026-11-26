"""#35 — the smart account picker: capacity, a fair share and tiers, in place of fill-first.

Fill-first took the FIRST account in priority order under ``switch_at``; on the owner's Mac
(2026-11-25, 22:30Z) 25 of 26 live agents ran on the personal Max 20x account at 45% of its
week while three Max 5x accounts sat at 0-4%. The pick now weighs each account's capacity
left (plan units times its week left), its fair share of the live fleet, and the tier of the
agent being placed, with no setting. ``balanced_pick`` is pure, so most of this file feeds it
figures; the rest runs the resolver (``choose``), the spawn and ``accounts usage`` against
fixtures: scripted usage, files for credentials, never a Keychain or a real account.
"""

from __future__ import annotations

import json
from collections.abc import Sequence
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import claude_accounts as core
from aisquare.core import codex_accounts
from aisquare.core.orchestrator import team_project
from aisquare.core.store import store_session
from aisquare.models import (
    ClaudeAccount,
    FleetAgent,
    ProjectInfo,
    StatuslineSession,
    TaskStatus,
    TeamTask,
)
from aisquare.services import claude_accounts as service
from aisquare.services import fleet as fleet_service
from aisquare.services import settings as settings_service
from aisquare.services.claude_accounts import Seat, Tier
from tests.test_claude_accounts import NOW, _sign_in
from tests.test_fleet_service import (  # noqa: F401 - fixtures, by name
    FakeTmux,
    _command,
    _flag,
    claude_on_path,
    project,
    repo,
    tmux,
)
from tests.test_usage_aware_accounts import _Usage

#: The owner's measurement: 22:30Z on Wednesday 2026-11-25.
MEASURED_AT = datetime(2026, 11, 25, 22, 30, tzinfo=UTC)

TWENTY_X = service.plan_units("max 20x")
FIVE_X = service.plan_units("max 5x")


def _measured() -> list[Seat]:
    """The owner's four accounts, in the machine order they had then (1, 2, 3, 4): three Max
    5x at 0-4% of the week, resetting in 4 days, 2 days and 10 hours, and the personal Max 20x
    at 45%, resetting in 6 days, the account the owner uses themselves."""
    return [
        Seat(1, FIVE_X, week_used=0.0, week_resets_at=MEASURED_AT + timedelta(days=4)),
        Seat(2, FIVE_X, week_used=2.0, week_resets_at=MEASURED_AT + timedelta(days=2)),
        Seat(3, FIVE_X, week_used=4.0, week_resets_at=MEASURED_AT + timedelta(hours=10)),
        Seat(
            4,
            TWENTY_X,
            week_used=45.0,
            week_resets_at=MEASURED_AT + timedelta(days=6),
            human=True,
        ),
    ]


#: 26 spawns as a fleet makes them: managers and coders on the critical path (a task another
#: one ``--needs``), ordinary coders, and testers, reviewers and validators between them.
SPAWNS: tuple[tuple[str, bool], ...] = (
    ("manager", False),
    ("coder", True),
    ("coder", False),
    ("tester", False),
    ("coder", True),
    ("coder", False),
    ("reviewer", False),
    ("tester", False),
    ("coder", True),
    ("coder", False),
    ("validator", False),
    ("coder", False),
    ("reviewer", False),
    ("coder", True),
    ("coder", False),
    ("tester", False),
    ("reviewer", False),
    ("coder", False),
    ("manager", False),
    ("tester", False),
    ("coder", False),
    ("reviewer", False),
    ("coder", False),
    ("validator", False),
    ("tester", False),
    ("coder", False),
)


def _land(seats: Sequence[Seat], slot: int, tier: Tier) -> list[Seat]:
    """``seats`` with one more live agent of ``tier`` on ``slot``."""
    return [
        replace(seat, live={**seat.live, tier: seat.live.get(tier, 0) + 1})
        if seat.slot == slot
        else seat
        for seat in seats
    ]


def _within(seats: Sequence[Seat]) -> list[Seat]:
    """The seats with room for one more agent: one more keeps each within its share + 1."""
    units = service.seats_units(seats)
    shares = service.fair_shares(units, sum(seat.agents for seat in seats) + 1)
    return [s for s in seats if units[s.slot] > 0 and s.agents <= shares[s.slot] + 1e-9]


# --------------------------------------------------------------------------- the measured case


def test_the_measured_case_spreads_26_spawns_by_capacity_tier_and_reset() -> None:
    """Acceptance 1: 26 spawns across a 20x at 45% and three 5x at 0-4%."""
    seats = _measured()
    landed: list[tuple[Tier, int]] = []
    for role, critical in SPAWNS:
        tier = service.agent_tier(role, critical=critical)
        room = _within(seats)
        placed = service.balanced_pick(seats, tier, now=MEASURED_AT)
        assert placed.slot is not None
        if tier == "low":
            # Testers take the soonest-reset account that has room for one more.
            soonest = min(room, key=lambda s: s.week_resets_at or datetime.max.replace(tzinfo=UTC))
            assert placed.slot == soonest.slot, (role, placed.reason)
        seats = _land(seats, placed.slot, tier)
        landed.append((tier, placed.slot))

    units = service.seats_units(seats)
    shares = service.fair_shares(units, len(SPAWNS))
    by_slot = {seat.slot: seat.agents for seat in seats}
    # No account ends more than one over its fair share, and every account took some work:
    # fill-first put all 26 on slot 1, the first under the line.
    assert all(by_slot[slot] - shares[slot] <= 1 for slot in by_slot), (by_slot, shares)
    assert all(count > 0 for count in by_slot.values()) and sum(by_slot.values()) == 26
    # The personal account keeps 20% of its week: it is weighed at 35% left, not 55%.
    assert units[4] == pytest.approx(TWENTY_X * 0.35)
    assert service.seat_units(seats[3]) == pytest.approx(TWENTY_X * 0.55)
    # Managers and critical-path coders go to the longest runway: the first to the account
    # with the most units and the latest reset, every one to the two latest resets, most of
    # them to the first; none to the account that resets tonight.
    high = [slot for tier, slot in landed if tier == "high"]
    assert high[0] == 4
    assert set(high) <= {4, 1} and high.count(4) > len(high) / 2
    # Testers start on the account whose week resets soonest (10 hours away).
    low = [slot for tier, slot in landed if tier == "low"]
    assert low[0] == 3


def test_with_25_agents_on_the_personal_account_the_next_spawns_go_to_the_others() -> None:
    """The state the owner measured: slot 4 is far over its share, so no tier lands there."""
    seats = [replace(seat, live={"normal": 25}) if seat.slot == 4 else seat for seat in _measured()]
    for tier in ("high", "normal", "low", "high", "normal", "low", "normal", "normal"):
        placed = service.balanced_pick(seats, tier, now=MEASURED_AT)
        assert placed.slot != 4, placed.reason
        assert placed.slot is not None
        seats = _land(seats, placed.slot, tier)


def test_the_personal_account_keeps_its_reserve_while_another_has_room() -> None:
    """Its last 20% of the week is the person's: an agent takes it only when no other account
    can be had."""
    personal = Seat(4, TWENTY_X, week_used=82.0, human=True)  # 18% left: inside the reserve
    other = Seat(2, FIVE_X, week_used=70.0)
    for tier in ("high", "normal", "low"):
        assert service.balanced_pick([personal, other], tier, now=MEASURED_AT).slot == 2
    assert service.seats_units([personal, other])[4] == 0
    alone = service.balanced_pick([personal], "normal", now=MEASURED_AT)
    assert alone.slot == 4 and "keeps" not in alone.reason  # no other: the reserve is spent


def test_plan_units_are_each_agents_own() -> None:
    """Units by the plan ``accounts list`` names; Codex's plans are not weighed, so its ``pro``
    (ChatGPT Pro) is one unit, not Claude's Pro, and its accounts keep no reserve."""
    assert (service.plan_units("max 20x"), service.plan_units("Max 5x")) == (4.0, 1.0)
    assert service.plan_units("pro") == pytest.approx(0.2)
    assert service.plan_units("team") == service.plan_units(None) == 1.0
    assert service.plan_units("pro", codex_accounts.AGENT) == 1.0
    assert core.kind(service.AGENT).reserves_for_person
    assert not core.kind(codex_accounts.AGENT).reserves_for_person


def test_a_five_hour_window_near_its_cap_shrinks_an_accounts_units() -> None:
    fresh = Seat(2, FIVE_X, session_used=None)
    busy = Seat(3, FIVE_X, session_used=80.0)
    assert service.seat_units(fresh) == pytest.approx(1.0)
    assert service.seat_units(busy) == pytest.approx(0.5)  # 20 of the last 40 points left
    assert service.balanced_pick([busy, fresh], "normal", now=MEASURED_AT).slot == 2


# --------------------------------------------------------------------------- no fill-first


@pytest.mark.parametrize("tier", ["high", "normal", "low"])
def test_two_equal_accounts_take_turns(tier: Tier) -> None:
    """Acceptance 2: fill-first gave the first account every spawn until it crossed the line."""
    resets = MEASURED_AT + timedelta(days=3)
    seats = [Seat(2, FIVE_X, 10.0, resets), Seat(3, FIVE_X, 10.0, resets)]
    picks = []
    for _ in range(6):
        placed = service.balanced_pick(seats, tier, now=MEASURED_AT)
        assert placed.slot is not None
        picks.append(placed.slot)
        seats = _land(seats, placed.slot, tier)
    assert picks == [2, 3, 2, 3, 2, 3]


def test_consecutive_launches_through_the_resolver_alternate(
    fake_home: Path, work: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The same, end to end: each launch's pick counts the live rows the earlier ones left."""
    _accounts(monkeypatch, {2: ("max 5x", 10.0), 3: ("max 5x", 10.0)})
    _person_on_the_plain_claude()
    picks = []
    for number in range(4):
        choice = service.choose(None, role="coder", project=work)
        assert choice.source == "headroom" and choice.account is not None
        picks.append(choice.account.slot)
        _live(work, f"coder-{number}", "coder", choice.account.slot)
    assert picks == [2, 3, 2, 3]


# --------------------------------------------------------------------------- the hard edges


def test_the_flag_a_binding_and_a_project_default_still_win(
    fake_home: Path, work: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Acceptance 3: the balanced pick is the headroom rung's, below every explicit one."""
    _accounts(monkeypatch, {2: ("max 5x", 10.0), 3: ("max 20x", 10.0)})
    _person_on_the_plain_claude()
    balanced = service.choose(None, role="coder", project=work)
    assert (balanced.source, _slot(balanced)) == ("headroom", 3)  # four times the units

    flag = service.choose("2", role="coder", project=work)
    assert (flag.source, _slot(flag)) == ("flag", 2)

    settings_service.bind_role("coder", account="2")
    bound = service.choose(None, role="coder", project=work)
    assert (bound.source, _slot(bound)) == ("role binding", 2)
    settings_service.clear_role_binding("coder")

    service.set_default("2", project=work)
    default = service.choose(None, role="coder", project=work)
    assert (default.source, _slot(default)) == ("project default", 2)


@pytest.mark.parametrize("over", ["session", "week"])
def test_the_line_still_blocks_however_big_the_account(
    fake_home: Path, work: ProjectInfo, monkeypatch: pytest.MonkeyPatch, over: str
) -> None:
    """An account over ``switch_at`` in either window is no candidate, 20x or not."""
    big = (90.0, 10.0) if over == "session" else (10.0, 86.0)
    _accounts(monkeypatch, {2: ("max 5x", 50.0), 3: ("max 20x", *big)})
    _person_on_the_plain_claude()
    for number in range(3):
        choice = service.choose(None, role="manager", project=work)
        assert choice.source == "headroom" and _slot(choice) == 2, choice.notes
        _live(work, f"manager-{number}", "manager", 2)


def test_a_disabled_account_is_never_picked(
    fake_home: Path, work: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    _accounts(monkeypatch, {2: ("max 5x", 60.0), 3: ("max 20x", 0.0)})
    _person_on_the_plain_claude()
    service.set_disabled("3", True)
    for number in range(4):
        choice = service.choose(None, role="coder", project=work)
        assert _slot(choice) == 2, choice.notes
        _live(work, f"coder-{number}", "coder", 2)


# --------------------------------------------------------------------------- the tiers


@pytest.mark.parametrize(
    ("role", "critical", "tier"),
    [
        ("manager", False, "high"),
        ("captain", False, "high"),
        ("coder", True, "high"),
        ("coder", False, "normal"),
        ("planner", False, "normal"),
        (None, False, "normal"),
        ("tester", False, "low"),
        ("reviewer", False, "low"),
        ("validator", False, "low"),
        ("ui-tester", False, "low"),
        ("tester", True, "low"),  # checking work stays low on the critical path
    ],
)
def test_the_tier_of_a_role(role: str | None, critical: bool, tier: Tier) -> None:
    assert service.agent_tier(role, critical=critical) == tier


def test_a_coder_whose_task_another_open_task_needs_is_high(
    fake_home: Path, work: ProjectInfo
) -> None:
    """Acceptance 4, from the board: the tiers of the agent being placed and of the live rows."""
    needed = _task(work, "needed")
    _task(work, "waits", needs=[needed.id])
    lone = _task(work, "lone")
    done_need = _task(work, "done-need")
    _task(work, "closed", needs=[done_need.id], status="done")  # a done task waits on nothing
    _live(work, "manager", "manager", 2)
    _live(work, "coder-1", "coder", 2, task_id=needed.id)
    _live(work, "coder-2", "coder", 3, task_id=lone.id)
    _live(work, "tester-1", "tester", 3)
    _live(work, "reviewer-1", "reviewer", 3)

    def tier(role: str, task_id: str | None = None) -> Tier:
        return service.balance_for(role, task_id).tier

    assert tier("coder", needed.id) == "high"
    assert tier("coder", lone.id) == "normal"
    assert tier("coder", done_need.id) == "normal"
    assert tier("manager") == "high"
    assert tier("tester") == tier("reviewer") == tier("validator") == "low"
    assert service.balance_for("coder").live == {
        2: {"high": 2},
        3: {"normal": 1, "low": 2},
    }


# --------------------------------------------------------------------------- the person's account


def test_the_persons_account_is_their_newest_session_no_fleet_row_owns() -> None:
    """Acceptance 5, pure: the status line's sessions, newest first."""
    accounts = [_account(2), _account(3), _account(4)]

    def said(slot: int | None, fleet: str | None, minutes: int) -> StatuslineSession:
        return StatuslineSession(
            session_id=f"s{slot}{fleet}",
            updated_at=MEASURED_AT - timedelta(minutes=minutes),
            slot=slot,
            fleet_agent=fleet,
        )

    sessions = [said(3, "fa_coder", 1), said(9, None, 2), said(4, None, 5), said(2, None, 30)]
    assert service.human_slot(sessions, accounts, accounts[0]) == 4  # past a fleet one, not ours
    assert service.human_slot([said(3, "fa_coder", 1)], accounts, accounts[0]) == 2  # default
    assert service.human_slot([], accounts, accounts[1]) == 3
    assert service.human_slot([], accounts, None) is None


def test_the_persons_account_is_read_from_the_status_line_else_the_machine_default(
    fake_home: Path, work: ProjectInfo, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Acceptance 5, from the records ``asq-statusline`` writes."""
    made = _accounts(monkeypatch, {2: ("max 5x", 10.0), 3: ("max 5x", 10.0), 4: ("max 20x", 10.0)})
    service.reorder(["3", "2", "4"])
    listed = [a for a in service.list_accounts() if a.agent == service.AGENT]
    assert service.balance_for("coder", accounts=listed).human == 3  # no session: rank 1

    _record("fleet", made[2].config_dir, fleet_agent="fa_someone", minutes=1)
    assert service.balance_for("coder", accounts=listed).human == 3  # a fleet row's is no one's

    _record("mine", made[4].config_dir, minutes=20)
    assert service.balance_for("coder", accounts=listed).human == 4


# --------------------------------------------------------------------------- saying why


def test_the_reason_reads_as_the_spawn_line_says_it() -> None:
    """Acceptance 6: ``slot 3 · the man (fair share 3/5 · resets Sat · low tier)``."""
    saturday = datetime(2026, 11, 28, 12, 0, tzinfo=UTC)  # a Saturday, three days out
    seats = [
        Seat(3, FIVE_X, week_resets_at=saturday, live={"low": 2}),
        Seat(4, FIVE_X, week_resets_at=saturday + timedelta(days=2), live={"normal": 6}),
    ]
    placed = service.balanced_pick(seats, "low", now=MEASURED_AT)
    assert placed.slot == 3
    assert placed.reason == "fair share 3/5 · resets Sat · low tier"
    choice = service.AccountChoice(_account(3, alias="the man"), "headroom", [], why=placed.reason)
    assert choice.describe() == "slot 3 · the man (fair share 3/5 · resets Sat · low tier)"
    unnamed = service.AccountChoice(_account(3), "headroom", [], why=placed.reason)
    assert unnamed.describe() == "account 3 (fair share 3/5 · resets Sat · low tier)"

    soon = service.balanced_pick(
        [Seat(2, FIVE_X, week_resets_at=MEASURED_AT + timedelta(hours=5))],
        "high",
        now=MEASURED_AT,
    )
    assert soon.reason == "fair share 1/1 · resets in 5h · high tier"
    kept = service.balanced_pick(
        [Seat(4, TWENTY_X, week_used=10.0, human=True), Seat(2, FIVE_X, week_used=99.0)],
        "normal",
        now=MEASURED_AT,
    )
    assert kept.slot == 4 and kept.reason == "fair share 1/1 · normal tier · keeps 20% for you"


def test_the_spawn_names_the_account_it_picked_and_why(
    tmux: FakeTmux,  # noqa: F811
    claude_on_path: Path,  # noqa: F811
    project: ProjectInfo,  # noqa: F811
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The spawn's ``accounts:`` line names the pick and its reason, and the window runs on
    it; the task it is spawned for decides its tier (``fleet spawn coder --task``)."""
    _accounts(monkeypatch, {2: ("max 5x", 50.0), 3: ("max 5x", 10.0)}, now=_wall_clock())
    service.set_alias("3", "the man")
    _person_on_the_plain_claude()
    needed = _task(project, "needed")
    _task(project, "waits", needs=[needed.id])

    receipt = fleet_service.spawn(project, "coder", worktree=False, task_id=needed.id)

    assert _flag(_command(tmux), "--account") == "3"
    said = [note for note in receipt.notes if note.startswith("accounts: headroom:")]
    assert len(said) == 1, receipt.notes
    # The week resets on the coming Saturday: ``resets Sat``, or ``resets in …`` within a day.
    assert " — slot 3 · the man (fair share 1/1 · resets " in said[0], said[0]
    assert said[0].endswith(" · high tier)"), said[0]


def test_accounts_usage_shows_each_accounts_agents_and_fair_share(
    fake_home: Path, work: ProjectInfo, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Acceptance 6: display only; the columns are there while fleet agents run."""
    _accounts(monkeypatch, {2: ("max 5x", 0.0), 3: ("max 20x", 0.0)})
    _person_on_the_plain_claude()
    monkeypatch.setenv("COLUMNS", "200")

    quiet = runner.invoke(app, ["accounts", "usage"])
    assert quiet.exit_code == 0, quiet.output
    assert "share" not in quiet.stdout.splitlines()[0]  # no fleet, no shares to show

    _live(work, "coder-1", "coder", 2)
    for number in range(3):
        _live(work, f"coder-{number + 2}", "coder", 3)
    shown = runner.invoke(app, ["accounts", "usage"])
    assert shown.exit_code == 0, shown.output
    lines = shown.stdout.splitlines()
    assert lines[0].split()[-2:] == ["agents", "share"]
    rows = {line.split()[0]: line.split() for line in lines[1:] if line.split()}
    # Four agents over 1 + 4 units: 0.8 for the 5x, 3.2 for the 20x.
    assert rows["2"][-2:] == ["1", "0.8"]
    assert rows["3"][-2:] == ["3", "3.2"]
    assert rows["all"][-1] == "4"


# --------------------------------------------------------------------------- fixtures


@pytest.fixture
def work(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> ProjectInfo:
    root = tmp_path / "repo"
    root.mkdir()
    monkeypatch.chdir(root)
    info = team_project(root)
    with store_session() as store:
        store.onboard_project(info)
    return info


def _wall_clock() -> datetime:
    """The clock a fleet test runs on: the real one (no redirected-home clock there)."""
    return datetime.now(tz=UTC)


def _accounts(
    monkeypatch: pytest.MonkeyPatch,
    slots: dict[int, tuple[Any, ...]],
    *,
    now: datetime = NOW,
) -> dict[int, ClaudeAccount]:
    """Managed slots 2, 3, … signed in on the plans named, with a scripted usage endpoint:
    ``slot: (plan, week %)`` or ``(plan, five-hour %, week %)``. Each week resets on the
    Saturday after ``now``."""
    tiers = {"max 5x": "default_claude_max_5x", "max 20x": "default_claude_max_20x"}
    answers: dict[str, dict[str, Any] | int] = {}
    made: dict[int, ClaudeAccount] = {}
    saturday = (now + timedelta(days=(5 - now.weekday()) % 7 or 7)).replace(
        hour=12, minute=0, second=0, microsecond=0
    )
    for slot in sorted(slots):
        plan, *figures = slots[slot]
        session, week = (0.0, figures[0]) if len(figures) == 1 else figures
        account = core.create_account()
        assert account.slot == slot
        _sign_in(account, f"slot{slot}@example.com", tier=tiers[plan])
        creds = core.credentials_path(account)
        data = json.loads(creds.read_text())
        data["claudeAiOauth"]["accessToken"] = f"tok-{slot}"
        data["claudeAiOauth"]["expiresAt"] = int((now + timedelta(days=3650)).timestamp() * 1000)
        creds.write_text(json.dumps(data))
        answers[f"tok-{slot}"] = {
            "five_hour": {
                "utilization": session,
                "resets_at": (now + timedelta(hours=3)).isoformat(),
            },
            "seven_day": {"utilization": week, "resets_at": saturday.isoformat()},
            "limits": [{"kind": "session", "percent": session, "severity": "normal"}],
        }
        made[slot] = account
    monkeypatch.setattr(service, "_http_get", _Usage(answers))
    return made


def _account(slot: int, *, alias: str | None = None) -> ClaudeAccount:
    return ClaudeAccount(
        slot=slot, config_dir=Path(f"accounts/{slot}"), managed=True, alias=alias, position=slot
    )


def _slot(choice: service.AccountChoice) -> int | None:
    return choice.account.slot if choice.account is not None else None


def _record(
    session_id: str,
    config_dir: Path | None,
    *,
    fleet_agent: str | None = None,
    minutes: int = 1,
) -> None:
    """A record ``asq-statusline`` writes for a session on ``config_dir``."""
    folder = service.statusline_dir()
    folder.mkdir(parents=True, exist_ok=True)
    record: dict[str, Any] = {
        "session_id": session_id,
        "updated_at": (service._now() - timedelta(minutes=minutes)).timestamp(),
        "config_dir": str(config_dir) if config_dir is not None else None,
    }
    if fleet_agent is not None:
        record["fleet_agent"] = fleet_agent
    (folder / f"{session_id}.json").write_text(json.dumps(record), encoding="utf-8")


def _person_on_the_plain_claude() -> None:
    """The person's own session runs on the plain claude (slot 1), which no test here signs
    in: the accounts a test picks among keep no reserve."""
    _record("person", None)


def _live(
    where: ProjectInfo, label: str, role: str, slot: int, *, task_id: str | None = None
) -> FleetAgent:
    """A live fleet row on ``slot``, as a spawn leaves one."""
    with store_session() as store:
        return store.upsert_fleet_agent(
            FleetAgent(
                id=f"fa_{label}",
                project_id=where.id,
                label=label,
                role=role,
                pane_id=f"%{abs(hash(label)) % 1000}",
                cwd=where.root,
                task_id=task_id,
                account_slot=slot,
                created_at=datetime.now(tz=UTC),
            )
        )


def _task(
    where: ProjectInfo,
    key: str,
    *,
    needs: list[str] | None = None,
    status: TaskStatus = "doing",
) -> TeamTask:
    now = datetime.now(tz=UTC)
    with store_session() as store:
        task, _ = store.upsert_task(
            TeamTask(
                id=f"tsk_{key}",
                project_id=where.id,
                key=key,
                title=key,
                status=status,
                needs=needs or [],
                created_at=now,
                updated_at=now,
            )
        )
    return task
