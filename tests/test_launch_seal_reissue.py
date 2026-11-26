"""``core.launch_seal.reissue``: every record kept beside the launches written again with a
change applied to what it holds, by the seal code's own writers (#31, H2's re-seal). The home's
move hands it a function that moves paths under the old home; here a plain one stands in, so
each kind of file and each way one can fail is pinned on its own.
"""

from __future__ import annotations

import os
import sys
from datetime import UTC, datetime
from pathlib import Path

import pytest

from aisquare.core import launch_seal
from aisquare.models import LaunchSpec

NOW = datetime(2026, 11, 26, 9, 0, tzinfo=UTC)


def _swap(old: str, new: str) -> object:
    def move(value: object) -> object:
        if isinstance(value, str):
            return value.replace(old, new)
        if isinstance(value, list):
            return [move(item) for item in value]
        if isinstance(value, dict):
            return {key: move(item) for key, item in value.items()}
        return value

    return move


def _record(agent_id: str, root: str) -> launch_seal.LaunchRecord:
    return launch_seal.LaunchRecord(
        id=agent_id,
        project_id="prj_a",
        project_root=root,
        role="coder",
        agent="claude-code",
        binary="claude",
        cwd=root,
        spec=LaunchSpec(binary="claude"),
    )


def test_every_kind_of_file_is_written_again_with_the_change(tmp_path: Path) -> None:
    old, new = str(tmp_path / "old"), str(tmp_path / "new")
    assert launch_seal.write(_record("agt-1", f"{old}/app")) is None
    window = launch_seal.WindowPlace(
        tmux_socket="asq", pane_id="%1", socket_path=f"{old}/sock", created_at=NOW
    )
    assert launch_seal.placed("agt-1", window) is None
    assert launch_seal.take_census([_record("agt-legacy", f"{old}/legacy")]) is None
    place = launch_seal.SessionPlace(
        tmux_socket="asq", socket_path=f"{old}/sock", session_id="$1", name="s", created_at=NOW
    )
    assert launch_seal.add_project_session("prj_a", place) is None

    failed, skipped = launch_seal.reissue(_swap(old, new))  # type: ignore[arg-type]

    assert (failed, skipped) == ([], [])
    record = launch_seal.read("agt-1")
    assert isinstance(record, launch_seal.LaunchRecord)
    assert record.project_root == f"{new}/app"
    assert record.window is not None and record.window.socket_path == f"{new}/sock"
    legacy = launch_seal.read("agt-legacy")
    assert isinstance(legacy, launch_seal.LaunchRecord) and legacy.project_root == f"{new}/legacy"
    assert [s.socket_path for s in launch_seal.project_sessions("prj_a")] == [f"{new}/sock"]


def test_a_file_the_change_leaves_as_it_was_is_not_written(tmp_path: Path) -> None:
    assert launch_seal.write(_record("agt-1", str(tmp_path / "elsewhere"))) is None
    path = launch_seal._path("agt-1")
    before = path.stat().st_mtime_ns
    os.utime(path, ns=(before - 10**9, before - 10**9))

    assert launch_seal.reissue(_swap("/nowhere", "/still-nowhere")) == ([], [])  # type: ignore[arg-type]

    assert path.stat().st_mtime_ns == before - 10**9


def test_a_record_that_cannot_be_read_or_names_another_row_is_left_and_said(
    tmp_path: Path,
) -> None:
    assert launch_seal.write(_record("agt-1", str(tmp_path))) is None
    # Another row's record under this row's name, as a sandbox could leave it.
    stolen = launch_seal._path("agt-2")
    stolen.write_text(launch_seal._path("agt-1").read_text(encoding="utf-8"), encoding="utf-8")
    broken = launch_seal._path("agt-3")
    broken.write_text("{not json", encoding="utf-8")
    broken_window = launch_seal._window_path("agt-4")
    broken_window.write_text("[]", encoding="utf-8")

    failed, skipped = launch_seal.reissue(_swap(str(tmp_path), "/moved"))  # type: ignore[arg-type]

    assert failed == []
    said = " ".join(skipped)
    assert f"{stolen.name}: it names another row" in said
    assert f"{broken.name}: ValidationError" in said
    assert f"{broken_window.name}: ValidationError" in said
    assert broken.read_text(encoding="utf-8") == "{not json", "left as it was"


@pytest.mark.skipif(sys.platform == "win32", reason="a folder's write bit is POSIX's")
def test_a_file_that_cannot_be_written_is_said_failed(tmp_path: Path) -> None:
    if os.geteuid() == 0:
        pytest.skip("root writes anywhere")
    assert launch_seal.write(_record("agt-1", str(tmp_path / "old"))) is None
    window = launch_seal.WindowPlace(
        tmux_socket="asq", pane_id="%1", socket_path=str(tmp_path / "old"), created_at=NOW
    )
    assert launch_seal.placed("agt-1", window) is None
    folder = launch_seal.seals_dir()
    folder.chmod(0o500)
    try:
        failed, skipped = launch_seal.reissue(_swap(str(tmp_path / "old"), "/moved"))  # type: ignore[arg-type]
    finally:
        folder.chmod(0o700)

    assert skipped == [] and len(failed) == 2, failed
    assert all("could not be" in why for why in failed)


def test_a_seals_folder_that_cannot_be_listed_is_said_failed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(launch_seal, "seals_dir", lambda: tmp_path / "not-there")

    failed, skipped = launch_seal.reissue(_swap("a", "b"))  # type: ignore[arg-type]

    assert skipped == [] and len(failed) == 1 and "could not be listed" in failed[0]
