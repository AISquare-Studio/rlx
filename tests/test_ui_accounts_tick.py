"""``asq``'s accounts read parses a slot's ``.claude.json`` and its settings checks again only once
the file changed.

The status line and the Accounts page follow ``claude_accounts.overview``, which ``asq`` reads
after every fleet frame. These tests count the ``.claude.json`` reads and the settings parses
over reads with nothing changed, read what the read after each kind of change shows, and read
what the parse cache (``core.parsed_files``) keeps. The cache's clock is frozen at the test's
start (``cache_clock``), and the slots' files are written an hour before it.
"""

from __future__ import annotations

import base64
import json
import os
import time
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from datetime import timedelta
from pathlib import Path
from typing import Any

import pytest

from aisquare.core import agents as agent_core
from aisquare.core import claude_accounts as core
from aisquare.core import parsed_files
from aisquare.core.agent_adapters import claude_code
from aisquare.models import AccountsOverview, ClaudeAccount
from aisquare.services import claude_accounts as service
from tests.conftest import CLAUDE_ACCOUNTS_NOW

#: Each slot's ``.claude.json`` carries a project history as Claude Code keeps one.
HISTORY = {f"/work/project-{n}": {"hasTrustDialogAccepted": True} for n in range(200)}

#: The files a slot keeps its account in, by name.
ACCOUNT_FILES = frozenset(
    {".claude.json", ".credentials.json", "settings.json", "installed_plugins.json", "auth.json"}
)


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
def cache_clock(monkeypatch: pytest.MonkeyPatch) -> list[float]:
    """The parse cache's clock (``parsed_files._now``), frozen at the test's start; a test moves
    it by setting its one item."""
    clock = [time.time()]
    monkeypatch.setattr(parsed_files, "_now", lambda: clock[0])
    return clock


def settle(fake_home: Path) -> None:
    """Every account file last written an hour before the cache's clock."""
    old = parsed_files._now() - 3600
    for root in (fake_home, core.accounts_root(), core.accounts_root("codex")):
        for path in root.rglob("*"):
            if path.is_file():
                os.utime(path, (old, old))


@pytest.fixture
def slots(fake_home: Path, cache_clock: list[float]) -> list[ClaudeAccount]:
    """Claude Code's plain slot and two managed ones, each signed in, each with a settings
    file, and a Codex slot signed in, all written an hour before the cache's clock."""
    accounts = [core.default_account(), core.create_account(), core.create_account()]
    for n, account in enumerate(accounts):
        sign_in(account, f"slot{n}@example.com")
        (account.config_dir / "settings.json").write_text('{"theme": "dark"}\n', encoding="utf-8")
    codex = core.create_account("codex")
    codex_sign_in(codex, "codex@example.com")
    settle(fake_home)
    return [*accounts, codex]


