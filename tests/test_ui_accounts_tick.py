"""``asq``'s accounts read reuses a slot's identity parse while its ``.claude.json`` is unchanged.

The status line and the Accounts page follow ``claude_accounts.overview``, which ``asq`` reads
after every fleet frame. ``read_identity`` keeps its parse of each ``.claude.json`` under the file's
stat signature (``core.parsed_files``: ``st_mtime_ns``, ``st_ctime_ns``, ``st_size``, ``st_ino``):
a lookup whose stat is unchanged opens nothing, and any other reads and parses (rlx#557, by
Vaibhav; the stat rule is the owner's decision on its takeover, #51). These tests count the
identity reads and parses, read what the read after each kind of change shows, and read what the
cache keeps.

Every file here is old enough to be kept (:func:`settled`) unless a test says otherwise: a file
written or changed less than ``parsed_files.RACY_NS`` before it was read is read again at every
lookup.
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
from types import SimpleNamespace
from typing import Any, cast

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


@pytest.fixture(autouse=True)
def settled(monkeypatch: pytest.MonkeyPatch) -> None:
    """The clock an hour on: every file the fixtures just wrote reads as settled, and is kept."""
    monkeypatch.setattr(parsed_files, "_clock", lambda: time.time_ns() + 3600 * 10**9)


@contextmanager
def identity_io(monkeypatch: pytest.MonkeyPatch) -> Iterator[tuple[list[Path], list[Path]]]:
    """The ``.claude.json`` files opened and read (``parsed_files._read``) and the ones the
    identity parser parsed (its JSON step, ``json_object``) in the block, once each time."""
    read: list[Path] = []
    parsed: list[Path] = []
    read_file, json_object = parsed_files._read, parsed_files.json_object

    def reading(path: Path) -> tuple[parsed_files.Signature, bytes]:
        if path.name == ".claude.json":
            read.append(path)
        return read_file(path)

    def parsing(path: Path, text: str) -> Any:
        parsed.append(path)
        return json_object(path, text)

    with monkeypatch.context() as patched:
        patched.setattr(parsed_files, "_read", reading)
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


def test_ten_warm_overviews_read_no_claude_json_and_parse_none(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """The ``slots`` fixture: three Claude ``.claude.json`` files, each asked for its identity
    three times an overview. Unchanged, none is opened: each lookup is an ``os.stat`` (#51; the
    digest rule read all three files at every lookup, 30 reads)."""
    assert rows(service.overview()) == FIRST
    with identity_io(monkeypatch) as (read, parsed):
        for _ in range(10):
            assert rows(service.overview()) == FIRST
    assert parsed == []
    assert read == []


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


def _settled_at(path: Path, seconds_ago: float) -> int:
    """``path``'s modification time set ``seconds_ago`` back; the new ``st_mtime_ns``."""
    when = time.time_ns() - int(seconds_ago * 10**9)
    os.utime(path, ns=(when, when))
    return when


@pytest.mark.parametrize("key", ["mtime", "size", "inode"])
def test_a_change_in_each_key_of_the_stat_is_read_again(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch, key: str
) -> None:
    """Each of ``st_mtime_ns``, ``st_size`` and ``st_ino`` changed on disk, the other two as they
    were, has the next lookup read and parse the file again, and show what it holds. (On POSIX
    ``st_ctime_ns`` moves with each: the test below moves each key of the signature alone.)"""
    path = core.claude_json_path(slots[1])
    _write_email(path, "first@example.com")
    mtime = _settled_at(path, 3600)
    assert _email(path) == "first@example.com"
    before = os.stat(path)
    if key == "mtime":
        _write_email(path, "again@example.com")  # the same size, in place: the same inode
        _settled_at(path, 1800)
    elif key == "size":
        _write_email(path, "a.longer.one@example.com")  # in place: the same inode
        os.utime(path, ns=(mtime, mtime))
    else:
        beside = path.with_name("replacement.json")
        _write_email(beside, "again@example.com")  # the same size, a new inode
        os.utime(beside, ns=(mtime, mtime))
        os.replace(beside, path)
    after = os.stat(path)
    changed = {
        "mtime": after.st_mtime_ns != before.st_mtime_ns,
        "size": after.st_size != before.st_size,
        "inode": after.st_ino != before.st_ino,
    }
    assert changed == {name: name == key for name in changed}, changed
    with identity_io(monkeypatch) as (read, parsed):
        shown = _email(path)
    assert shown in ("again@example.com", "a.longer.one@example.com")
    assert read == [path] and parsed == [path]


