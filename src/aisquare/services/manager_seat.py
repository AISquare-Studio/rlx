"""Project policy and durable read positions for the reserved manager seat."""

from __future__ import annotations

from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Literal

from aisquare.core import locking, paths
from aisquare.core.store import ContextStore
from aisquare.models import FleetAgent, ManagerWake, TeamSession

LABEL = "manager"
WAKE = "manager_wake"
PASS_MINUTES = "manager_pass_minutes"
EVENT_PROJECT = "manager_project"
WakeMode = ManagerWake | Literal["blocked"]


@dataclass(frozen=True)
class Policy:
    """The project's runtime policy and any invalid stored keys."""

    wake: ManagerWake = "every_write"
    pass_minutes: int = 0
    configured: bool = False
    invalid: tuple[str, ...] = ()


def validate(key: str, raw: str) -> str:
    """Validate a project setting before it reaches the store."""
    if key == WAKE and raw in {"every_write", "schedule"}:
        return raw
    if key == PASS_MINUTES and raw.isascii() and raw.isdecimal():
        try:
            return str(int(raw))
        except ValueError:
            pass
    raise ValueError(f"invalid {key}: {raw!r}")


def policy(store: ContextStore, project: str) -> Policy:
    """Read policy for one fleet project, with explicit defaults."""
    wake = store.project_setting(project, WAKE)
    raw = store.project_setting(project, PASS_MINUTES)
    invalid: list[str] = []
    minutes = 0
    if raw is not None:
        try:
            minutes = int(validate(PASS_MINUTES, raw))
        except ValueError:
            invalid.append(PASS_MINUTES)
    if wake not in (None, "every_write", "schedule"):
        invalid.append(WAKE)
    return Policy(
        "schedule" if wake == "schedule" else "every_write",
        minutes,
        wake is not None or raw is not None,
        tuple(invalid),
    )


def bound(store: ContextStore, row: FleetAgent) -> TeamSession | None:
    """Return the live board session that occupies this project's reserved row."""
    if row.label != LABEL or row.ended_at is not None or row.session_id is None:
        return None
    session = store.get_session(row.session_id)
    if (
        session is None
        or session.ended_at is not None
        or session.role != LABEL
        or session.project_id != (row.board or row.project_id)
    ):
        return None
    return session if store.fleet_agent_for_session(session.id) == row else None


def mode(store: ContextStore, session: TeamSession) -> tuple[WakeMode, str | None]:
    """Resolve the reader's own fleet and refuse mismatched configured seats."""
    row = store.fleet_agent_for_session(session.id)
    project = row.project_id if row is not None else session.project_id
    settings = policy(store, project)
    if not settings.configured:
        return "every_write", None
    if row is None or bound(store, row) is None:
        return "blocked", project
    return settings.wake, project


def progress_key(project: str, board: str, kind: str) -> str:
    """Build a progress key owned by both its fleet and source board."""
    return f"manager-seat/{project}/{board}/{kind}"


def urgent_position(store: ContextStore, project: str, board: str) -> int:
    """Read the urgent position, starting at retained history on first activation."""
    return int(store.get_meta(progress_key(project, board, "urgent")) or 0)


def advance_urgent(store: ContextStore, project: str, board: str, seq: int) -> None:
    """Record the highest urgent sequence the seat has seen."""
    key = progress_key(project, board, "urgent")
    while True:
        prior = store.get_meta(key)
        if prior is None:
            if store.set_meta_once(key, str(seq)):
                return
        elif int(prior) >= seq or store.compare_meta(key, prior, str(seq)):
            return


@contextmanager
def reserve(project: str) -> Iterator[None]:
    """Serialize pane delivery without holding a store transaction."""
    locks = paths.aisquare_home() / "manager-delivery"
    locks.mkdir(parents=True, exist_ok=True)
    with locking.holding(locks / project, wait_s=0):
        yield
