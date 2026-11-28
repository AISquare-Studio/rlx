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
import shutil
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
    exist and hold a file each, and a ``notes.txt`` sits directly in it. The run's own
    configuration is put back after."""
    home = tmp_path / "real-home"
    for name in (".claude", ".claude-2", ".codex", ".aisquare", ".rlx", "Documents"):
        (home / name).mkdir(parents=True)
        (home / name / "kept").write_text("x", encoding="utf-8")
    (home / ".claude.json").write_text("{}", encoding="utf-8")
    (home / "notes.txt").write_text("mine", encoding="utf-8")
    sqlite3.connect(home / "Documents" / "board.db").close()
    before = guard.snapshot()
    guarded = guard.guarded_paths(home, {})
    allowed = [str(REPO), str(tmp_path / "run-root"), str(home / "work")]
    guard.configure(guarded, allowed, real_home=home)
    monkeypatch.setenv(guard.GUARDED_VAR, os.pathsep.join(guarded))
    monkeypatch.setenv(guard.ALLOWED_VAR, os.pathsep.join(allowed))
    monkeypatch.setenv(guard.REAL_HOME_VAR, str(home))
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


# --- nothing is written directly in the real home (manager #3718) --------------------------

WRITES: list[tuple[str, Callable[[Path], object], str]] = [
    ("a file made", lambda h: (h / "new.txt").write_text("x"), "new.txt"),
    ("a file appended to", lambda h: open(h / "notes.txt", "a").close(), "notes.txt"),
    ("a folder made", lambda h: (h / "project").mkdir(), "project"),
    ("a file moved in", lambda h: (h / "Documents" / "kept").rename(h / "kept"), "kept"),
    ("a file removed", lambda h: (h / "notes.txt").unlink(), "notes.txt"),
    ("a database made", lambda h: sqlite3.connect(h / "rows.sqlite3"), "rows.sqlite3"),
    ("a temp dir made", lambda h: tempfile.mkdtemp(dir=h), ""),
    ("the home itself", lambda h: os.chmod(h, 0o700), "real-home"),
]


@pytest.mark.parametrize(
    ("write", "name"), [(w[1], w[2]) for w in WRITES], ids=[w[0] for w in WRITES]
)
def test_nothing_is_written_directly_in_the_real_home(
    write: Callable[[Path], object], name: str, scratch_real_home: Path, own_log: Path
) -> None:
    """What reviewer #3717 found unguarded: a project folder or a file dropped beside the
    person's own, outside ``.claude*``, ``.codex*``, ``.aisquare`` and ``.rlx``."""
    with pytest.raises(guard.RealHomeRefused, match="directly in the developer's real home"):
        write(scratch_real_home)
    entries, _ = guard.read_log(own_log)
    assert [entry["kind"] for entry in entries] == ["home-write"]
    if name:
        assert Path(entries[0]["path"]).name == name
    assert (scratch_real_home / "notes.txt").read_text(encoding="utf-8") == "mine"


def test_reading_in_the_real_home_and_writing_deeper_or_in_a_run_root_are_not_refused(
    scratch_real_home: Path, own_log: Path
) -> None:
    """Directly in the home is the rule; deeper is the guarded roots' (``Documents`` is the
    person's, and nobody's business here, but not the guard's); a run root is the run's."""
    assert (scratch_real_home / "notes.txt").read_text(encoding="utf-8") == "mine"
    (scratch_real_home / "Documents" / "new.txt").write_text("x", encoding="utf-8")
    (scratch_real_home / "work").mkdir()
    (scratch_real_home / "work" / "x").write_text("x", encoding="utf-8")
    assert refusals(own_log) == []


def test_a_child_that_writes_directly_in_the_real_home_is_refused(
    scratch_real_home: Path, own_log: Path
) -> None:
    code = f"open({str(scratch_real_home / 'dropped.txt')!r}, 'w').close()"
    child = subprocess.run(
        [sys.executable, "-c", code], env=os.environ, capture_output=True, text=True, timeout=60
    )
    assert child.returncode != 0 and "RealHomeRefused" in child.stderr, child.stderr
    assert [entry["kind"] for entry in guard.read_log(own_log)[0]] == ["home-write"]
    assert not (scratch_real_home / "dropped.txt").exists()


# --- no test's folder is in the real home, so no walk reaches it (#3717's MAJOR) -----------


