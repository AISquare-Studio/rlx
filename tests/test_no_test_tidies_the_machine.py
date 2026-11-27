"""No test reaches this machine through ``rlx tidy`` (#58, tester #3507).

A guard once ran a real ``rlx tidy`` under the suite, and tidy's stale-lock rule reads the
SYSTEM temp directory (macOS's per-user one, found without ``TMPDIR``) and the process table:
a free ``rlx-home-migration-<hex>.lock`` older than a day there would have been deleted. The
suite-wide fixture ``no_tidy_of_the_machine`` (``tests/conftest.py``) points both at the test's
own, as ``#52``'s guard does for the Keychain. These tests prove it holds for every test.
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path

from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.services import tidy


def test_tidy_reads_no_temp_dir_and_no_process_of_the_machines(tmp_path: Path) -> None:
    assert tidy._system_temp_dirs() == [tmp_path / "system-temp"]
    assert tidy._command_lines() == []


def test_a_real_rlx_tidy_under_the_suite_frees_a_stale_lock_in_the_sandbox_alone(
    tmp_path: Path,
) -> None:
    """``rlx tidy`` run for real, as ``test_stubs`` runs every command: the only lock it can
    find and free is one in the test's own system temp."""
    # Asked first, so a suite without the fixture fails here, before any real run.
    assert tidy._system_temp_dirs() == [tmp_path / "system-temp"]
    assert tidy._command_lines() == []
    lock = tmp_path / "system-temp" / f"rlx-home-migration-{'a' * 16}.lock"
    lock.parent.mkdir()
    lock.write_bytes(b"")
    day_ago = time.time() - 25 * 3600
    os.utime(lock, (day_ago, day_ago))

    result = CliRunner().invoke(app, ["--json", "tidy"])

    assert result.exit_code == 0, result.output
    freed = json.loads(result.stdout)["freed"]
    assert [Path(item["path"]) for item in freed] == [lock] and not lock.exists()
