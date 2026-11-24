"""``asq``'s accounts read reuses a slot's identity parse while its ``.claude.json`` holds the
same bytes.

The status line and the Accounts page follow ``claude_accounts.overview``, which ``asq`` reads
after every fleet frame. ``read_identity`` keeps its parse of each ``.claude.json`` under the
SHA-256 digest of the bytes it parsed (``core.parsed_files``): each lookup reads the file and parses
it only when the digest differs. These tests count the identity reads and parses, read what the read
after each kind of change shows, and read what the cache keeps.
"""

from __future__ import annotations

import base64
import json
import os
import sys
import time
from collections import OrderedDict
from collections.abc import Iterator
from contextlib import contextmanager
from datetime import timedelta
from pathlib import Path
from typing import Any

import pytest

from aisquare.core import claude_accounts as core
from aisquare.core import parsed_files
from aisquare.models import AccountsOverview, ClaudeAccount
from aisquare.services import claude_accounts as service
from tests.conftest import CLAUDE_ACCOUNTS_NOW

#: Each slot's ``.claude.json`` carries a project history as Claude Code keeps one.
HISTORY = {f"/work/project-{n}": {"hasTrustDialogAccepted": True} for n in range(200)}


def sign_in(account: ClaudeAccount, email: str) -> None:
    """What a signed-in Claude Code leaves in a slot: its ``.claude.json`` and its login."""
    target = core.claude_json_path(account)
    target.parent.mkdir(parents=True, exist_ok=True)
    body = {"oauthAccount": {"emailAddress": email}, "projects": HISTORY}
    target.write_text(json.dumps(body), encoding="utf-8")
    expires = CLAUDE_ACCOUNTS_NOW + timedelta(hours=7)
    login = {
        "claudeAiOauth": {
            "accessToken": "sk-ant-oat01-test",
            "expiresAt": int(expires.timestamp() * 1000),
            "subscriptionType": "max",
            "rateLimitTier": "default_claude_max_5x",
        }
    }
    core.credentials_path(account).write_text(json.dumps(login), encoding="utf-8")


def codex_sign_in(account: ClaudeAccount, email: str) -> None:
    """What ``codex login`` leaves in a Codex slot: its ``auth.json``, an id token's claims."""
    claims = {"email": email, "https://api.openai.com/auth": {"chatgpt_plan_type": "pro"}}
    payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
    tokens = {"id_token": f"header.{payload}.signature", "refresh_token": "rt-test"}
    (account.config_dir / "auth.json").write_text(json.dumps({"tokens": tokens}), encoding="utf-8")


@pytest.fixture
def slots(fake_home: Path) -> list[ClaudeAccount]:
    """Claude Code's plain slot and two managed ones, each signed in with a settings file, and
    a Codex slot signed in: three Claude ``.claude.json`` files."""
    accounts = [core.default_account(), core.create_account(), core.create_account()]
    for n, account in enumerate(accounts):
        sign_in(account, f"slot{n}@example.com")
        (account.config_dir / "settings.json").write_text('{"theme": "dark"}\n', encoding="utf-8")
    codex = core.create_account("codex")
    codex_sign_in(codex, "codex@example.com")
    return [*accounts, codex]


@pytest.fixture(autouse=True)
def no_kept_parses() -> None:
    """Each test starts with no kept parse (``core.parsed_files``)."""
    with parsed_files._LOCK:
        parsed_files._KEPT.clear()


@contextmanager
def identity_io(monkeypatch: pytest.MonkeyPatch) -> Iterator[tuple[list[Path], list[Path]]]:
    """The ``.claude.json`` files read through ``Path.read_bytes`` and the ones the identity
    parser parsed (its JSON step, ``json_object``) in the block, once each time."""
    read: list[Path] = []
    parsed: list[Path] = []
    read_bytes, json_object = Path.read_bytes, parsed_files.json_object

    def reading(path: Path) -> bytes:
        if path.name == ".claude.json":
            read.append(path)
        return read_bytes(path)

    def parsing(path: Path, text: str) -> Any:
        parsed.append(path)
        return json_object(path, text)

    with monkeypatch.context() as patched:
        patched.setattr(Path, "read_bytes", reading)
        patched.setattr(core, "json_object", parsing)
        yield read, parsed


