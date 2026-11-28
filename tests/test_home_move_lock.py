"""The home-move lock (#62) on every system, Windows' legs included: the move's own suite
(``tests/test_home_migration.py``) runs on POSIX alone. Taken, the lock refuses a second
holder; let go, it leaves no file, in Windows' order there (let go, closed, then removed, as
Windows removes no file open anywhere) and in POSIX's elsewhere."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from aisquare.services import home_migration


def test_the_move_lock_refuses_a_second_holder_and_leaves_no_file(tmp_path: Path) -> None:
    """A home's lock, beside it: a second holder is refused while the first holds it, the
    first's file left as it is; let go, no file is left, and the lock can be taken again."""
    home = tmp_path / "home"
    home.mkdir()
    lock = home_migration.move_lock_path(str(home), named=True)
    assert lock.parent == Path(os.path.realpath(tmp_path))

    with home_migration._one_at_a_time(str(home), named=True):
        assert lock.is_file()
        with (
            pytest.raises(home_migration.MigrationRefused, match=home_migration.ANOTHER_MOVE),
            home_migration._one_at_a_time(str(home), named=True),
        ):
            pass
        assert lock.is_file(), "the refused one leaves the holder's file"
    assert not lock.exists()

    with home_migration._one_at_a_time(str(home), named=True):
        assert lock.is_file()
    assert sorted(path.name for path in tmp_path.iterdir()) == ["home"]
