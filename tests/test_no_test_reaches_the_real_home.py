"""No test, and no child it starts, sees the developer's real home (#54).

#52's guard stopped nine tests whose ``aisquare`` child read the real Keychain. Why they did is
what this closes: the suite moved rlx's home (``AISQUARE_HOME``) and the product's ``_home`` in
the pytest process, never ``HOME``. A child read the developer's ``~/.claude.json``, saw a
signed-in account, and asked the Keychain for its login (reviewer note #3289).

So every test's ``HOME`` (and ``USERPROFILE`` on Windows) is a directory of its own
(``tests/conftest.py``'s ``isolated_user_home``), and the run's is too, between tests
(``_home_of_the_run``). The guard's tripwire then refuses any open, listing or change under the
real home's ``.claude*``, ``.codex*``, ``.aisquare`` and ``.rlx``, or under a Claude Code, Codex
or rlx directory the shell named, in the pytest process and in every Python child: a test that
undid the move still reads nothing there, and fails, named.

Every probe here aims the guard at a scratch "real home" of its own (:func:`scratch_real_home`),
so even with the guard broken it would open a file the test wrote, never the developer's.
"""

from __future__ import annotations

import os
import sqlite3
import subprocess
import sys
import tempfile
import textwrap
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest

from tests import conftest
from tests.keychain_guard import rlx_keychain_guard as guard

REPO = Path(__file__).resolve().parents[1]


