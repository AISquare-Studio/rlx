"""No test can reach the real macOS Keychain tool: the suite-wide guard is in force (#52).

A package-wide guard (the twelfth): it proves the autouse that covers every test, rather than
scanning files. A scratch ``HOME`` does not scope the Keychain. ``test_persona_import``'s Ctrl-C
test ran the real ``security find-generic-password -s "Claude Code-credentials"`` in the
``aisquare`` child it started, and on a developer's Mac that reads the person's own Claude Code
login (tester note #3183). The guard (``tests/keychain_guard/rlx_keychain_guard.py``, installed
by ``tests/conftest.py``'s ``keychain_guard`` and ``no_real_keychain``) refuses the tool in the
pytest process and in every Python child that inherits the suite's environment, and fails the
test that tried, naming it.

Every probe here is harmless even if the guard were broken: ``security -h`` prints its usage,
and the product's read asks for an item no one has (:data:`PROBE_SERVICE`). Each probe writes
its refusal to a log of its own (:func:`own_log`), so the run's log, where any line fails the
test, stays clean.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

from aisquare.core import claude_accounts as core
from tests.conftest import KEYCHAIN_GUARD_DIR
from tests.keychain_guard import rlx_keychain_guard as guard

REAL = guard.REAL_TOOL
PROBE_SERVICE = "rlx-tests-keychain-guard-probe-no-such-item"
REPO = Path(__file__).resolve().parents[1]


@pytest.fixture
def own_log(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """This test's refusals go to a file of its own, for it to read (children inherit it)."""
    log = tmp_path / "refusals.jsonl"
    monkeypatch.setenv(guard.LOG_VAR, str(log))
    return log


def refusals(log: Path) -> list[tuple[str | None, str, str]]:
    entries, _ = guard.read_log(log)
    return [(entry["program"], entry["subcommand"], entry["service"]) for entry in entries]


def test_the_guard_is_in_force_for_every_test() -> None:
    """The hook is in this process. The product's tool is the refused sentinel and its
    Keychain is off. A child finds the guard first on PYTHONPATH, and the run logs."""
    assert guard.installed()
    assert core.KEYCHAIN_TOOL == guard.SENTINEL
    assert core.keychain_platform() is False
    assert os.environ["PYTHONPATH"].split(os.pathsep)[0] == KEYCHAIN_GUARD_DIR
    assert (Path(KEYCHAIN_GUARD_DIR) / "sitecustomize.py").is_file()
    assert Path(os.environ[guard.LOG_VAR]).is_file()


SPAWNS = [
    pytest.param(lambda: subprocess.run([REAL, "-h"], capture_output=True), id="subprocess"),
    pytest.param(lambda: subprocess.run(f"{REAL} -h", shell=True, capture_output=True), id="shell"),
    pytest.param(lambda: os.system(f"{REAL} -h"), id="os.system"),
]
if hasattr(os, "posix_spawn"):
    SPAWNS.append(
        pytest.param(lambda: os.posix_spawn(REAL, [REAL, "-h"], dict(os.environ)), id="posix_spawn")
    )
if shutil.which("security") and os.path.realpath(shutil.which("security") or "") == REAL:
    SPAWNS.append(
        pytest.param(lambda: subprocess.run(["security", "-h"], capture_output=True), id="on PATH")
    )


@pytest.mark.parametrize("spawn", SPAWNS)
def test_the_real_tool_is_refused_in_this_process(spawn: object, own_log: Path) -> None:
    with pytest.raises(guard.KeychainRefused, match="use the suite's fake security"):
        spawn()  # type: ignore[operator]
    assert [program for program, _, _ in refusals(own_log)] in ([REAL], ["security"])


def test_a_fake_security_of_its_own_is_not_refused(tmp_path: Path, own_log: Path) -> None:
    """The guard refuses the real tool, not every program called ``security``."""
    fake = tmp_path / "security"
    fake.write_text("", encoding="utf-8")
    assert not guard.refused(str(fake))
    assert guard.refused(REAL) and guard.refused(guard.SENTINEL)
    assert refusals(own_log) == []


