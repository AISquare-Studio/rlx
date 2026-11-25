"""``core.registry``: the folder-of-records machinery the home's process registries share
(0.10.3 §4.1a).

``core.daemons`` is the first registry on it (``tests/test_daemons.py`` drives its rules
through it); ``core.instances`` and ``core.launchers`` move onto it in 6.2. These pin what is
generic: a record type of the caller's own, the names a reader leaves alone, a lock file a
registry may not keep, and #574's Windows rule, that a delete refused because a reader has the
file open is tried again for a second.
"""

from __future__ import annotations

import errno
import os
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pytest

from aisquare.core import daemons, locking, paths, registry

THINGS = registry.Registry("things", stale_seconds=60.0)


@dataclass(frozen=True)
class Thing:
    id: str
    pid: int


def _thing(thing_id: str, data: object) -> Thing | None:
    if not isinstance(data, dict) or type(data.get("pid")) is not int:
        return None
    return Thing(thing_id, data["pid"])


def _age(path: Path, seconds: float) -> None:
    then = time.time() - seconds
    os.utime(path, (then, then))


def test_a_registry_takes_a_record_type_of_its_own() -> None:
    fd = THINGS.take("0123456789ab")
    try:
        THINGS.write("0123456789ab", {"pid": 7})
        found = THINGS.roll(_thing, lambda thing, held: bool(held))
        assert found.live == [Thing("0123456789ab", 7)]
        assert found.held == frozenset({"0123456789ab"})
    finally:
        THINGS.leave("0123456789ab", fd)
    assert sorted(os.listdir(THINGS.folder)) == []
    assert THINGS.folder == paths.aisquare_home() / "things"


def test_a_lock_file_a_registry_may_not_keep_can_not_be_told() -> None:
    THINGS.private_folder()
    THINGS.write("0123456789ab", {"pid": os.getpid()})
    assert THINGS.lock_held("0123456789ab") is False
    assert THINGS.lock_held("0123456789ab", missing=None) is None
    seen: list[bool | None] = []

    def live(thing: Thing, held: bool | None) -> bool:
        seen.append(held)
        return True

    found = THINGS.roll(_thing, live, missing=None)
    assert seen == [None] and [t.id for t in found.live] == ["0123456789ab"]


def test_a_reserved_name_is_kept_only_while_it_is_reserved() -> None:
    # An old file of a record that is gone: pruned, unless the registry reserves its name.
    folder = THINGS.private_folder()
    sock = folder / "0123456789ab.sock"
    sock.touch()
    _age(sock, 3600)
    THINGS.roll(_thing, lambda thing, held: True, reserved=frozenset({sock.name}))
    assert sock.exists(), "reserved: kept"
    THINGS.roll(_thing, lambda thing, held: True)
    assert not sock.exists(), "not reserved: a gone record's leftover, pruned"


def test_a_record_the_registry_keeps_is_neither_listed_nor_pruned() -> None:
    THINGS.private_folder()
    THINGS.write("0123456789ab", {"pid": 7})
    (THINGS.folder / "0123456789ab.lock").touch()
    found = THINGS.roll(_thing, lambda thing, held: False, keep=lambda thing, held: True)
    assert found.live == [] and found.pruned == []
    assert sorted(os.listdir(THINGS.folder)) == ["0123456789ab.json", "0123456789ab.lock"]
    found = THINGS.roll(_thing, lambda thing, held: False, keep=lambda thing, held: False)
    assert found.pruned == ["0123456789ab"]
    assert os.listdir(THINGS.folder) == [registry.GATE_LOCK], "pruned under the gate"


def test_keep_lock_keeps_the_descriptor_while_the_file_is_the_one_it_holds() -> None:
    fd = THINGS.take("0123456789ab")
    try:
        assert THINGS.keep_lock("0123456789ab", fd) == fd
    finally:
        THINGS.leave("0123456789ab", fd)