@contextmanager
def account_files_read(fake_home: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[list[Path]]:
    """The account files (:data:`ACCOUNT_FILES`) under the home and the slots' roots read in the
    block, once each time one is read."""
    read: list[Path] = []
    read_text, read_bytes = Path.read_text, Path.read_bytes
    roots = (fake_home, core.accounts_root(), core.accounts_root("codex"))

    def note(path: Path) -> None:
        if path.name in ACCOUNT_FILES and any(path.is_relative_to(root) for root in roots):
            read.append(path)

    def text(path: Path, *args: Any, **kwargs: Any) -> str:
        note(path)
        return read_text(path, *args, **kwargs)

    def raw(path: Path) -> bytes:
        note(path)
        return read_bytes(path)

    with monkeypatch.context() as patched:
        patched.setattr(Path, "read_text", text)
        patched.setattr(Path, "read_bytes", raw)
        yield read


def settings_parsed(monkeypatch: pytest.MonkeyPatch) -> list[Path]:
    """Each settings file the enabled-plugins and hooks-switch parsers parse from now on, once
    each time one is parsed (the parsers replaced by counting ones, which the cache keys by)."""
    parsed: list[Path] = []

    def counting(parse: Callable[[Path, str], Any]) -> Callable[[Path, str], Any]:
        def parse_counted(path: Path, text: str) -> Any:
            parsed.append(path)
            return parse(path, text)

        return parse_counted

    monkeypatch.setattr(agent_core, "_enabled_in_file", counting(agent_core._enabled_in_file))
    monkeypatch.setattr(claude_code, "_hooks_off_in_file", counting(claude_code._hooks_off_in_file))
    return parsed


def claude_jsons(read: list[Path]) -> list[Path]:
    return [path for path in read if path.name == ".claude.json"]


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


def test_ten_reads_with_nothing_changed_read_no_claude_json_and_parse_no_settings_check(
    slots: list[ClaudeAccount], fake_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    parsed = settings_parsed(monkeypatch)
    assert rows(service.overview()) == FIRST
    parsed.clear()
    with account_files_read(fake_home, monkeypatch) as read:
        for _ in range(10):
            assert rows(service.overview()) == FIRST
    assert claude_jsons(read) == []
    assert parsed == []


def test_a_changed_claude_json_is_the_one_claude_json_the_next_read_reads_and_shows(
    slots: list[ClaudeAccount], fake_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    service.overview()
    changed = core.claude_json_path(slots[1])
    changed.write_text(
        json.dumps({"oauthAccount": {"emailAddress": "another.login@example.com"}}),
        encoding="utf-8",
    )
    with account_files_read(fake_home, monkeypatch) as read:
        shown = rows(service.overview())
    assert shown[1][2] == "another.login@example.com"
    assert set(claude_jsons(read)) == {changed}


def test_a_changed_settings_file_is_the_one_settings_file_the_next_read_parses_and_shows(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    parsed = settings_parsed(monkeypatch)
    service.overview()
    parsed.clear()
    assert agent_core.install_hooks("claude-code", slots[2].config_dir)
    shown = rows(service.overview())
    assert [row[6] for row in shown] == [False, False, True, False]
    assert set(parsed) == {slots[2].config_dir / "settings.json"}


def test_a_slot_added_is_listed_and_one_removed_is_dropped_by_the_next_read(
    slots: list[ClaudeAccount], fake_home: Path
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


def test_a_claude_json_whose_read_failed_shows_its_login_on_the_next_read(
    slots: list[ClaudeAccount], fake_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Slot 2's ``.claude.json`` holds a new login, written an hour before the cache's clock, and
    every read of it in one overview is refused: this test reads slot 2's email in that overview
    and the next."""
    service.overview()
    path = core.claude_json_path(slots[1])
    path.write_text(json.dumps({"oauthAccount": {"emailAddress": "new@example.com"}}))
    settle(fake_home)
    read_text = Path.read_text

    def refused(self: Path, *args: Any, **kwargs: Any) -> str:
        if self == path:
            raise PermissionError(13, "Permission denied", str(self))
        return read_text(self, *args, **kwargs)

    with monkeypatch.context() as patched:
        patched.setattr(Path, "read_text", refused)
        assert rows(service.overview())[1][2] is None
    assert rows(service.overview())[1][2] == "new@example.com"


@pytest.mark.parametrize("name", [".claude.json", "settings.json"])
def test_an_empty_account_file_is_read_as_often_by_the_second_of_two_reads_as_by_the_first(
    slots: list[ClaudeAccount], fake_home: Path, monkeypatch: pytest.MonkeyPatch, name: str
) -> None:
    """Slot 2's ``name`` empty, as a write caught halfway leaves it, an hour before the cache's
    clock: this test counts the reads of the file in each of two overviews."""
    path = slots[1].config_dir / name
    path.write_text("", encoding="utf-8")
    settle(fake_home)
    with account_files_read(fake_home, monkeypatch) as first:
        service.overview()
    with account_files_read(fake_home, monkeypatch) as second:
        service.overview()
    assert second.count(path) == first.count(path) > 0


def _pin(path: Path, monkeypatch: pytest.MonkeyPatch) -> os.stat_result:
    """``path``'s stat pinned at what it is now, as a clock that does not tick between two
    writes shows them."""
    pinned = os.stat(path)
    real_stat = os.stat

    def coarse(target: Any, *args: Any, **kwargs: Any) -> os.stat_result:
        if not args and not kwargs and Path(target) == path:
            return pinned
        return real_stat(target, *args, **kwargs)

    monkeypatch.setattr(os, "stat", coarse)
    return pinned


def test_two_writes_with_one_stat_show_the_second_on_the_next_read(
    slots: list[ClaudeAccount], cache_clock: list[float], monkeypatch: pytest.MonkeyPatch
) -> None:
    """This test pins the stat of slot 2's ``.claude.json`` at a first write's, with the cache's
    clock at that write's time, writes the file again at the same size, and reads the email in
    an overview after each write."""
    path = core.claude_json_path(slots[1])
    path.write_text(json.dumps({"oauthAccount": {"emailAddress": "first@example.com"}}))
    pinned = _pin(path, monkeypatch)
    cache_clock[0] = pinned.st_mtime
    assert rows(service.overview())[1][2] == "first@example.com"
    path.write_text(json.dumps({"oauthAccount": {"emailAddress": "again@example.com"}}))
    assert rows(service.overview())[1][2] == "again@example.com"


def test_a_write_with_one_stat_landing_while_a_read_pauses_past_settled_shows_on_the_next_read(
    slots: list[ClaudeAccount], cache_clock: list[float], monkeypatch: pytest.MonkeyPatch
) -> None:
    """This test pins the stat of slot 2's ``.claude.json`` at a first write's, with the cache's
    clock at that write's time; the first read of the file in an overview moves the clock past
    ``SETTLED`` and writes the file again at the same size. It reads the email the next
    overview shows."""
    path = core.claude_json_path(slots[1])
    path.write_text(json.dumps({"oauthAccount": {"emailAddress": "first@example.com"}}))
    pinned = _pin(path, monkeypatch)
    cache_clock[0] = pinned.st_mtime
    read_text = Path.read_text

    def pausing(self: Path, *args: Any, **kwargs: Any) -> str:
        text = read_text(self, *args, **kwargs)
        if self == path and "first@" in text:
            cache_clock[0] += parsed_files.SETTLED + 1
            self.write_text(json.dumps({"oauthAccount": {"emailAddress": "again@example.com"}}))
        return text

    with monkeypatch.context() as patched:
        patched.setattr(Path, "read_text", pausing)
        service.overview()
    assert rows(service.overview())[1][2] == "again@example.com"


def test_a_write_landing_while_a_settled_file_is_read_keeps_no_parse_of_it(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """This test writes slot 2's ``.claude.json`` again, at another size, while one
    ``read_identity`` reads it (it was written an hour before the cache's clock), and reads that
    read's email and whether the file is among the kept parses after it."""
    path = core.claude_json_path(slots[1])
    read_text = Path.read_text

    def written_meanwhile(self: Path, *args: Any, **kwargs: Any) -> str:
        text = read_text(self, *args, **kwargs)
        if self == path and "slot1@" in text:
            self.write_text(json.dumps({"oauthAccount": {"emailAddress": "meanwhile@x.example"}}))
        return text

    with monkeypatch.context() as patched:
        patched.setattr(Path, "read_text", written_meanwhile)
        identity = core.read_identity(path)
    assert identity is not None and identity.email == "slot1@example.com"
    assert all(kept != path for kept, _parse in parsed_files._KEPT)


def test_a_callers_change_to_its_identity_is_not_handed_to_the_next_caller(
    slots: list[ClaudeAccount],
) -> None:
    """This test reads slot 2's identity twice, changes the first answer's email, and reads the
    second's."""
    path = core.claude_json_path(slots[1])
    first = core.read_identity(path)
    assert first is not None
    first.email = "changed.by.its.caller@example.com"
    again = core.read_identity(path)
    assert again is not None and again.email == "slot1@example.com"


def test_each_lookup_keep_and_drop_of_a_parse_in_three_overviews_holds_the_lock(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """This test reads, at each lookup, keep and drop in the kept parses' map over three
    overviews of the ``slots`` fixture (the third once slot 2's ``.claude.json`` is gone),
    whether the map's lock is held, by kind."""
    held: dict[str, list[bool]] = {"lookup": [], "keep": [], "drop": []}

    class Noting(dict[Any, Any]):
        def get(self, key: Any, default: Any = None) -> Any:
            held["lookup"].append(parsed_files._LOCK.locked())
            return super().get(key, default)

        def __setitem__(self, key: Any, value: Any) -> None:
            held["keep"].append(parsed_files._LOCK.locked())
            super().__setitem__(key, value)

        def pop(self, key: Any, default: Any = None) -> Any:
            held["drop"].append(parsed_files._LOCK.locked())
            return super().pop(key, default)

    monkeypatch.setattr(parsed_files, "_KEPT", Noting())
    service.overview()
    service.overview()
    core.claude_json_path(slots[1]).unlink()
    service.overview()
    assert all(held.values()), held
    assert all(all(seen) for seen in held.values()), held


def _kept_names() -> set[str]:
    return {path.name for path, _parse in parsed_files._KEPT}


def test_an_overview_keeps_no_parse_of_a_login_file(slots: list[ClaudeAccount]) -> None:
    """This test reads the names of the files whose parses two overviews of the ``slots``
    fixture keep (three Claude slots with a ``.credentials.json``, a Codex slot with an
    ``auth.json``)."""
    service.overview()
    service.overview()
    kept = _kept_names()
    assert ".claude.json" in kept
    assert kept.isdisjoint({".credentials.json", "auth.json"})


@pytest.mark.parametrize("gone", ["removed", "rewritten", "unreadable"])
def test_a_claude_json_removed_rewritten_or_unreadable_drops_its_kept_parse(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch, gone: str
) -> None:
    """This test reads whether slot 2's ``.claude.json`` is among the kept parses after an
    overview, then after an overview with the file removed, written again after the cache's
    clock, or its read refused."""
    path = core.claude_json_path(slots[1])
    service.overview()
    assert any(kept == path for kept, _parse in parsed_files._KEPT)
    if gone == "removed":
        path.unlink()
        service.overview()
    elif gone == "rewritten":
        path.write_text(json.dumps({"oauthAccount": {"emailAddress": "new@example.com"}}))
        assert rows(service.overview())[1][2] == "new@example.com"
    else:
        read_text = Path.read_text

        def refused(self: Path, *args: Any, **kwargs: Any) -> str:
            if self == path:
                raise PermissionError(13, "Permission denied", str(self))
            return read_text(self, *args, **kwargs)

        path.write_text(json.dumps({"oauthAccount": {"emailAddress": "new@example.com"}}))
        with monkeypatch.context() as patched:
            patched.setattr(Path, "read_text", refused)
            service.overview()
    assert all(kept != path for kept, _parse in parsed_files._KEPT)


#: Nesting that ``json.loads`` decodes and ``copy.deepcopy`` cannot copy within Python's
#: default recursion limit.
DEEP = 800


def test_a_settings_file_holding_a_deep_unrelated_value_reads_as_the_base_read_it(
    slots: list[ClaudeAccount], fake_home: Path
) -> None:
    """This test reads the rows of two overviews of the ``slots`` fixture once slot 2's
    ``settings.json`` holds, beside its theme, a list nested :data:`DEEP` deep."""
    deep = "[" * DEEP + "]" * DEEP
    (slots[1].config_dir / "settings.json").write_text(
        '{"theme": "dark", "deep": ' + deep + "}\n", encoding="utf-8"
    )
    settle(fake_home)
    assert rows(service.overview()) == FIRST
    assert rows(service.overview()) == FIRST
