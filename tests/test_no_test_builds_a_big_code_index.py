"""No test builds a big code index (#61).

The sweeps that run every command ran ``code index`` and the first ``code`` query in the
checkout they stand in, and built a full index of this repository per test: a 102 MB
``code/index.db``, 112 MB of temp each, 41 GB across one Mac's kept pytest basetemps.
``tests/conftest.py`` now gives such a build no files (``no_files_of_this_checkout``), and
fails any test whose index grows past ``CODE_INDEX_BOUND_MB`` (``small_code_index``).
These tests hold both.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from aisquare.core import code_index
from tests import conftest as suite

THIS_CHECKOUT = Path(__file__).resolve().parents[1]


def _fixture_repo(root: Path) -> Path:
    """A repo of two small Python files: what a test that wants an index builds one of."""
    root.mkdir(parents=True)
    (root / "a.py").write_text("def helper(value):\n    return value + 1\n", encoding="utf-8")
    (root / "b.py").write_text("from a import helper\n\nhelper(1)\n", encoding="utf-8")
    return root


def test_a_build_of_this_checkout_runs_and_indexes_none_of_its_files() -> None:
    """The build a sweep's ``code index`` asks for still runs to its report, over no files."""
    assert (THIS_CHECKOUT / "src" / "aisquare").is_dir(), "the premise: this is the checkout"
    report = code_index.build("prj_this_checkout", THIS_CHECKOUT, full=True)
    status = code_index.status("prj_this_checkout")

    assert (report.files, status.exists, status.files) == (0, True, 0)
    assert suite.index_megabytes("prj_this_checkout") < 1.0


def test_a_fixture_repo_under_tmp_path_is_indexed_as_written(tmp_path: Path) -> None:
    root = _fixture_repo(tmp_path / "repo")
    report = code_index.build("prj_fixture", root, full=True)

    assert report.files == 2
    assert [symbol.path for symbol in code_index.find("prj_fixture", "helper")] == ["a.py"]


def test_an_index_past_the_bound_fails_the_test_that_built_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The guard fires: a bound below a two-file index's size fails its build, naming the
    root, as a 102 MB index of the checkout would have."""
    root = _fixture_repo(tmp_path / "repo")
    monkeypatch.setattr(suite, "CODE_INDEX_BOUND_MB", 0.0)

    with pytest.raises(pytest.fail.Exception, match=r"code index of .*repo"):
        code_index.build("prj_fixture", root, full=True)
