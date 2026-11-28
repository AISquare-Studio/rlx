"""When something happened, as the boards write it for a person (B1 #6).

A board's feed, its details, its done archive and ``aisquare board``'s text all wrote
``%H:%M`` alone, so yesterday's ``14:05`` read as today's. :func:`stamp` is the one rule
for a moment that has passed (a lease's end too): the clock time alone today, the day
before it on any other day. A rate-limit reset, which lies ahead, has its own
(``core.claude_accounts.format_reset``: a distance and a weekday). :func:`clock_minute` is
no moment shown: it is what tells two sessions of one name apart.

In core so ``services.team`` (the text board) and ``cli.watch`` (the TUI) share it.
"""

from __future__ import annotations

from datetime import UTC, datetime


def _now() -> datetime:
    """The wall clock "today" is read from when the caller names none."""
    return datetime.now(UTC)


def stamp(when: datetime, *, seconds: bool = False, now: datetime | None = None) -> str:
    """``when`` in the user's local time: ``14:05`` today, ``Nov 24 14:05`` another day of
    the year, ``2025-11-24 14:05`` another year; ``seconds`` adds them (``14:05:09``).
    ``now`` is the clock "today" is read from (the wall clock when ``None``)."""
    local = when.astimezone()
    today = (now if now is not None else _now()).astimezone().date()
    hours = f"{local:%H:%M:%S}" if seconds else f"{local:%H:%M}"
    if local.date() == today:
        return hours
    if local.year == today.year:
        return f"{local:%b} {local.day} {hours}"
    return f"{local:%Y-%m-%d} {hours}"


def clock_minute(when: datetime) -> str:
    """The local minute ``when`` fell in, ``14:02``, with no day: what ``cli.board_names`` tells
    two sessions of one name apart by, and what ``services.team.board_sessions`` reads their
    namesakes by, so the two can never differ."""
    return f"{when.astimezone():%H:%M}"