def rows(overview: AccountsOverview) -> list[tuple[object, ...]]:
    """What the status line and the Accounts page show of each slot, in order."""
    return [
        (
            status.account.slot,
            status.label,
            status.identity.email if status.identity is not None else None,
            status.signed_in,
            status.token_state,
            status.subscription,
            status.hooks_installed,
        )
        for status in overview.accounts
    ]


#: :func:`rows` of the ``slots`` fixture, as the read that parsed every file each time showed them.
FIRST = [
    (1, "plain claude", "slot0@example.com", True, "ok", "max 5x", False),
    (2, "account 2", "slot1@example.com", True, "ok", "max 5x", False),
    (3, "account 3", "slot2@example.com", True, "ok", "max 5x", False),
    (4, "account 4", "codex@example.com", True, "ok", "pro", False),
]


def _email(path: Path) -> str | None:
    identity = core.read_identity(path)
    return identity.email if identity is not None else None


def _write_email(path: Path, email: str) -> None:
    path.write_text(json.dumps({"oauthAccount": {"emailAddress": email}}), encoding="utf-8")


def _kept(path: Path) -> list[object]:
    return [kept for (key_path, _parse), kept in parsed_files._KEPT.items() if key_path == path]


def test_ten_warm_overviews_read_each_claude_json_three_times_an_overview_and_parse_none(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """The ``slots`` fixture: three Claude ``.claude.json`` files, each asked for its identity
    three times an overview."""
    claude_jsons = {core.claude_json_path(account) for account in slots[:3]}
    assert rows(service.overview()) == FIRST
    with identity_io(monkeypatch) as (read, parsed):
        for _ in range(10):
            assert rows(service.overview()) == FIRST
    assert parsed == []
    assert sorted(read) == sorted(path for path in claude_jsons for _ in range(30))


def test_a_changed_claude_json_is_the_one_the_next_overview_parses_and_shows(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    service.overview()
    changed = core.claude_json_path(slots[1])
    _write_email(changed, "another.login@example.com")
    with identity_io(monkeypatch) as (_read, parsed):
        shown = rows(service.overview())
    assert shown[1][2] == "another.login@example.com"
    assert parsed == [changed]


def test_a_rewrite_holding_the_stat_of_the_file_it_replaced_shows_on_the_next_read(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """Slot 2's ``.claude.json`` holds A, written an hour ago, and is read once; then B, of the same
    size, is written in place while ``os.stat`` of the file answers A's stat (size, inode,
    modification and change times). This test reads the email of the read after B."""
    path = core.claude_json_path(slots[1])
    _write_email(path, "first@example.com")
    old = time.time() - 3600
    os.utime(path, (old, old))
    assert _email(path) == "first@example.com"
    pinned = os.stat(path)
    real_stat = os.stat

    def as_before(target: Any, *args: Any, **kwargs: Any) -> os.stat_result:
        if not args and not kwargs and Path(target) == path:
            return pinned
        return real_stat(target, *args, **kwargs)

    monkeypatch.setattr(os, "stat", as_before)
    _write_email(path, "again@example.com")
    assert _email(path) == "again@example.com"


@pytest.mark.skipif(sys.platform == "win32", reason="a symlink needs a privilege on Windows")
def test_a_link_moved_to_b_while_read_and_back_to_a_gives_b_then_a(
    slots: list[ClaudeAccount], tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Slot 2's ``.claude.json`` is a link to file A. Its first read moves the link to B, reads B,
    and moves the link back to A before the lookup returns. This test reads that lookup's email,
    then the next lookup's."""
    path = core.claude_json_path(slots[1])
    a, b = tmp_path / "a.json", tmp_path / "b.json"
    _write_email(a, "a@example.com")
    _write_email(b, "b@example.com")
    path.unlink()
    path.symlink_to(a)
    read_bytes = Path.read_bytes
    moved: list[bool] = []

    def moving(self: Path) -> bytes:
        if self != path or moved:
            return read_bytes(self)
        moved.append(True)
        self.unlink()
        self.symlink_to(b)
        body = read_bytes(self)
        self.unlink()
        self.symlink_to(a)
        return body

    with monkeypatch.context() as patched:
        patched.setattr(Path, "read_bytes", moving)
        assert _email(path) == "b@example.com"
    assert _email(path) == "a@example.com"


def test_a_failed_read_of_an_unchanged_warm_file_gives_none_then_the_next_read_parses_it(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    path = core.claude_json_path(slots[1])
    assert _email(path) == "slot1@example.com"
    read_bytes = Path.read_bytes

    def refused(self: Path) -> bytes:
        if self == path:
            raise PermissionError(13, "Permission denied", str(self))
        return read_bytes(self)

    with monkeypatch.context() as patched:
        patched.setattr(Path, "read_bytes", refused)
        assert _email(path) is None
    assert _kept(path) == []
    with identity_io(monkeypatch) as (_read, parsed):
        assert _email(path) == "slot1@example.com"
    assert parsed == [path]


def test_an_older_reader_publishing_last_gives_each_reader_its_own_identity(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """Reader 1 reads A; before it parses, the file becomes B and reader 2 reads, parses and keeps
    B; then reader 1 parses and keeps A. This test reads both readers' emails, then the email and
    the parses of the next read of B."""
    path = core.claude_json_path(slots[1])
    _write_email(path, "a@example.com")
    read_bytes = Path.read_bytes
    second: list[str | None] = []

    def overtaken(self: Path) -> bytes:
        body = read_bytes(self)
        if self == path and not second:
            second.append(None)
            _write_email(path, "b@example.com")
            second[0] = _email(path)
        return body

    with monkeypatch.context() as patched:
        patched.setattr(Path, "read_bytes", overtaken)
        first = _email(path)
    assert (first, second) == ("a@example.com", ["b@example.com"])
    with identity_io(monkeypatch) as (_read, parsed):
        assert _email(path) == "b@example.com"
    assert parsed == [path]


def test_a_claude_json_removed_or_empty_gives_none_and_keeps_nothing(
    slots: list[ClaudeAccount],
) -> None:
    removed, emptied = core.claude_json_path(slots[1]), core.claude_json_path(slots[2])
    assert _email(removed) == "slot1@example.com" and _email(emptied) == "slot2@example.com"
    removed.unlink()
    emptied.write_text("", encoding="utf-8")
    assert (_email(removed), _email(emptied)) == (None, None)
    assert _kept(removed) == [] and _kept(emptied) == []


def test_a_valid_claude_json_with_no_login_gives_none_and_is_not_parsed_again(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    path = core.claude_json_path(slots[1])
    path.write_text(json.dumps({"projects": HISTORY}), encoding="utf-8")
    assert _email(path) is None
    with identity_io(monkeypatch) as (_read, parsed):
        assert _email(path) is None
    assert parsed == []


def test_a_callers_change_to_its_identity_is_not_handed_to_the_next_caller(
    slots: list[ClaudeAccount],
) -> None:
    path = core.claude_json_path(slots[1])
    first = core.read_identity(path)
    assert first is not None
    first.email = "changed.by.its.caller@example.com"
    assert _email(path) == "slot1@example.com"


def test_an_overview_sweeps_the_parse_of_a_removed_slot_and_keeps_no_login_file(
    slots: list[ClaudeAccount],
) -> None:
    gone = core.claude_json_path(slots[1])
    service.overview()
    names = {path.name for path, _parse in parsed_files._KEPT}
    assert names == {".claude.json"}
    assert _kept(gone) != []
    core.remove_account(slots[1])
    service.overview()
    assert _kept(gone) == []
    assert _kept(core.claude_json_path(slots[2])) != []


def test_the_cap_drops_the_least_recently_used_parse(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(parsed_files, "MAX_KEPT", 2)
    first, second, third = (core.claude_json_path(account) for account in slots[:3])
    _email(first)
    _email(second)
    _email(first)
    _email(third)
    assert _kept(first) != [] and _kept(third) != []
    assert _kept(second) == []


def test_each_lookup_keep_drop_eviction_and_sweep_of_a_parse_holds_the_lock(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """This test reads, at each lookup, keep, drop, eviction and sweep of the kept parses over
    three overviews of the ``slots`` fixture with :data:`~aisquare.core.parsed_files.MAX_KEPT`
    patched below its three ``.claude.json`` files (the third overview once slot 2 is removed
    and slot 3's ``.claude.json`` is gone), whether the map's lock is held, by kind."""
    held: dict[str, list[bool]] = {
        "lookup": [],
        "keep": [],
        "drop": [],
        "eviction": [],
        "sweep": [],
    }

    class Noting(OrderedDict[Any, Any]):
        def get(self, key: Any, default: Any = None) -> Any:
            held["lookup"].append(parsed_files._LOCK.locked())
            return super().get(key, default)

        def move_to_end(self, key: Any, last: bool = True) -> None:
            held["lookup"].append(parsed_files._LOCK.locked())
            super().move_to_end(key, last)

        def __setitem__(self, key: Any, value: Any) -> None:
            held["keep"].append(parsed_files._LOCK.locked())
            super().__setitem__(key, value)

        def pop(self, key: Any, default: Any = None) -> Any:
            held["drop"].append(parsed_files._LOCK.locked())
            return super().pop(key, default)

        def popitem(self, last: bool = True) -> tuple[Any, Any]:
            held["eviction"].append(parsed_files._LOCK.locked())
            return super().popitem(last)

        def __iter__(self) -> Iterator[Any]:
            held["sweep"].append(parsed_files._LOCK.locked())
            return super().__iter__()

    monkeypatch.setattr(parsed_files, "_KEPT", Noting())
    monkeypatch.setattr(parsed_files, "MAX_KEPT", 2)
    service.overview()
    service.overview()
    core.remove_account(slots[1])
    core.claude_json_path(slots[2]).unlink()
    service.overview()
    assert all(held.values()), held
    assert all(all(seen) for seen in held.values()), held


#: Nesting that ``json.loads`` decodes and ``copy.deepcopy`` cannot copy within Python's
#: default recursion limit.
DEEP = 800


def test_a_settings_file_holding_a_deep_unrelated_value_reads_as_the_base_read_it(
    slots: list[ClaudeAccount],
) -> None:
    """This test reads the rows of two overviews of the ``slots`` fixture once slot 2's
    ``settings.json`` holds, beside its theme, a list nested :data:`DEEP` deep."""
    deep = "[" * DEEP + "]" * DEEP
    (slots[1].config_dir / "settings.json").write_text(
        '{"theme": "dark", "deep": ' + deep + "}\n", encoding="utf-8"
    )
    assert rows(service.overview()) == FIRST
    assert rows(service.overview()) == FIRST


def test_a_slot_added_is_listed_and_one_removed_is_dropped_by_the_next_read(
    slots: list[ClaudeAccount],
) -> None:
    service.overview()
    added = core.create_account()
    sign_in(added, "added@example.com")
    shown = rows(service.overview())
    assert shown[-1][:3] == (added.slot, f"account {added.slot}", "added@example.com")
    core.remove_account(slots[1])
    assert [row[0] for row in rows(service.overview())] == [1, 3, 4, added.slot]


def test_an_alias_set_is_shown_by_the_next_read(slots: list[ClaudeAccount]) -> None:
    service.overview()
    service.set_alias("3", "work")
    assert rows(service.overview())[2][1] == "work"


def test_a_login_expiring_with_no_file_changed_reads_expired_by_the_next_read(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    assert [row[4] for row in rows(service.overview())[:3]] == ["ok", "ok", "ok"]
    monkeypatch.setattr(core, "_now", lambda: CLAUDE_ACCOUNTS_NOW + timedelta(hours=8))
    assert [row[4] for row in rows(service.overview())[:3]] == ["expired", "expired", "expired"]