@pytest.mark.skipif(sys.platform == "win32", reason="Windows deletes no file a process holds")
def test_keep_lock_takes_a_deleted_or_replaced_lock_file_again() -> None:
    fd = THINGS.take("0123456789ab")
    lock = THINGS.lock("0123456789ab")
    lock.unlink()
    fd = THINGS.keep_lock("0123456789ab", fd)
    assert THINGS.lock_held("0123456789ab") is True, "deleted: created and locked again"
    lock.unlink()
    lock.touch()  # another file of that name, which nobody holds
    renewed = THINGS.keep_lock("0123456789ab", fd)
    try:
        assert THINGS.lock_held("0123456789ab") is True, "replaced: taken again"
        assert os.fstat(renewed).st_ino == lock.stat().st_ino
    finally:
        THINGS.leave("0123456789ab", renewed)


@pytest.mark.skipif(sys.platform == "win32", reason="Windows deletes no file a process holds")
def test_keep_lock_says_so_when_another_holds_the_new_file_and_keeps_its_own() -> None:
    fd = THINGS.take("0123456789ab")
    lock = THINGS.lock("0123456789ab")
    lock.unlink()
    theirs = locking.open_lock_file(lock, mode=0o600)  # another process took the name
    locking.acquire(theirs, wait_s=0)
    try:
        with pytest.raises(OSError, match="held by another process"):
            THINGS.keep_lock("0123456789ab", fd, wait_s=0.05)
        os.fstat(fd)  # its own descriptor, left as it was: still open
    finally:
        os.close(theirs)
        THINGS.leave("0123456789ab", fd)


def test_keep_lock_takes_the_lock_again_when_its_own_descriptor_cannot_be_stat() -> None:
    fd = THINGS.take("0123456789ab")
    real_fstat = os.fstat

    def fstat(target: int) -> os.stat_result:
        if target == fd:
            raise OSError(errno.ESTALE, "Stale NFS file handle")
        return real_fstat(target)

    if sys.platform != "win32":  # gone as on NFS (Windows deletes no file a process holds)
        THINGS.lock("0123456789ab").unlink()
    with pytest.MonkeyPatch.context() as stale:  # scoped: never undoes the test's home
        stale.setattr(os, "fstat", fstat)
        if sys.platform == "win32":
            # Still there and still its own: the new handle's lock is refused, the old kept.
            with pytest.raises(OSError):
                THINGS.keep_lock("0123456789ab", fd, wait_s=0.05)
            renewed = fd
        else:
            renewed = THINGS.keep_lock("0123456789ab", fd)
            assert renewed != fd, "not taken for the file it holds"
    try:
        assert THINGS.lock_held("0123456789ab") is True
    finally:
        THINGS.leave("0123456789ab", renewed)


def test_an_unreadable_record_whose_age_cannot_be_read_is_reported_never_dropped() -> None:
    THINGS.private_folder()
    THINGS.write("0123456789ab", {"pid": 7})
    record = THINGS.record("0123456789ab")
    real_read, real_stat = Path.read_text, Path.stat

    def read_text(self: Path, *args: Any, **kwargs: Any) -> str:
        if self == record:
            raise OSError(errno.ESTALE, "stale file handle")
        return real_read(self, *args, **kwargs)

    def stat(self: Path, *args: Any, **kwargs: Any) -> os.stat_result:
        if self == record:
            raise OSError(errno.EIO, "input/output error")
        return real_stat(self, *args, **kwargs)

    with pytest.MonkeyPatch.context() as failing:  # scoped: never undoes the test's home
        failing.setattr(Path, "read_text", read_text)
        failing.setattr(Path, "stat", stat)
        found = THINGS.roll(_thing, lambda thing, held: True)
    assert found.live == [] and found.pruned == []
    (unreadable,) = found.unreadable
    assert (unreadable.id, unreadable.written_at, unreadable.held) == ("0123456789ab", None, False)
    assert record.exists()


def test_a_record_is_replaced_while_a_reader_holds_it_open() -> None:
    """On Windows a rename over a file another handle holds without sharing delete is refused
    (WinError 5 or 32) until it closes: the writer tries again for about a second
    (``paths.despite_windows_contention``). Elsewhere the rename simply succeeds."""
    THINGS.private_folder()
    THINGS.write("0123456789ab", {"pid": 7})
    reading = THINGS.record("0123456789ab").open("rb")
    letting_go = threading.Timer(0.3, reading.close)
    letting_go.start()
    try:
        THINGS.write("0123456789ab", {"pid": 8})
    finally:
        letting_go.join()
        reading.close()
    found = THINGS.roll(_thing, lambda thing, held: True, prune=False)
    assert found.live == [Thing("0123456789ab", 8)]


