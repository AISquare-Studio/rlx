"""Durable, bounded passes delivered to the reserved manager seat."""

from __future__ import annotations

from datetime import datetime
from pathlib import Path
from uuid import uuid4

from pydantic import BaseModel

from aisquare.core import harness, paths, program
from aisquare.core.agent_adapters import recorded_adapter, transcripts
from aisquare.core.store import ContextStore, store_session
from aisquare.models import TeamSession
from aisquare.services import manager_seat

_PREFIX = "Scheduled manager pass "
_DEFAULT = "Read the board range and act on what needs you. If nothing needs you, stop."


class Pass(BaseModel):
    """An immutable board range with its current delivery attempt."""

    id: str
    lower: int
    upper: int
    session: str
    attempted_at: datetime | None = None
    acknowledged_at: datetime | None = None


class Progress(BaseModel):
    """Completed read position, decision freshness and the unfinished range."""

    read: int = 0
    decision_at: datetime | None = None
    delivered_at: datetime | None = None
    completed: str | None = None
    pending: Pass | None = None


def read(store: ContextStore, project: str, board: str) -> tuple[str | None, Progress]:
    """Read the seat's pass state and its compare-and-set value."""
    raw = store.get_meta(manager_seat.progress_key(project, board, "pass"))
    return raw, Progress.model_validate_json(raw) if raw is not None else Progress()


def save(
    store: ContextStore, project: str, board: str, previous: str | None, progress: Progress
) -> bool:
    """Persist a transition only if no hook or reset changed the state."""
    key = manager_seat.progress_key(project, board, "pass")
    value = progress.model_dump_json()
    if previous is None:
        return store.set_meta_once(key, value)
    return store.compare_meta(key, previous, value)


def due(progress: Progress, minutes: int, now: datetime) -> bool:
    """Determine whether a new range can be opened."""
    if not minutes:
        return False
    last = progress.delivered_at
    return last is None or (now - last).total_seconds() >= minutes * 60


def new_range(store: ContextStore, session: TeamSession, progress: Progress) -> Pass:
    """Snapshot a range with an exclusive lower and inclusive upper bound."""
    return Pass(
        id=uuid4().hex,
        lower=progress.read,
        upper=store.latest_seq(session.project_id),
        session=session.id,
    )


def template(project: str) -> tuple[str, str | None]:
    """Read the project template safely, or return the built-in text and a reason."""
    try:
        found = (
            paths.read_below(
                paths.aisquare_home(),
                ("projects", project, "manager", "pass.md"),
                harness.STATE_FILE_MAX * 4,
            )
            .data.decode("utf-8")
            .strip()
        )
    except (OSError, UnicodeError) as exc:
        return _DEFAULT, f"pass.md unavailable ({type(exc).__name__})"
    if not found or len(found) > harness.STATE_FILE_MAX:
        return _DEFAULT, "pass.md empty or too long"
    return found, None


def prompt(project: str, board: str, pending: Pass) -> tuple[str, str | None]:
    """Attach commands for the exact range to the project template."""
    body, why = template(project)
    cli = program.word()
    return (
        f"{_PREFIX}{pending.id}\n{body}\n\n"
        f"Board {board}. Read after #{pending.lower} through #{pending.upper}:\n"
        f"{cli} --json team log --pass-id {pending.id} --since-seq {pending.lower} "
        f"--through-seq {pending.upper} --as {pending.session}\n"
        "Continue paging from the last seq until the range is exhausted. "
        "After reading and acting, run:\n"
        f"{cli} team complete-pass {pending.id} --as {pending.session}",
        why,
    )


def _binding(store: ContextStore, session: TeamSession) -> str:
    """Resolve the session from the current reserved row, including on a hub."""
    row = store.fleet_agent_for_session(session.id)
    if row is None or manager_seat.bound(store, row) is None:
        raise ValueError("the session does not hold a live manager seat")
    return row.project_id


