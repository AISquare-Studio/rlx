"""The brain's ``usage`` (task #15): each Claude Code account's room, for rlxd's routes by
connector. Its fullest window, percent used, from the reading ON RECORD, and whether it
refuses requests now, with when that ends. The usage endpoint is asked nothing."""

from __future__ import annotations

import sqlite3
from datetime import UTC, datetime, timedelta
from typing import Any

import pytest

from aisquare.core import claude_accounts as core
from aisquare.core.store import store_session
from aisquare.services import brain
from aisquare.services import claude_accounts as service
from tests.test_claude_accounts import NOW
from tests.test_usage_aware_accounts import _payload, _slot, _Usage

#: The accounts code's clock stopped at ``NOW`` and credentials in files, as the usage suites
#: read them (``tests/conftest.py``'s ``fake_home``).
pytestmark = pytest.mark.usefixtures("fake_home")


def _usage() -> dict[int, dict[str, Any]]:
    result = brain.answer("usage", {}, brain.METHODS)
    assert result["ok"] is True, result
    return {row["slot"]: row for row in result["result"]["accounts"]}


def _z(moment: datetime) -> str:
    return moment.astimezone(UTC).isoformat().replace("+00:00", "Z")


def _two(monkeypatch: pytest.MonkeyPatch, **by_token: dict[str, Any] | int) -> _Usage:
    """Slots ``work`` and ``home`` signed in, each with its token; a scripted endpoint."""
    _slot("work@example.com", "tok-work")
    _slot("home@example.com", "tok-home")
    endpoint = _Usage(dict(by_token))
    monkeypatch.setattr(service, "_http_get", endpoint)
    return endpoint


def _on_record(endpoint: _Usage) -> None:
    """Put each account's reading on record, as the Accounts page or a launch would, and
    forget the requests that took: what is counted after is what ``usage`` asks."""
    service.cached_usage(service.list_accounts())
    endpoint.calls.clear()


def _later(hours: float) -> datetime:
    """``hours`` after the accounts code's clock, which the suite stops at ``NOW``."""
    return NOW + timedelta(hours=hours)