KEYS = ("st_mtime_ns", "st_ctime_ns", "st_size", "st_ino")
"""The stat's fields the signature is made of (``parsed_files._stat_signature``)."""


@pytest.mark.parametrize("key", KEYS)
def test_a_change_in_any_one_key_of_the_signature_alone_is_read_again(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch, key: str
) -> None:
    """Each of the four fields of the stat the signature is made of moved alone, the bytes as
    they were, has the next lookup read and parse the file again. The path is stat'ed through a
    stand-in, a stat whose fields the test sets, made a signature by ``_stat_signature`` itself
    (``parsed_files._stat``, and ``_read``'s): no file system moves ``st_ctime_ns`` alone, or the
    other three without it."""
    path = core.claude_json_path(slots[1])
    _write_email(path, "first@example.com")
    real = os.stat(path)
    fields = {name: getattr(real, name) for name in KEYS}
    stat, read_file = parsed_files._stat, parsed_files._read

    def signature() -> parsed_files.Signature:
        return parsed_files._stat_signature(cast(os.stat_result, SimpleNamespace(**fields)))

    def statting(target: Path) -> parsed_files.Signature:
        return signature() if target == path else stat(target)

    def reading(target: Path) -> tuple[parsed_files.Signature, bytes]:
        read, body = read_file(target)
        return (signature() if target == path else read), body

    monkeypatch.setattr(parsed_files, "_stat", statting)
    monkeypatch.setattr(parsed_files, "_read", reading)
    assert _email(path) == "first@example.com"
    assert _kept(path) != []
    fields[key] += 1
    with identity_io(monkeypatch) as (read, parsed):
        assert _email(path) == "first@example.com"
    assert read == [path] and parsed == [path]


