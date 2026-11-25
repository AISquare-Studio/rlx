"""``core.registry``: the folder-of-records machinery the home's process registries share
(0.10.3 §4.1a).

``core.daemons`` is the first registry on it (``tests/test_daemons.py`` drives its rules
through it); ``core.instances`` and ``core.launchers`` move onto it in 6.2. These pin what is
generic: a record type of the caller's own, the names a reader leaves alone, a lock file a
registry may not keep, and #574's Windows rule, that a delete refused because a reader has the
file open is tried again for a second.
"""

from __future__ import annotations

import os
import threading
import time
from dataclasses import dataclass
from pathlib import Path

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


def test_a_reserved_name_is_never_pruned() -> None:
    folder = THINGS.private_folder()
    for name in (".gate.lock", "0123456789ab.lock"):
        (folder / name).touch()
        _age(folder / name, 3600)
    THINGS.roll(_thing, lambda thing, held: True, reserved=frozenset({".gate.lock"}))
    assert sorted(os.listdir(folder)) == [".gate.lock"]


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
    monkeypatch.setattr(Path, "unlink", unlink)
    started = time.monotonic()
    assert registry.unlink(target) is False
    assert 0.2 <= time.monotonic() - started < 5
    monkeypatch.undo()
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
    assert os.listdir(daemons.registry_dir()) == [], "the record and its lock both left"


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