def test_this_runs_folders_are_outside_the_real_home_and_git_stops_below_it(
    tmp_path: Path, isolated_user_home: Path
) -> None:
    """On Windows ``%TEMP%`` is always in the home; on POSIX a ``TMPDIR`` under ``~/.rlx`` is
    (#57). There a markerless folder walked up past the test's own ``HOME`` to the real one,
    and ``core.workspace`` took the real home, with a dotfiles ``.git``, for its project."""
    real = conftest._DEVELOPERS_HOME
    for folder in (tmp_path, isolated_user_home, Path(tempfile.gettempdir())):
        assert not conftest.inside(folder, real), folder
    assert str(real) in os.environ["GIT_CEILING_DIRECTORIES"].split(os.pathsep)


def test_the_temp_root_moves_only_when_it_is_in_the_home(tmp_path: Path) -> None:
    home, outside = tmp_path / "home", tmp_path / "elsewhere"
    in_home = home / ".rlx" / "tmp" / "agent"
    in_home.mkdir(parents=True)
    assert conftest.temp_root_outside(home, str(outside), [str(in_home)]) == str(outside)
    candidates = [str(home / "AppData" / "Temp"), str(outside / "rlx-tests")]
    assert conftest.temp_root_outside(home, str(in_home), candidates) == str(outside / "rlx-tests")
    assert (outside / "rlx-tests").is_dir()
    assert conftest.temp_root_outside(home, str(in_home), [str(home / "x")]) is None
    said = conftest.temp_problem(home, str(in_home))
    assert conftest.TEMP_ROOT_VAR in said and str(in_home) in said


def test_where_the_temp_root_may_go_starts_with_the_developers_pick() -> None:
    found = conftest.temp_candidates({conftest.TEMP_ROOT_VAR: "/picked", "RUNNER_TEMP": "/rt"})
    assert found[0] == "/picked"
    if os.name == "nt":
        assert found[1].endswith("rlx-tests") and found[2] == "/rt"
    else:
        assert found[1:] == ["/tmp", "/var/tmp"]


