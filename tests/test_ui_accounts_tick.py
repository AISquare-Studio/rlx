"""``asq``'s accounts read parses a slot's files again only once they changed.

The status line and the Accounts page follow ``claude_accounts.overview``, which ``asq`` reads
after every fleet frame: each slot's ``.claude.json`` (tens of KB to tens of MB), its login, its
settings and its plugins, several times a slot a read. These tests count the account files read
over reads with nothing changed, and read what the read after each kind of change reads and shows.
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
from aisquare.core import codex_accounts
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


def settle(fake_home: Path) -> None:
    """Every account file last written an hour ago, as a slot's files stand between logins."""
    old = time.time() - 3600
    for root in (fake_home, core.accounts_root(), core.accounts_root("codex")):
        for path in root.rglob("*"):
            if path.is_file():
                os.utime(path, (old, old))


@pytest.fixture
def slots(fake_home: Path) -> list[ClaudeAccount]:
    """Claude Code's plain slot and two managed ones, each signed in, each with a settings
    file, and a Codex slot signed in, all written an hour ago."""
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


def test_ten_reads_with_no_account_file_changed_read_no_account_file(
    slots: list[ClaudeAccount], fake_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    assert rows(service.overview()) == FIRST
    with account_files_read(fake_home, monkeypatch) as read:
        for _ in range(10):
            assert rows(service.overview()) == FIRST
    assert read == []


def test_a_changed_claude_json_is_the_one_account_file_the_next_read_reads_and_shows(
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
    assert set(read) == {changed}


def test_a_changed_settings_file_is_read_and_shown_by_the_next_read(
    slots: list[ClaudeAccount], fake_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    service.overview()
    assert agent_core.install_hooks("claude-code", slots[2].config_dir)
    with account_files_read(fake_home, monkeypatch) as read:
        shown = rows(service.overview())
    assert [row[6] for row in shown] == [False, False, True, False]
    assert slots[2].config_dir / "settings.json" in read
    assert all(path.is_relative_to(slots[2].config_dir) for path in read)


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
    """Slot 2's ``.claude.json`` holds a new login, written an hour ago, and every read of it
    in one overview is refused: this test reads slot 2's email in that overview and the next."""
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
    """Slot 2's ``name`` empty, as a write caught halfway leaves it, an hour old: this test
    counts the reads of the file in each of two overviews."""
    path = slots[1].config_dir / name
    path.write_text("", encoding="utf-8")
    settle(fake_home)
    with account_files_read(fake_home, monkeypatch) as first:
        service.overview()
    with account_files_read(fake_home, monkeypatch) as second:
        service.overview()
    assert second.count(path) == first.count(path) > 0


def test_two_writes_with_one_stat_show_the_second_on_the_next_read(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """This test pins the stat of slot 2's ``.claude.json`` at a first write's, writes the file
    again at the same size, and reads the email in an overview after each write."""
    path = core.claude_json_path(slots[1])
    path.write_text(json.dumps({"oauthAccount": {"emailAddress": "first@example.com"}}))
    pinned = os.stat(path)
    real_stat = os.stat

    def coarse(target: Any, *args: Any, **kwargs: Any) -> os.stat_result:
        if not args and not kwargs and Path(target) == path:
            return pinned
        return real_stat(target, *args, **kwargs)

    monkeypatch.setattr(os, "stat", coarse)
    assert rows(service.overview())[1][2] == "first@example.com"
    path.write_text(json.dumps({"oauthAccount": {"emailAddress": "again@example.com"}}))
    assert rows(service.overview())[1][2] == "again@example.com"


def _settings(slot: ClaudeAccount) -> Any:
    return agent_core._read_settings(slot.config_dir / "settings.json")


def _codex_login(slot: ClaudeAccount) -> Any:
    return codex_accounts._login(slot.config_dir)[0]


def _plugin_record(slot: ClaudeAccount) -> Any:
    return agent_core._plugin_records(slot.config_dir)[0]


@pytest.mark.parametrize(
    ("slot_at", "answer"),
    [(1, _settings), (3, _codex_login), (1, _plugin_record)],
    ids=["a settings file", "a Codex login", "a plugin record"],
)
def test_a_callers_change_to_its_answer_is_not_handed_to_the_next_caller(
    slots: list[ClaudeAccount],
    fake_home: Path,
    slot_at: int,
    answer: Callable[[ClaudeAccount], dict[str, Any]],
) -> None:
    """This test asks each of three readers twice for one file written an hour ago, changes the
    first answer, and reads the second."""
    slot = slots[slot_at]
    installed = slot.config_dir / "plugins" / "installed_plugins.json"
    installed.parent.mkdir(parents=True, exist_ok=True)
    installed.write_text(
        json.dumps({"version": 2, "plugins": {agent_core.CLAUDE_PLUGIN_ID: [{"version": "1"}]}}),
        encoding="utf-8",
    )
    settle(fake_home)
    first = answer(slot)
    before = json.dumps(first, sort_keys=True)
    first["changed by its caller"] = True
    assert json.dumps(answer(slot), sort_keys=True) == before


def test_each_lookup_and_keep_of_a_parse_in_two_overviews_holds_the_lock(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """This test reads, at each lookup and each keep in the kept parses' map over two overviews
    of the ``slots`` fixture, whether the map's lock is held."""
    from aisquare.core import parsed_files

    held: list[bool] = []

    class Noting(dict[Any, Any]):
        def get(self, key: Any, default: Any = None) -> Any:
            held.append(parsed_files._LOCK.locked())
            return super().get(key, default)

        def __setitem__(self, key: Any, value: Any) -> None:
            held.append(parsed_files._LOCK.locked())
            super().__setitem__(key, value)

    monkeypatch.setattr(parsed_files, "_KEPT", Noting())
    service.overview()
    service.overview()
    assert held and all(held)