def test_the_gate_is_one_holder_at_a_time_and_never_pruned() -> None:
    with THINGS.gate(), pytest.raises(TimeoutError), THINGS.gate(wait_s=0.05):
        pass
    gate = THINGS.folder / registry.GATE_LOCK
    _age(gate, 3600)
    THINGS.roll(_thing, lambda thing, held: True)
    assert gate.exists()
    with THINGS.gate(wait_s=0):  # let go: taken at once
        pass


def test_a_second_writer_of_one_id_is_refused_and_keeps_nothing_open() -> None:
    fd = THINGS.take("0123456789ab")
    try:
        with pytest.raises(OSError, match="held by another process"):
            THINGS.take("0123456789ab", wait_s=0.05)
    finally:
        THINGS.leave("0123456789ab", fd)


def test_an_id_from_anywhere_but_the_pattern_builds_no_path() -> None:
    for bad in ("../../etc/passwd", "0123456789AB", "0123456789a", "", "0123456789ab.json"):
        assert not registry.valid_id(bad)
        with pytest.raises(ValueError):
            THINGS.record(bad)
    assert registry.valid_id(registry.new_id())


# --- #574's Windows rule: a refused delete is tried again for a second ------------------------


def test_a_delete_refused_a_moment_is_tried_again(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    target = tmp_path / "0123456789ab.json"
    target.touch()
    real = Path.unlink
    refusals = [PermissionError(32, "in use by another process")] * 3

    def unlink(self: Path, missing_ok: bool = False) -> None:
        if self == target and refusals:
            raise refusals.pop()
        real(self, missing_ok=missing_ok)

    monkeypatch.setattr(Path, "unlink", unlink)
    assert registry.unlink(target) is True
    assert not target.exists() and refusals == []


def test_a_delete_refused_past_the_retry_is_given_up_without_raising(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    target = tmp_path / "0123456789ab.json"
    target.touch()

    def unlink(self: Path, missing_ok: bool = False) -> None:
        raise PermissionError(32, "in use by another process")

    monkeypatch.setattr(registry, "DELETE_RETRY_SECONDS", 0.2)
    with pytest.MonkeyPatch.context() as refusing:  # scoped: never undoes the test's home
        refusing.setattr(Path, "unlink", unlink)
        started = time.monotonic()
        assert registry.unlink(target) is False
        assert 0.2 <= time.monotonic() - started < 5
    assert target.exists()


def test_any_other_delete_error_is_not_retried(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls: list[Path] = []

    def unlink(self: Path, missing_ok: bool = False) -> None:
        calls.append(self)
        raise IsADirectoryError(21, "is a directory")

    monkeypatch.setattr(Path, "unlink", unlink)
    assert registry.unlink(tmp_path / "x") is False and len(calls) == 1


def test_a_daemon_leaves_the_registry_while_a_reader_holds_its_record_open() -> None:
    """On Windows a file another handle has open cannot be deleted (WinError 32): the reader
    here keeps the record open for a moment as a deregistration deletes it, and the delete is
    tried again until it lets go. Elsewhere the delete simply succeeds."""
    registration = daemons.register("serve", argv=["rlx", "serve"], restartable=True)
    record = daemons.REGISTRY.record(registration.daemon.id)
    reading = record.open("rb")
    letting_go = threading.Timer(0.3, reading.close)
    letting_go.start()
    try:
        registration.deregister()
    finally:
        letting_go.join()
        reading.close()
    left = os.listdir(daemons.registry_dir())
    assert left == [registry.GATE_LOCK], "the record and its lock both left; the gate stays"


def test_a_leftover_whose_lock_a_reader_holds_is_not_taken_for_a_gone_one() -> None:
    folder = THINGS.private_folder()
    lock = folder / "0123456789ab.lock"
    fd = locking.open_lock_file(lock, mode=0o600)
    locking.acquire(fd, wait_s=0)
    try:
        _age(lock, 3600)
        THINGS.roll(_thing, lambda thing, held: True)
        assert lock.exists()
    finally:
        os.close(fd)
    THINGS.roll(_thing, lambda thing, held: True)
    assert not lock.exists()
