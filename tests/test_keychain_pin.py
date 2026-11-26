"""A managed slot's Keychain item read, copied and deleted for the home's move, and the pin that
keeps a login too long to copy under its old name (#31, H2): each way they answer, on their own.
``security`` is the FAKE of tests/test_home_migration.py; nothing reads the login Keychain.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest

from aisquare.core import claude_accounts as core
from tests.test_home_migration import FakeSecurity

POSIX_ONLY = pytest.mark.skipif(
    sys.platform == "win32", reason="the fake `security` is a POSIX script, as the real one is"
)


@pytest.fixture
def fake(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> FakeSecurity:
    security = FakeSecurity(tmp_path / "keychain")
    monkeypatch.setattr(core, "KEYCHAIN_TOOL", str(security.tool))
    return security


def _slot(tmp_path: Path, marker: str | None = '{"slot": 2}') -> Any:
    account = core.ClaudeAccount(slot=2, config_dir=tmp_path / "slot", managed=True)
    account.config_dir.mkdir()
    if marker is not None:
        (account.config_dir / core.MARKER).write_text(marker, encoding="utf-8")
    return account


# --- the pin ---------------------------------------------------------------------------------


def test_a_pin_is_recorded_read_and_taken_away_keeping_the_marker(tmp_path: Path) -> None:
    account = _slot(tmp_path)
    assert core.keychain_pin(account) is None

    core.pin_keychain_dir(account, "/old/claude-accounts/2")

    assert core.keychain_pin(account) == "/old/claude-accounts/2"
    assert core.keychain_pin(account) == "/old/claude-accounts/2", "read again from the memo"
    marker = json.loads((account.config_dir / core.MARKER).read_text(encoding="utf-8"))
    assert marker == {"slot": 2, core.KEYCHAIN_PIN_KEY: "/old/claude-accounts/2"}
    core.pin_keychain_dir(account, None)
    assert core.keychain_pin(account) is None
    assert json.loads((account.config_dir / core.MARKER).read_text(encoding="utf-8")) == {"slot": 2}


@pytest.mark.parametrize("marker", ["{not json", "[1, 2]", '{"keychain_dir": 7}', None])
def test_a_marker_that_holds_no_pin_reads_as_none(tmp_path: Path, marker: str | None) -> None:
    assert core.keychain_pin(_slot(tmp_path, marker)) is None


def test_the_plain_slot_is_never_pinned_and_unpinning_a_slot_with_no_marker_makes_none(
    tmp_path: Path,
) -> None:
    assert core.keychain_pin(core.default_account()) is None
    account = _slot(tmp_path, marker=None)

    core.pin_keychain_dir(account, None)

    assert not (account.config_dir / core.MARKER).exists()


def test_pinning_over_an_unreadable_marker_writes_the_pin(tmp_path: Path) -> None:
    account = _slot(tmp_path, "[not an object]")

    core.pin_keychain_dir(account, "/old/2")

    assert core.keychain_pin(account) == "/old/2"


def test_a_pinned_slot_s_plaintext_fallback_is_in_its_pinned_folder(tmp_path: Path) -> None:
    account = _slot(tmp_path)
    core.pin_keychain_dir(account, str(tmp_path / "old-2"))

    assert core.plaintext_credentials_path(account, {}) == tmp_path / "old-2" / core.CREDENTIALS
    users = {core.SECURE_STORAGE_VAR: str(tmp_path / "mine")}
    assert core.plaintext_credentials_path(account, users) == tmp_path / "mine" / core.CREDENTIALS


def test_only_the_user_s_own_secure_storage_variable_is_theirs() -> None:
    assert core._users_secure_storage({}) is None
    assert core._users_secure_storage({core.SECURE_STORAGE_VAR: "/a"}) == "/a"
    ours = {core.SECURE_STORAGE_VAR: "/a", core.KEYCHAIN_PIN_VAR: "/a"}
    assert core._users_secure_storage(ours) is None
    changed = {core.SECURE_STORAGE_VAR: "/b", core.KEYCHAIN_PIN_VAR: "/a"}
    assert core._users_secure_storage(changed) == "/b", "set again since, by the user"


# --- an item read, copied and deleted --------------------------------------------------------


@POSIX_ONLY
def test_an_item_reads_as_its_bytes_or_none_or_why_not(fake: FakeSecurity) -> None:
    fake.put("svc-text", "4142")  # hex-looking text that prints as it stands: its text
    fake.put("svc-locked", "", exit=36, stderr="User interaction is not allowed.\n")

    assert core.keychain_item_data("svc-text", "tester").data == b"4142"
    assert core.keychain_item_data("svc-none", "tester").data is None
    locked = core.keychain_item_data("svc-locked", "tester")
    assert locked.data is None and locked.unreadable == core.KEYCHAIN_LOCKED


@POSIX_ONLY
def test_an_item_is_refused_a_name_it_cannot_quote_or_a_line_it_cannot_take(
    fake: FakeSecurity,
) -> None:
    assert "cannot be given" in (core.add_keychain_item('svc"', "tester", b"x") or "")
    assert "cannot be given" in (core.add_keychain_item("svc", "a b\n", b"x") or "")
    assert not core.keychain_copy_fits("svc", "tester", b"x" * 3000)
    assert core.keychain_copy_fits("svc", "tester", b"x" * 100)
    assert fake.calls() == [], "nothing was asked of security"


@POSIX_ONLY
def test_a_delete_of_an_item_that_is_gone_is_done_and_a_refused_one_says_why(
    fake: FakeSecurity,
) -> None:
    fake.put("svc", "{}")
    assert core.delete_keychain_item("svc", "tester") is None
    assert core.delete_keychain_item("svc", "tester") is None, "gone already: done"


def test_security_that_cannot_run_or_answer_is_said(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(core, "KEYCHAIN_TOOL", str(tmp_path / "no-security"))
    assert "no-security" in (core.keychain_item_data("svc", "tester").unreadable or "")
    assert "no-security" in (core.delete_keychain_item("svc", "tester") or "")

    def slow(*args: object, **kwargs: object) -> Any:
        raise subprocess.TimeoutExpired(cmd="security", timeout=5)

    monkeypatch.setattr(subprocess, "run", slow)
    assert "no answer" in (core.add_keychain_item("svc", "tester", b"{}") or "")


@POSIX_ONLY
def test_a_failed_write_or_delete_says_the_keychain_s_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    tool = tmp_path / "security"
    tool.write_text("#!/bin/sh\necho 'User interaction is not allowed.' >&2\nexit 36\n")
    tool.chmod(0o755)
    monkeypatch.setattr(core, "KEYCHAIN_TOOL", str(tool))

    assert core.add_keychain_item("svc", "tester", b"{}") == core.KEYCHAIN_LOCKED
    assert core.delete_keychain_item("svc", "tester") == core.KEYCHAIN_LOCKED