def test_each_accounts_room_is_its_reading_on_record_and_nothing_is_asked(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    endpoint = _two(
        monkeypatch,
        **{
            "tok-work": _payload(70, resets_at=_later(3)),
            "tok-home": _payload(20, resets_at=_later(3)),
        },
    )
    _on_record(endpoint)
    rows = _usage()
    assert endpoint.calls == []
    work, home = (row for slot, row in sorted(rows.items()) if slot != 1)
    assert work == {"slot": work["slot"], "used_percent": 70.0, "limited": False, "resets_at": None}
    assert home["used_percent"] == 20.0
    assert home["limited"] is False


def test_an_account_with_no_reading_on_record_has_none_and_is_not_read(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    endpoint = _two(monkeypatch, **{"tok-work": _payload(10), "tok-home": _payload(10)})
    rows = _usage()
    assert endpoint.calls == [], "usage fetched a reading"
    managed = [row for slot, row in rows.items() if slot != 1]
    assert [row["used_percent"] for row in managed] == [None, None]
    assert [row["limited"] for row in managed] == [False, False]


def test_the_accounts_are_those_accounts_lists_in_its_order(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _two(monkeypatch)
    listed = brain.answer("accounts", {}, brain.METHODS)["result"]["accounts"]
    usage = brain.answer("usage", {}, brain.METHODS)["result"]["accounts"]
    assert [row["slot"] for row in usage] == [row["slot"] for row in listed]


def test_another_agents_slot_is_not_listed() -> None:
    codex = core.create_account("codex")
    assert codex.slot not in _usage()


def test_a_limit_mark_every_model_shares_is_limited_until_it_ends(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _two(monkeypatch)
    work = max(_usage())
    until = _later(5).replace(microsecond=0)
    service.remember_limit(work, "week", until)
    row = _usage()[work]
    assert row["limited"] is True
    assert row["resets_at"] == _z(until)


def test_a_mark_that_has_ended_or_is_one_familys_limits_nothing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _two(monkeypatch)
    work = max(_usage())
    # An Opus week shuts the account to an Opus agent alone: rlxd's runs aren't asked
    # which model they are, so it is no limit of the account's.
    service.remember_limit(work, "opus", _later(5))
    service.remember_limit(work, "session", NOW - timedelta(minutes=1))
    row = _usage()[work]
    assert (row["limited"], row["resets_at"]) == (False, None)


def test_a_spent_window_on_record_is_limited_until_it_resets(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    resets = _later(2).replace(microsecond=0)
    endpoint = _two(
        monkeypatch,
        **{"tok-work": _payload(100, resets_at=resets), "tok-home": _payload(5)},
    )
    _on_record(endpoint)
    rows = _usage()
    assert endpoint.calls == []
    spent = [row for row in rows.values() if row["limited"]]
    assert len(spent) == 1, rows
    assert spent[0]["used_percent"] == 100.0
    assert spent[0]["resets_at"] == _z(resets)


def test_the_later_of_a_mark_and_a_spent_window_is_when_it_resets(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    window = _later(2).replace(microsecond=0)
    endpoint = _two(
        monkeypatch,
        **{
            "tok-work": _payload(100, resets_at=window),
            "tok-home": _payload(100, resets_at=window),
        },
    )
    _on_record(endpoint)
    slots = sorted(slot for slot in _usage() if slot != 1)
    later = _later(30).replace(microsecond=0)
    sooner = _later(1).replace(microsecond=0)
    service.remember_limit(slots[0], "week", later)
    service.remember_limit(slots[1], "week", sooner)
    rows = _usage()
    assert rows[slots[0]]["resets_at"] == _z(later)
    assert rows[slots[1]]["resets_at"] == _z(window)


#: Every writer of the store's that a usage read could reach: each fails the test.
_WRITERS = (
    "set_ui_state",
    "replace_ui_state",
    "set_meta",
    "set_meta_once",
    "compare_meta",
    "delete_meta",
    "touch_meta",
    "add_usage_sample",
    "delete_usage_samples",
)

#: Every reader of the Keychain's: each fails the test.
_KEYCHAIN = ("read_keychain", "_ask_keychain", "item_login", "stored_login", "credentials")


def _store_rows() -> dict[str, list[str]]:
    """Every row of rlx's store, read with a connection of the test's own, read-only."""
    from aisquare.core import paths

    con = sqlite3.connect(f"file:{paths.db_path()}?mode=ro", uri=True)
    try:
        tables = [
            row[0] for row in con.execute("SELECT name FROM sqlite_master WHERE type='table'")
        ]
        return {
            table: sorted(repr(row) for row in con.execute(f'SELECT * FROM "{table}"'))
            for table in tables
        }
    finally:
        con.close()


def _on_a_mac_that_must_not_be_asked(monkeypatch: pytest.MonkeyPatch) -> None:
    """Logins in Keychain items (``keychain_platform`` True), and a test that fails on any
    Keychain read and any write of the store."""
    from aisquare.core.store import ContextStore

    def asked(*_args: Any, **_kwargs: Any) -> Any:
        raise AssertionError("usage read the Keychain")

    def written(*_args: Any, **_kwargs: Any) -> Any:
        raise AssertionError("usage wrote to rlx's store")

    monkeypatch.setattr(core, "keychain_platform", lambda: True)
    for name in _KEYCHAIN:
        monkeypatch.setattr(core, name, asked)
    for name in _WRITERS:
        monkeypatch.setattr(ContextStore, name, written)


def test_on_a_mac_usage_reads_no_keychain_and_writes_nothing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    endpoint = _two(
        monkeypatch,
        **{
            "tok-work": _payload(70, resets_at=_later(3)),
            "tok-home": _payload(20, resets_at=_later(3)),
        },
    )
    _on_record(endpoint)
    slots = sorted(account.slot for account in core.managed_accounts())
    until = _later(5).replace(microsecond=0)
    service.remember_limit(slots[0], "week", until)
    # Each login said in its row, as a process that read its Keychain item says it.
    with store_session() as store:
        for account in core.managed_accounts():
            key = service._seen_key(account)
            assert key is not None
            seen = service.SeenLogin(token=service._token_fingerprint(account, NOW))
            store.replace_ui_state(key, store.ui_state(key), seen.dump())
    _on_a_mac_that_must_not_be_asked(monkeypatch)
    before = _store_rows()
    rows = _usage()
    assert _store_rows() == before, "usage changed rlx's store"
    assert endpoint.calls == []
    # Served by the logins the rows name, as a slot whose Keychain is locked is.
    assert [rows[slot]["used_percent"] for slot in slots] == [70.0, 20.0]
    assert rows[slots[0]]["limited"] is True
    assert rows[slots[0]]["resets_at"] == _z(until)
    assert rows[slots[1]]["limited"] is False


def test_on_a_mac_a_slot_no_row_names_has_no_reading_and_nothing_is_asked(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    endpoint = _two(
        monkeypatch,
        **{"tok-work": _payload(70, resets_at=_later(3)), "tok-home": _payload(20)},
    )
    _on_record(endpoint)
    _on_a_mac_that_must_not_be_asked(monkeypatch)
    before = _store_rows()
    rows = _usage()
    assert _store_rows() == before
    assert endpoint.calls == []
    managed = [row for slot, row in rows.items() if slot != 1]
    assert [row["used_percent"] for row in managed] == [None, None]