def _inner_run(
    tmp_path: Path, files: dict[str, str], env: dict[str, str], *args: str
) -> subprocess.CompletedProcess[str]:
    """A pytest of its own in ``tmp_path``, with the suite's conftest, ``files`` as its tests,
    and ``env`` over the suite's environment less what the inner run names for itself."""
    for name, source in files.items():
        (tmp_path / name).write_text(textwrap.dedent(source), encoding="utf-8")
    full = {**os.environ, "PYTHONPATH": os.pathsep.join([str(REPO), os.environ["PYTHONPATH"]])}
    for name in (guard.LOG_VAR, guard.GUARDED_VAR, guard.ALLOWED_VAR, guard.REAL_HOME_VAR):
        full.pop(name, None)  # the inner run names its own
    full.pop("PYTEST_DEBUG_TEMPROOT", None)
    full.update(env)
    argv = [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", "-p", "no:xdist"]
    argv += ["-o", "addopts=", *args, *files]
    return subprocess.run(argv, cwd=tmp_path, env=full, capture_output=True, text=True, timeout=300)


INNER_CONFTEST = (
    "from tests.conftest import (  # noqa: F401\n"
    "    _home_of_the_run, isolated_user_home, keychain_guard, no_real_keychain,\n"
    ")\n"
)


@pytest.mark.skipif(shutil.which("git") is None, reason="needs git")
@pytest.mark.parametrize(
    "inside",
    [".rlx/tmp/agent", "AppData/Local/Temp"],
    ids=["TMPDIR under ~/.rlx", "Windows' %TEMP%"],
)
def test_a_temp_dir_in_the_real_home_is_moved_out_so_no_walk_reaches_the_home(
    inside: str, tmp_path: Path
) -> None:
    """The reviewer's probe, through the suite's conftest: a "developer" whose home is a git
    repository (dotfiles) and whose temp dir is inside it. The run moves its temp root out
    (here to :data:`conftest.TEMP_ROOT_VAR`'s pick), so a markerless folder of a test has no
    project, and git, a C child the hook can't see, finds no repository for it."""
    real = tmp_path / "developer"
    (real / inside).mkdir(parents=True)
    subprocess.run(["git", "init", "-q"], cwd=real, check=True)
    moved = tmp_path / "moved-here"
    probe = f"""
        import os, subprocess, tempfile
        from pathlib import Path
        from aisquare.core import workspace
        from tests import conftest

        def test_no_walk_reaches_the_home(tmp_path):
            real = Path({str(real)!r})
            start = tmp_path / "markerless"
            start.mkdir()
            assert workspace.real_project_root(start) is None
            assert workspace.project_tree(start) is None
            assert workspace.find_project_root(start) == start.resolve()
            assert not conftest.inside(start, real)
            assert conftest.inside(tempfile.gettempdir(), {str(moved)!r})
            assert str(real) in os.environ["GIT_CEILING_DIRECTORIES"].split(os.pathsep)
            done = subprocess.run(
                ["git", "rev-parse", "--show-toplevel"], cwd=start, capture_output=True, text=True
            )
            assert done.returncode != 0, done.stdout
    """
    temp = str(real / inside)
    env = {name: str(real) for name in conftest.USER_HOME_VARS}
    env.update(TMPDIR=temp, TEMP=temp, TMP=temp, **{conftest.TEMP_ROOT_VAR: str(moved)})
    env["GIT_CEILING_DIRECTORIES"] = ""
    inner = _inner_run(tmp_path, {"conftest.py": INNER_CONFTEST, "test_probe.py": probe}, env)
    assert inner.returncode == 0, inner.stdout + inner.stderr
    assert "1 passed" in inner.stdout


def test_a_basetemp_named_in_the_real_home_is_refused(tmp_path: Path) -> None:
    real = tmp_path / "developer"
    real.mkdir()
    env = {name: str(real) for name in conftest.USER_HOME_VARS}
    files = {"conftest.py": INNER_CONFTEST, "test_any.py": "def test_any():\n    pass\n"}
    inner = _inner_run(tmp_path, files, env, "--basetemp", str(real / "projects" / "base"))
    assert inner.returncode == 4, inner.stdout + inner.stderr
    assert "is inside your home" in inner.stdout + inner.stderr


# --- the opt-ins, enforced by the asking test's file (reviewer #3717, minor 1) -------------


def test_real_home_is_refused_to_a_test_outside_its_file(request: pytest.FixtureRequest) -> None:
    with pytest.raises(pytest.fail.Exception, match="may not ask for real_home"):
        request.getfixturevalue("real_home")
    assert Path.home() != conftest._DEVELOPERS_HOME


def test_the_opt_ins_go_by_file_name_in_a_run_of_their_own(tmp_path: Path) -> None:
    """With a scratch "developer": the e2e's file gets the real home and real agents, a native
    suite's file gets real agents unasked and a home under ``RLX_TESTS_NATIVE_HOMES`` (outside
    every sandbox's temporary roots, so a real Codex's launch isn't refused), and any other
    file asking for the home fails."""
    real = tmp_path / "developer"
    real.mkdir()
    native_homes = tmp_path / "native-homes"
    files = {
        "conftest.py": INNER_CONFTEST
        + "from tests.conftest import real_agent_binaries, real_home  # noqa: F401\n",
        "test_cross_account_e2e.py": f"""
            from pathlib import Path
            import pytest
            from tests.keychain_guard import rlx_keychain_guard as guard

            @pytest.mark.usefixtures("real_agent_binaries")
            def test_the_e2e_gets_both(real_home):
                assert real_home == Path.home() == Path({str(real)!r})
                state = guard.snapshot()
                assert state["home_allowed"] and state["agents_allowed"]
        """,
        "test_claude_native_probe.py": f"""
            from pathlib import Path
            from tests.keychain_guard import rlx_keychain_guard as guard

            def test_a_native_file_gets_real_agents_and_a_home_outside_the_temp_roots():
                state = guard.snapshot()
                assert state["agents_allowed"] and not state["home_allowed"]
                homes = Path({str(native_homes)!r}) / "rlx-native-homes"
                assert Path.home().parent == homes, Path.home()
        """,
        "test_other.py": """
            def test_asks_for_the_home(real_home):
                pass
        """,
    }
    env = {name: str(real) for name in conftest.USER_HOME_VARS}
    env["RLX_TESTS_NATIVE_HOMES"] = str(native_homes)
    inner = _inner_run(tmp_path, files, env, "--basetemp", str(tmp_path / "base"))
    out = inner.stdout
    assert "2 passed, 1 error" in out, out + inner.stderr
    assert list((native_homes / "rlx-native-homes").iterdir()) == [], "each is removed after"
    assert "test_other.py::test_asks_for_the_home may not ask for real_home" in out, out


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

            # A variable, given to the child by repr: a Windows path's backslashes don't survive
            # two string literals ("\\U" opens a unicode escape).
            TARGET = {str(real / ".claude")!r}

            def test_swallows_the_refusal():
                try:
                    open({claude_json!r}).read()
                except OSError:
                    pass

            def test_its_child_reaches_it():
                code = "import os; os.listdir(" + repr(TARGET) + ")"
                done = subprocess.run(
                    [sys.executable, "-c", code], env=os.environ, capture_output=True, text=True
                )
                assert "RealHomeRefused" in done.stderr, done.stderr

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