def test_a_file_written_moments_ago_is_read_at_every_lookup_until_it_settles(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """Git's "racily clean" rule: a file read inside its timestamp's granularity of its last write
    could be written again with the same stat, so nothing is kept for it, and each lookup reads.
    Here, a rewrite of the same size given the first one's modification time back, made while
    the file is fresh, shows at once. Its answer is kept once
    :data:`~aisquare.core.parsed_files.RACY_NS` has passed since the later of its modification and
    status-change times, and not a nanosecond before."""
    now = time.time_ns()
    monkeypatch.setattr(parsed_files, "_clock", lambda: now)
    path = core.claude_json_path(slots[1])
    _write_email(path, "first@example.com")
    os.utime(path, ns=(now, now))
    assert _email(path) == "first@example.com"
    assert _kept(path) == []
    _write_email(path, "again@example.com")
    os.utime(path, ns=(now, now))
    assert _email(path) == "again@example.com", "the same mtime and size, read again while fresh"
    written = os.stat(path)
    settles = max(written.st_mtime_ns, written.st_ctime_ns) + parsed_files.RACY_NS
    monkeypatch.setattr(parsed_files, "_clock", lambda: settles - 1)
    assert _email(path) == "again@example.com"
    assert _kept(path) == []
    monkeypatch.setattr(parsed_files, "_clock", lambda: settles)
    assert _email(path) == "again@example.com"
    assert _kept(path) != []
    with identity_io(monkeypatch) as (read, _parsed):
        assert _email(path) == "again@example.com"
    assert read == []


def _a_later_ctime(path: Path, than: int, mtime: int) -> os.stat_result:
    """``path``'s stat once its ``st_ctime_ns`` is past ``than``, its modification time put back
    to ``mtime`` again until it is: a file system that stamps from a coarse clock (ext4's, a tick
    of milliseconds) can give a write the status-change time of the change before it."""
    deadline = time.monotonic() + 5.0
    while (now := os.stat(path)).st_ctime_ns <= than:
        assert time.monotonic() < deadline, "the status-change time never moved"
        time.sleep(0.01)
        os.utime(path, ns=(mtime, mtime))
    return now


@pytest.mark.skipif(sys.platform == "win32", reason="st_ctime_ns is the creation time on Windows")
def test_a_settled_rewrite_that_puts_its_old_mtime_back_is_seen_by_its_ctime(
    slots: list[ClaudeAccount],
) -> None:
    """The blind spot of a signature of ``st_mtime_ns``, ``st_size`` and ``st_ino`` alone, closed
    (review of #51): once the file has settled, a rewrite in place at the same size that puts the
    old modification time back keeps those three, and moves ``st_ctime_ns``, which nothing sets
    back. The next lookup reads it."""
    path = core.claude_json_path(slots[1])
    _write_email(path, "first@example.com")
    mtime = _settled_at(path, 3600)
    assert _email(path) == "first@example.com"
    before = os.stat(path)
    _write_email(path, "again@example.com")
    os.utime(path, ns=(mtime, mtime))
    after = _a_later_ctime(path, before.st_ctime_ns, mtime)
    held = (after.st_mtime_ns, after.st_size, after.st_ino)
    assert held == (before.st_mtime_ns, before.st_size, before.st_ino), "the premise"
    assert _email(path) == "again@example.com"


@pytest.mark.skipif(sys.platform != "win32", reason="elsewhere st_ctime_ns sees it (test above)")
def test_on_windows_a_settled_rewrite_holding_every_key_of_the_stat_is_not_seen(
    slots: list[ClaudeAccount],
) -> None:
    """The stat rule's one blind spot left, pinned so a change to it is deliberate: on Windows
    ``st_ctime_ns`` is the creation time, which a rewrite in place leaves, so once the file has
    settled, a rewrite at the same size that puts the old modification time back keeps every key
    of the signature, and the kept answer stands until something else of it changes. Claude Code
    never writes so."""
    path = core.claude_json_path(slots[1])
    _write_email(path, "first@example.com")
    mtime = _settled_at(path, 3600)
    assert _email(path) == "first@example.com"
    before = parsed_files._stat(path)
    _write_email(path, "again@example.com")
    os.utime(path, ns=(mtime, mtime))
    assert parsed_files._stat(path) == before, "the premise: every key held"
    assert _email(path) == "first@example.com"
    _settled_at(path, 1800)
    assert _email(path) == "again@example.com", "any key changed: read again"


def test_a_parse_that_outlasts_the_racy_window_keeps_nothing_of_a_file_fresh_when_read(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """The settled check reads the clock as the read begins, not once the parse is done (review
    of #51): a file read while fresh could be written again within its timestamps' granularity
    after the read, so its answer is not kept however long the parse took."""
    path = core.claude_json_path(slots[1])
    _write_email(path, "first@example.com")
    written = os.stat(path)
    clock = [max(written.st_mtime_ns, written.st_ctime_ns)]
    monkeypatch.setattr(parsed_files, "_clock", lambda: clock[0])
    json_object = parsed_files.json_object

    def slow(target: Path, text: str) -> Any:
        clock[0] += 3600 * 10**9  # an hour passes while it parses
        return json_object(target, text)

    monkeypatch.setattr(core, "json_object", slow)
    assert _email(path) == "first@example.com"
    assert _kept(path) == []


@pytest.mark.skipif(sys.platform == "win32", reason="a symlink needs a privilege on Windows")
def test_a_link_moved_to_b_while_read_and_back_to_a_gives_b_then_a(
    slots: list[ClaudeAccount], tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Slot 2's ``.claude.json`` is a link to file A. Its first read moves the link to B, reads B,
    and moves the link back to A before the lookup returns. This test reads that lookup's email,
    then the next lookup's: B's answer is kept under neither stat (the bytes' ``fstat`` is B's,
    the path's A's)."""
    path = core.claude_json_path(slots[1])
    a, b = tmp_path / "a.json", tmp_path / "b.json"
    _write_email(a, "a@example.com")
    _write_email(b, "b@example.com")
    path.unlink()
    path.symlink_to(a)
    read_file = parsed_files._read
    moved: list[bool] = []

    def moving(target: Path) -> tuple[parsed_files.Signature, bytes]:
        if target != path or moved:
            return read_file(target)
        moved.append(True)
        target.unlink()
        target.symlink_to(b)
        read = read_file(target)
        target.unlink()
        target.symlink_to(a)
        return read

    with monkeypatch.context() as patched:
        patched.setattr(parsed_files, "_read", moving)
        assert _email(path) == "b@example.com"
    assert _kept(path) == []
    assert _email(path) == "a@example.com"


def test_a_failed_read_of_a_changed_warm_file_gives_none_then_the_next_read_parses_it(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """A warm file whose stat is unchanged is not opened, so cannot fail to read: once its stat
    changes, a read that fails gives none and drops the kept answer."""
    path = core.claude_json_path(slots[1])
    assert _email(path) == "slot1@example.com"
    _settled_at(path, 1800)
    read_file = parsed_files._read

    def refused(target: Path) -> tuple[parsed_files.Signature, bytes]:
        if target == path:
            raise PermissionError(13, "Permission denied", str(target))
        return read_file(target)

    with monkeypatch.context() as patched:
        patched.setattr(parsed_files, "_read", refused)
        assert _email(path) is None
    assert _kept(path) == []
    with identity_io(monkeypatch) as (_read, parsed):
        assert _email(path) == "slot1@example.com"
    assert parsed == [path]


def test_an_older_reader_publishing_last_gives_each_reader_its_own_identity(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """Reader 1 reads A; before it parses, the file becomes B and reader 2 reads, parses and keeps
    B; then reader 1 parses A, finds the file's stat is no longer that of the bytes it read, and
    keeps nothing. This test reads both readers' emails, then the email and the parses of the
    next read of B.

    A and B are given modification times half an hour apart, as a later write has once the
    clock has ticked. Written moments apart at the same size, they can share every key of the
    stat: Windows stamps a write from a clock of about 15.6 ms and keeps the creation time as
    ``st_ctime_ns``, and ext4 stamps both from a tick of milliseconds (CI 46056955569,
    windows-py3.12 shard 6/10, kept A under B's stat). That case is the racy guard's, which
    :func:`settled` puts out of play here, and
    :func:`test_a_file_written_moments_ago_is_read_at_every_lookup_until_it_settles` covers."""
    path = core.claude_json_path(slots[1])
    _write_email(path, "a@example.com")
    _settled_at(path, 3600)
    read_file = parsed_files._read
    second: list[str | None] = []

    def overtaken(target: Path) -> tuple[parsed_files.Signature, bytes]:
        read = read_file(target)
        if target == path and not second:
            second.append(None)
            _write_email(path, "b@example.com")
            _settled_at(path, 1800)
            second[0] = _email(path)
        return read

    with monkeypatch.context() as patched:
        patched.setattr(parsed_files, "_read", overtaken)
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


def test_lookups_keeps_drops_evictions_and_both_sweep_steps_hold_the_lock(
    slots: list[ClaudeAccount], monkeypatch: pytest.MonkeyPatch
) -> None:
    """This test reads whether the map's lock is held, by kind, at these calls on the kept
    parses: ``get`` and ``move_to_end`` (a lookup), an assignment (a keep), ``pop`` from
    ``_forget`` (a failed read's drop), ``popitem`` (an eviction), iteration (the sweep's
    selection) and ``pop`` from ``forget_under`` (the sweep's removal). Each call is read on its
    own. It drives two overviews of the ``slots`` fixture, the second once slot 2 is removed
    with its parse still kept and slot 3's ``.claude.json`` is gone, then one read of slot 3's
    file written again, with :data:`~aisquare.core.parsed_files.MAX_KEPT` patched to one."""
    held: dict[str, list[bool]] = {
        "lookup": [],
        "keep": [],
        "drop": [],
        "eviction": [],
        "sweep selection": [],
        "sweep removal": [],
    }
    #: Who removes a parse by ``pop``, by the function that calls it.
    removers = {"_forget": "drop", "forget_under": "sweep removal"}

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
            caller = sys._getframe(1).f_code.co_name
            kind = removers.get(caller, f"pop in {caller}")
            held.setdefault(kind, []).append(parsed_files._LOCK.locked())
            return super().pop(key, default)

        def popitem(self, last: bool = True) -> tuple[Any, Any]:
            held["eviction"].append(parsed_files._LOCK.locked())
            return super().popitem(last)

        def __iter__(self) -> Iterator[Any]:
            held["sweep selection"].append(parsed_files._LOCK.locked())
            return super().__iter__()

    monkeypatch.setattr(parsed_files, "_KEPT", Noting())
    removed, gone = core.claude_json_path(slots[1]), core.claude_json_path(slots[2])
    service.overview()
    assert _kept(removed) != []
    core.remove_account(slots[1])
    gone.unlink()
    service.overview()
    assert _kept(removed) == [] and _kept(gone) == []
    monkeypatch.setattr(parsed_files, "MAX_KEPT", 1)
    _write_email(gone, "back@example.com")
    assert _email(gone) == "back@example.com"
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