def bound_range(store: ContextStore, session: TeamSession, pass_id: str) -> Pass:
    """Verify that this session owns the pending pass and its bound board."""
    project = _binding(store, session)
    pending = read(store, project, session.project_id)[1].pending
    if pending is None or pending.id != pass_id or pending.session != session.id:
        raise ValueError("no matching pending pass for this manager session")
    return pending


def acknowledge(session_id: str, text: str | None, now: datetime) -> bool:
    """Record the exact submitted pass, independently of the pane reservation."""
    first = (text or "").partition("\n")[0]
    if not first.startswith(_PREFIX):
        return False
    with store_session() as store:
        session = store.get_session(session_id)
        if session is None:
            return False
        try:
            project = _binding(store, session)
        except ValueError:
            return False
        raw, progress = read(store, project, session.project_id)
        pending = progress.pending
        if (
            pending is None
            or pending.session != session.id
            or pending.attempted_at is None
            or first != _PREFIX + pending.id
        ):
            return False
        if pending.acknowledged_at is not None:
            return True
        pending = pending.model_copy(update={"acknowledged_at": now})
        return save(
            store,
            project,
            session.project_id,
            raw,
            progress.model_copy(update={"pending": pending, "delivered_at": now}),
        )


def observe_transcript(session: TeamSession, pending: Pass) -> bool:
    """Recover an acknowledgement from the bound session's submitted transcript turn."""
    if pending.attempted_at is None or pending.acknowledged_at is not None:
        return False
    if pending.session != session.id or not session.transcript_path:
        return False
    adapter = recorded_adapter(session.agent)
    reader = adapter.transcript_reader() if adapter is not None else None
    if reader is None:
        return False
    path = Path(session.transcript_path)
    try:
        lines = transcripts.lines_backwards(
            path, path.stat().st_size, budget=transcripts.TAIL_BUDGET
        )
        for _, record in reader.records(lines):
            if record is None:
                continue
            submitted = transcripts.record_time(record)
            if submitted is None or submitted < pending.attempted_at:
                continue
            rendered = reader.page_lines(record, width=harness.STATE_FILE_MAX * 2)
            if (
                len(rendered) > 1 and rendered[0] == transcripts.YOU
                and rendered[1].strip() == _PREFIX + pending.id
            ):
                return acknowledge(session.id, rendered[1].strip(), submitted)
    except OSError:
        return False
    return False


def baseline(store: ContextStore, project: str, board: str, seq: int) -> None:
    """Set an operator-verified completed boundary before any pass starts."""
    event = store.get_event_by_seq(seq) if seq > 0 else None
    if seq < 0 or (seq > 0 and (event is None or event.project_id != board)):
        raise ValueError("the verified boundary must be on this board")
    raw, _ = read(store, project, board)
    if raw is not None or not save(store, project, board, None, Progress(read=seq)):
        raise ValueError("this seat already has a pass read position")


def complete(store: ContextStore, session: TeamSession, pass_id: str) -> None:
    """Atomically advance the read and decision positions for an acknowledged range."""
    project = _binding(store, session)
    board = session.project_id
    raw, progress = read(store, project, board)
    if progress.completed == pass_id:
        return
    pending = progress.pending
    if (
        pending is None
        or pending.id != pass_id
        or pending.session != session.id
        or pending.acknowledged_at is None
    ):
        raise ValueError("no acknowledged pass with that ID for this manager session")
    decision = store.filtered_events(board, through_seq=pending.upper, kind="decision", limit=1)
    decision_at = (
        decision[0].created_at
        if decision and decision[0].seq > pending.lower
        else progress.decision_at
    )
    if not save(
        store,
        project,
        board,
        raw,
        progress.model_copy(
            update={
                "pending": None,
                "read": pending.upper,
                "completed": pass_id,
                "decision_at": decision_at,
            }
        ),
    ):
        raise ValueError("pass changed during completion; check its current state")


def retryable(pending: Pass, session: TeamSession) -> bool:
    """Retry only after replacement or a later, finished turn."""
    if pending.session != session.id or pending.attempted_at is None:
        return True
    return (
        session.state == "waiting"
        and session.prompted_at is not None
        and session.prompted_at >= pending.attempted_at
    )
