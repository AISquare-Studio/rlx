"""The wake policy of a fleet's manager, per fleet project (#551, #41).

- ``every_write`` (the default): every board write the manager wakes for wakes it, as
  always (``services.team.manager_wakes``).
- ``schedule``: urgent news wakes it at once, and routine news at most every
  ``manager_pass_minutes`` minutes (``services.team.wake_now``).

Stored in the store's ``project_setting`` table, under the fleet project whose ``manager``
row the manager runs in: a writer's process, the manager's own hooks and the tickers that
send a held wake (the rlx service, ``asq``, ``rlx watch``) all read the same two keys.
``rlx fleet manager-policy`` reads, sets and clears them.

The policy says only WHEN news reaches the manager, never how it is delivered, so 0.10.3's
rlxd (an event bus and a scheduler, no typing into panes) can honour the same keys.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

from aisquare.core.store import ContextStore

ManagerWake = Literal["every_write", "schedule"]
WAKE = "manager_wake"
PASS_MINUTES = "manager_pass_minutes"
KEYS = (WAKE, PASS_MINUTES)
DEFAULT_MINUTES = 15
"""Routine news's interval under ``schedule`` when ``manager_pass_minutes`` is not set. 0
(routine news never wakes the manager) only when set explicitly: a waiting manager would
otherwise never hear a result, and a fleet could stall with nobody noticing (#41)."""
EVENT_PROJECT = "manager_project"
"""The ``meta`` key, on a board event, of the fleet project whose manager it is for: stamped
where a question, a sessionless note or a fleet's system event is written, so the managers of
several fleets on one hub board each wake for their own alone."""


@dataclass(frozen=True)
class Policy:
    """One fleet project's policy, the defaults filled in, and the stored keys that were not
    valid (and so read as their default)."""

    wake: ManagerWake = "every_write"
    minutes: int = DEFAULT_MINUTES
    invalid: tuple[str, ...] = ()

    @property
    def scheduled(self) -> bool:
        return self.wake == "schedule"


def validate(key: str, raw: str) -> str:
    """``raw`` as ``key`` stores it, or ``ValueError``: a wake mode, or a whole number of
    minutes."""
    value = raw.strip()
    if key == WAKE and value in ("every_write", "schedule"):
        return value
    if key == PASS_MINUTES and value.isascii() and value.isdecimal():
        return str(int(value))
    raise ValueError(f"invalid {key}: {raw!r}")


def policy(store: ContextStore, project_id: str) -> Policy:
    """``project_id``'s policy: a key not set, or not valid, reads as its default."""
    invalid: list[str] = []
    wake: ManagerWake = "every_write"
    stored = store.project_setting(project_id, WAKE)
    if stored == "schedule":
        wake = "schedule"
    elif stored not in (None, "every_write"):
        invalid.append(WAKE)
    minutes = DEFAULT_MINUTES
    raw = store.project_setting(project_id, PASS_MINUTES)
    if raw is not None:
        try:
            minutes = int(validate(PASS_MINUTES, raw))
        except ValueError:
            invalid.append(PASS_MINUTES)
    return Policy(wake, minutes, tuple(invalid))