@pytest.mark.parametrize("tool", [REAL, guard.SENTINEL], ids=["real tool", "sentinel"])
def test_the_products_keychain_read_is_refused_and_reads_nothing(
    tool: str, own_log: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``core.read_keychain`` with the Keychain on, through the real tool put back (as a
    test that undid the autouse would) or through the sentinel the autouse leaves."""
    monkeypatch.setattr(core, "keychain_platform", lambda: True)
    monkeypatch.setattr(core, "KEYCHAIN_TOOL", tool)
    read = core.read_keychain(PROBE_SERVICE, "rlx-tests", fresh=True)
    core.forget_keychain_reads()
    assert (read.found, read.credentials, read.problem) == (False, None, "failed")
    assert refusals(own_log) == [(tool, "find-generic-password", PROBE_SERVICE)]


CHILD_PROBES = [
    pytest.param(
        f"import subprocess; subprocess.run([{REAL!r}, '-h'])", (REAL, "-h", ""), id="subprocess"
    ),
    pytest.param(f"import os; os.execv({REAL!r}, ['security', '-h'])", (REAL, "-h", ""), id="exec"),
    pytest.param(
        "from aisquare.core import claude_accounts as c\n"
        "c.keychain_platform = lambda: True\n"
        f"read = c.read_keychain({PROBE_SERVICE!r}, 'rlx-tests')\n"
        "print(read.problem, read.credentials)\n",
        (REAL, "find-generic-password", PROBE_SERVICE),
        id="the product's read",
    ),
]


@pytest.mark.parametrize(("code", "refusal"), CHILD_PROBES)
def test_a_python_child_that_runs_the_real_tool_is_refused(
    code: str, refusal: tuple[str, str, str], own_log: Path
) -> None:
    """A child with the suite's environment, as ``test_persona_import``'s ``aisquare`` was."""
    child = subprocess.run(
        [sys.executable, "-c", code], env=os.environ, capture_output=True, text=True, timeout=60
    )
    assert refusals(own_log) == [refusal], child.stderr
    entries, _ = guard.read_log(own_log)
    assert entries[0]["pid"] != os.getpid()
    assert "test_a_python_child_that_runs_the_real_tool_is_refused" in entries[0]["test"]
    if "read_keychain" in code:
        assert child.stdout.split() == ["failed", "None"], child.stderr
    else:
        assert child.returncode != 0 and "KeychainRefused" in child.stderr


@pytest.mark.skipif(os.name == "nt", reason="the fake security is a script run by its #! line")
def test_a_child_given_the_fake_reads_the_keychain_through_it(
    tmp_path: Path, own_log: Path
) -> None:
    from tests.test_claude_keychain import FakeKeychain

    fake = FakeKeychain(tmp_path / "keychain")
    code = (
        "from aisquare.core import claude_accounts as c\n"
        "c.keychain_platform = lambda: True\n"
        f"read = c.read_keychain({PROBE_SERVICE!r}, 'rlx-tests')\n"
        "print(c.KEYCHAIN_TOOL == " + repr(str(fake.tool)) + ", read.found, read.problem)\n"
    )
    env = {**os.environ, guard.FAKE_VAR: str(fake.tool)}
    child = subprocess.run(
        [sys.executable, "-c", code], env=env, capture_output=True, text=True, timeout=60
    )
    assert child.stdout.split() == ["True", "False", "None"], child.stderr
    assert fake.calls() == [["find-generic-password", "-a", "rlx-tests", "-w", "-s", PROBE_SERVICE]]
    assert refusals(own_log) == []


def test_the_failure_names_the_test_what_it_ran_and_the_fake() -> None:
    entry = {
        "test": "tests/test_x.py::test_y (call)",
        "pid": 7,
        "program": REAL,
        "subcommand": "find-generic-password",
        "service": "Claude Code-credentials",
    }
    said = guard.failure("tests/test_x.py::test_y", [entry])
    assert said.startswith("tests/test_x.py::test_y reached the real macOS Keychain tool")
    assert f"{REAL} find-generic-password 'Claude Code-credentials'" in said
    assert "tests/test_claude_keychain.py's FakeKeychain" in said and guard.FAKE_VAR in said


def test_a_test_that_reaches_the_real_tool_fails_at_teardown_naming_itself(
    tmp_path: Path,
) -> None:
    """End to end, in a pytest of its own with the suite's two Keychain fixtures: a test
    that swallows the refusal (as product code catching ``OSError`` does) still fails, at
    teardown, named, and so does one whose child tried."""
    (tmp_path / "conftest.py").write_text(
        "from tests.conftest import keychain_guard, no_real_keychain  # noqa: F401\n",
        encoding="utf-8",
    )
    (tmp_path / "test_reaches.py").write_text(
        textwrap.dedent(
            f"""
            import os, subprocess, sys

            def test_swallows_the_refusal():
                try:
                    subprocess.run([{REAL!r}, "-h"], capture_output=True)
                except OSError:
                    pass

            def test_its_child_tries():
                code = "import subprocess; subprocess.run([{REAL!r}, '-h'])"
                subprocess.run([sys.executable, "-c", code], env=os.environ, capture_output=True)

            def test_never_tries():
                assert True
            """
        ),
        encoding="utf-8",
    )
    env = {**os.environ, "PYTHONPATH": os.pathsep.join([str(REPO), os.environ["PYTHONPATH"]])}
    env.pop(guard.LOG_VAR, None)  # the inner run names its own
    argv = [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", "-p", "no:xdist"]
    argv += ["--basetemp", str(tmp_path / "base"), "-o", "addopts=", "test_reaches.py"]
    inner = subprocess.run(
        argv,
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        timeout=300,
    )
    out = inner.stdout
    assert inner.returncode == 1, out + inner.stderr
    assert "3 passed, 2 errors" in out, out
    for name in ("test_swallows_the_refusal", "test_its_child_tries"):
        assert f"test_reaches.py::{name} reached the real macOS Keychain tool" in out, out
    assert "test_never_tries reached" not in out