@pytest.fixture
def own_log(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """This test's refusals go to a file of its own, for it to read (children inherit it)."""
    log = tmp_path / "refusals.jsonl"
    monkeypatch.setenv(guard.LOG_VAR, str(log))
    return log


@pytest.fixture
def scratch_real_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    """The guard aimed at a home of this test's, as if it were the developer's: its
    ``.claude.json``, ``.claude/``, ``.claude-2/``, ``.codex/``, ``.aisquare/`` and ``.rlx/``
    exist and hold a file each. The run's own configuration is put back after."""
    home = tmp_path / "real-home"
    for name in (".claude", ".claude-2", ".codex", ".aisquare", ".rlx", "Documents"):
        (home / name).mkdir(parents=True)
        (home / name / "kept").write_text("x", encoding="utf-8")
    (home / ".claude.json").write_text("{}", encoding="utf-8")
    sqlite3.connect(home / "Documents" / "board.db").close()
    before = guard.snapshot()
    guarded = guard.guarded_paths(home, {})
    allowed = [str(REPO), str(tmp_path / "run-root")]
    guard.configure(guarded, allowed)
    monkeypatch.setenv(guard.GUARDED_VAR, os.pathsep.join(guarded))
    monkeypatch.setenv(guard.ALLOWED_VAR, os.pathsep.join(allowed))
    yield home
    guard.restore(before)


def refusals(log: Path) -> list[tuple[str, str, str]]:
    entries, _ = guard.read_log(log)
    return [(entry["kind"], entry["event"], Path(entry["path"]).name) for entry in entries]


# --- the moved home ------------------------------------------------------------------------


def test_every_test_has_a_home_of_its_own_beside_its_tmp_path(
    isolated_user_home: Path, tmp_path: Path
) -> None:
    assert Path.home() == isolated_user_home != conftest._DEVELOPERS_HOME
    assert isolated_user_home == tmp_path.with_name(tmp_path.name + "-home")
    assert isolated_user_home.is_dir() and not isolated_user_home.is_relative_to(tmp_path)
    for name in conftest.USER_HOME_VARS:
        assert os.environ[name] == str(isolated_user_home)
    if os.name == "nt":
        assert conftest.USER_HOME_VARS == ("HOME", "USERPROFILE")
    for name in guard.REAL_DIR_VARS:
        if name != "AISQUARE_HOME":  # isolated_home's, the test's own
            assert name not in os.environ, name
    assert os.environ[guard.REAL_HOME_VAR] == str(conftest._DEVELOPERS_HOME)


@pytest.mark.parametrize("flags", [[], ["-E"], ["-I"]], ids=["plain", "-E", "-I"])
def test_a_child_has_the_tests_home_even_one_that_ignores_python_variables(
    flags: list[str], isolated_user_home: Path
) -> None:
    """``-E`` and ``-I`` drop ``PYTHONPATH`` (so the guard's ``sitecustomize``), not ``HOME``:
    the product's ``-E`` children (#54's m2) find the test's home, with no account in it."""
    code = "import os, pathlib; print(pathlib.Path.home()); print(os.environ.get('CODEX_HOME'))"
    child = subprocess.run(
        [sys.executable, *flags, "-c", code], capture_output=True, text=True, timeout=60
    )
    assert child.stdout.splitlines() == [str(isolated_user_home), "None"], child.stderr


def test_between_tests_the_user_home_and_the_rlx_home_are_the_runs_own(
    _home_of_the_run: Path, home_outside_a_test: Path, tmp_path_factory: pytest.TempPathFactory
) -> None:
    """Even where the shell named an rlx home (a fleet pane's live one, or CI's ambient legs'
    configured one): that home is guarded, never used (#54)."""
    basetemp = tmp_path_factory.getbasetemp()
    assert _home_of_the_run.parent == basetemp == home_outside_a_test.parent
    assert _home_of_the_run != conftest._DEVELOPERS_HOME
    if conftest.SHELL_HOME is not None:
        assert home_outside_a_test != Path(conftest.SHELL_HOME)


def test_the_guard_is_aimed_at_the_real_home_in_this_process_and_in_children() -> None:
    """By the paths' text alone: nothing here opens them."""
    home = conftest._DEVELOPERS_HOME
    roots, prefixes, allowed = guard.guarded_now()
    expected = guard.guarded_paths(home, {})
    assert {guard._norm(str(home / ".claude")), guard._norm(str(home / ".codex"))} <= set(prefixes)
    assert {guard._norm(str(home / ".aisquare")), guard._norm(str(home / ".rlx"))} <= set(roots)
    assert set(expected) <= set(conftest._GUARDED)
    assert os.environ[guard.GUARDED_VAR].split(os.pathsep) == conftest._GUARDED
    assert guard._norm(tempfile.gettempdir()) in allowed
    assert guard._norm(str(REPO)) in allowed
    if conftest.SHELL_HOME:
        assert guard.home_refused(conftest.SHELL_HOME), "the home the shell named is guarded"


def test_the_shells_own_directories_are_guarded_as_it_named_them(tmp_path: Path) -> None:
    named = {name: str(tmp_path / name.lower()) for name in guard.REAL_DIR_VARS}
    found = guard.guarded_paths(tmp_path / "home", named)
    assert [str(tmp_path / name.lower()) for name in guard.REAL_DIR_VARS] == found[4:]
    assert found[:4] == [
        str(tmp_path / "home" / ".claude") + "*",
        str(tmp_path / "home" / ".codex") + "*",
        str(tmp_path / "home" / ".aisquare"),
        str(tmp_path / "home" / ".rlx"),
    ]


# --- the tripwire, in this process ---------------------------------------------------------

REACHES: list[tuple[str, Callable[[Path], object], str]] = [
    ("read .claude.json", lambda h: (h / ".claude.json").read_text(), ".claude.json"),
    ("write .claude.json", lambda h: (h / ".claude.json").write_text("{}"), ".claude.json"),
    ("os.open", lambda h: os.close(os.open(h / ".claude.json", os.O_RDONLY)), ".claude.json"),
    ("a slot's dir", lambda h: (h / ".claude-2" / "kept").read_bytes(), "kept"),
    ("list .claude", lambda h: os.listdir(h / ".claude"), ".claude"),
    ("scandir .codex", lambda h: list(os.scandir(h / ".codex")), ".codex"),
    ("iterdir .aisquare", lambda h: list((h / ".aisquare").iterdir()), ".aisquare"),
    ("glob .rlx", lambda h: list((h / ".rlx").glob("*")), ".rlx"),
    ("mkdir in .rlx", lambda h: (h / ".rlx" / "new").mkdir(), "new"),
    ("rename into .codex", lambda h: (h / "Documents" / "kept").rename(h / ".codex" / "k"), "k"),
    ("remove from .aisquare", lambda h: (h / ".aisquare" / "kept").unlink(), "kept"),
    (
        "sqlite in .aisquare",
        lambda h: sqlite3.connect(h / ".aisquare" / "rows.sqlite3"),
        "rows.sqlite3",
    ),
    ("chdir into .claude", lambda h: os.chdir(h / ".claude"), ".claude"),
]


@pytest.mark.parametrize(
    ("reach", "where"), [(r[1], r[2]) for r in REACHES], ids=[r[0] for r in REACHES]
)
def test_the_real_homes_state_is_refused_in_this_process(
    reach: Callable[[Path], object], where: str, scratch_real_home: Path, own_log: Path
) -> None:
    """Raised where it would have happened, and logged, which is what fails a test: some
    callers swallow the ``PermissionError`` (pathlib's glob does, from Python 3.13)."""
    try:
        reach(scratch_real_home)
    except guard.RealHomeRefused as refused:
        assert "the developer's real Claude Code, Codex" in str(refused)
    else:
        assert where == ".rlx", "only the glob swallows it"
    assert [(kind, name) for kind, _, name in refusals(own_log)][:1] == [("home", where)]


def test_the_rest_of_the_real_home_is_not_refused(scratch_real_home: Path, own_log: Path) -> None:
    """Only the agents' and rlx's state: the home itself, and the person's other files, are
    nobody's business here but not the guard's either. ``os.stat`` raises no audit event."""
    assert sorted(path.name for path in scratch_real_home.iterdir())
    assert (scratch_real_home / "Documents" / "kept").read_text(encoding="utf-8") == "x"
    sqlite3.connect(scratch_real_home / "Documents" / "board.db").close()
    assert (scratch_real_home / ".claude.json").exists()
    assert refusals(own_log) == []


def test_a_run_root_inside_a_guarded_dir_is_the_runs_own(
    scratch_real_home: Path, own_log: Path, tmp_path: Path
) -> None:
    """A temp dir under ``~/.rlx`` (a fleet agent's own) holds the run's files: allowed. A
    guarded dir under a run root is still guarded."""
    own = scratch_real_home / ".rlx" / "tmp" / "agent"
    guard.configure(guard.guarded_paths(scratch_real_home, {}), [str(own), str(tmp_path)])
    assert not guard.home_refused(own / "pytest-of-x" / "file")
    assert guard.home_refused(scratch_real_home / ".rlx" / "rows.sqlite3")
    assert guard.home_refused(scratch_real_home / ".claude.json"), (
        "tmp_path holds it: still guarded"
    )
    assert refusals(own_log) == []


def test_a_relative_path_is_read_from_where_the_test_stands(
    scratch_real_home: Path, own_log: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.chdir(scratch_real_home)
    with pytest.raises(guard.RealHomeRefused):
        Path(".claude.json").read_text(encoding="utf-8")
    assert refusals(own_log) == [("home", "open", ".claude.json")]


def test_real_home_lifts_the_tripwire_and_gives_children_nothing_to_guard(
    real_home: Path,
) -> None:
    """The opt-in, touching nothing: the real ``HOME`` is back, this process lets the guarded
    paths through, and a child is told to guard none."""
    assert real_home == conftest._DEVELOPERS_HOME == Path.home()
    assert guard.snapshot()["home_allowed"] is True
    assert guard.GUARDED_VAR not in os.environ


def test_only_the_real_account_e2e_asks_for_the_real_home() -> None:
    """By name, as a parameter or in ``usefixtures``: the guard test above, and the e2e."""
    askers = []
    for path in sorted((REPO / "tests").rglob("*.py")):
        text = path.read_text(encoding="utf-8")
        if '"real_home"' in text or "real_home: Path" in text:
            askers.append(path.name)
    assert askers == ["test_cross_account_e2e.py", Path(__file__).name]


# --- the tripwire, in a child --------------------------------------------------------------


@pytest.mark.parametrize(
    "code",
    [
        "open({claude_json!r}).read()",
        "import os; os.listdir({codex!r})",
        "import sqlite3; sqlite3.connect({rows_db!r})",
    ],
    ids=["open", "listdir", "sqlite3"],
)
def test_a_python_child_that_reaches_the_real_home_is_refused(
    code: str, scratch_real_home: Path, own_log: Path
) -> None:
    """A child with the suite's environment loads the guard's ``sitecustomize``, which takes
    what to guard from it (``RLX_TESTS_GUARDED_PATHS``)."""
    source = code.format(
        claude_json=str(scratch_real_home / ".claude.json"),
        codex=str(scratch_real_home / ".codex"),
        rows_db=str(scratch_real_home / ".aisquare" / "rows.sqlite3"),
    )
    child = subprocess.run(
        [sys.executable, "-c", source], env=os.environ, capture_output=True, text=True, timeout=60
    )
    assert child.returncode != 0 and "RealHomeRefused" in child.stderr, child.stderr
    entries, _ = guard.read_log(own_log)
    assert [entry["kind"] for entry in entries] == ["home"]
    assert entries[0]["pid"] != os.getpid()
    assert "test_a_python_child_that_reaches_the_real_home_is_refused" in entries[0]["test"]


def test_the_failure_names_the_test_and_the_path() -> None:
    entry = {
        "test": "tests/test_x.py::test_y (call)",
        "pid": 7,
        "kind": "home",
        "event": "open",
        "path": "/Users/someone/.claude.json",
    }
    said = guard.failure("tests/test_x.py::test_y", [entry])
    assert said.startswith("tests/test_x.py::test_y reached the developer's real home")
    assert "open /Users/someone/.claude.json (pid 7" in said
    assert "isolated_user_home" in said


def test_a_test_that_reaches_the_real_home_fails_at_teardown_naming_itself(
    tmp_path: Path,
) -> None:
    """End to end, in a pytest of its own whose "developer" has a scratch home: a test that
    swallows the refusal (as product code catching ``OSError`` does) still fails, at teardown,
    named, and so does one whose child reached it."""
    real = tmp_path / "developer"
    (real / ".claude").mkdir(parents=True)
    (real / ".claude.json").write_text("{}", encoding="utf-8")
    (tmp_path / "conftest.py").write_text(
        "from tests.conftest import _home_of_the_run, keychain_guard, no_real_keychain  # noqa\n",
        encoding="utf-8",
    )
    claude_json = str(real / ".claude.json")
    (tmp_path / "test_reaches.py").write_text(
        textwrap.dedent(
            f"""
            import os, subprocess, sys

            def test_swallows_the_refusal():
                try:
                    open({claude_json!r}).read()
                except OSError:
                    pass

            def test_its_child_reaches_it():
                code = "import os; os.listdir({str(real / ".claude")!r})"
                subprocess.run([sys.executable, "-c", code], env=os.environ, capture_output=True)

            def test_never_reaches_it():
                assert True
            """
        ),
        encoding="utf-8",
    )
    env = {**os.environ, "PYTHONPATH": os.pathsep.join([str(REPO), os.environ["PYTHONPATH"]])}
    for name in conftest.USER_HOME_VARS:
        env[name] = str(real)  # the inner run's conftest takes this for the developer's
    env.pop(guard.LOG_VAR, None)  # the inner run names its own
    for name in (guard.GUARDED_VAR, guard.ALLOWED_VAR):
        env.pop(name, None)  # the inner run's conftest names its own
    argv = [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", "-p", "no:xdist"]
    argv += ["--basetemp", str(tmp_path / "base"), "-o", "addopts=", "test_reaches.py"]
    inner = subprocess.run(argv, cwd=tmp_path, env=env, capture_output=True, text=True, timeout=300)
    out = inner.stdout
    assert inner.returncode == 1, out + inner.stderr
    assert "3 passed, 2 errors" in out, out
    for name in ("test_swallows_the_refusal", "test_its_child_reaches_it"):
        assert f"test_reaches.py::{name} reached the developer's real home" in out, out
    assert "test_never_reaches_it reached" not in out
