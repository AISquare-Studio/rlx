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
from collections.abc import Iterator
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


# --- the tool behind any shell form (#54's m1) ---------------------------------------------

#: Each line runs the tool, in command position, behind a shell form a word scan missed or
#: would miss (tester #3258, reviewer #3289). ``{tool}`` is the real path, so the table holds
#: on every platform (a bare ``security`` resolves only on a Mac: ``SPAWNS``' "on PATH").
REFUSED_LINES = [
    "{tool} -h",
    "sh -c '{tool} -h'",
    "bash -ec '{tool} -h'",
    "bash -lc '{tool} -h'",
    "zsh -xc '{tool} -h'",
    "sh -euxc '{tool} -h'",
    "fish --command '{tool} -h'",
    "env {tool} -h",
    "env -i PATH=/usr/bin {tool} -h",
    "env -u HOME {tool} -h",
    "FOO=1 BAR=2 {tool} -h",
    "nohup {tool} -h",
    "timeout 5 {tool} -h",
    "timeout -s KILL 5 {tool} -h",
    "echo x | xargs {tool} -h",
    "echo x | xargs -I{{}} {tool} -h {{}}",
    "command {tool} -h",
    "exec {tool} -h",
    "exec -a name {tool} -h",
    "nice -n 5 {tool} -h",
    "stdbuf -o L {tool} -h",
    "sudo -u root {tool} -h",
    "true && {tool} -h",
    "false || {tool} -h",
    "true; {tool} -h",
    "(cd /; {tool} -h)",
    "echo $({tool} -h)",
    "echo `{tool} -h`",
    "x=$({tool} find-generic-password -s y -w)",
    "env nohup timeout 5 {tool} -h",
    "sh -c \"bash -c '{tool} -h'\"",
]
#: The word outside command position: not the tool (#3258's false positive).
ALLOWED_LINES = [
    "echo security",
    "echo {tool}",
    "grep security notes.txt",
    "ls --flag={tool}",
    "git commit -m 'security fix'",
    "printf %s {tool}",
    "sh ./security.sh",
    "cat security",
    "env FOO={tool} true",
    "timeout 5 true security",
]


def _sh(line: str) -> tuple[object, ...]:
    """A ``subprocess.Popen`` audit event for ``sh -c line``, as POSIX audits an argument list:
    handed to the hook itself, so nothing is started whatever it decides."""
    return ("/bin/sh", ["/bin/sh", "-c", line], None, None)


@pytest.mark.parametrize("line", REFUSED_LINES)
def test_the_tool_behind_any_shell_form_is_refused(line: str, own_log: Path) -> None:
    line = line.format(tool=REAL)
    assert REAL in [argv[0] for argv in guard.commands(line)]
    with pytest.raises(guard.KeychainRefused):
        guard._hook("subprocess.Popen", _sh(line))
    assert [program for program, _, _ in refusals(own_log)] == [REAL]


@pytest.mark.parametrize("line", ALLOWED_LINES)
def test_the_word_outside_command_position_is_not_the_tool(line: str, own_log: Path) -> None:
    line = line.format(tool=REAL)
    words = [argv[0] for argv in guard.commands(line)]
    assert REAL not in words and "security" not in words, words
    guard._hook("subprocess.Popen", _sh(line))
    assert refusals(own_log) == []


@pytest.mark.skipif(os.name == "nt", reason="POSIX shells: Windows' cmd /c is in the table")
@pytest.mark.parametrize(
    "line",
    [
        "sh -ec '/usr/bin/security -h'",
        "env -i PATH=/usr/bin /usr/bin/security -h",
        "echo $(/usr/bin/security -h)",
    ],
)
def test_a_real_shell_running_the_tool_is_refused_before_it_starts(
    line: str, own_log: Path
) -> None:
    """Through ``subprocess`` for real, ``shell=True``: harmless even with the guard broken
    (``security -h`` prints its usage)."""
    with pytest.raises(guard.KeychainRefused):
        subprocess.run(line, shell=True, capture_output=True)
    assert [program for program, _, _ in refusals(own_log)] == [REAL]


# --- the other credential stores, and the real agents (#3381, #54) -------------------------

CREDENTIAL_LINES = [
    "secret-tool lookup service x",
    "sh -c 'secret-tool lookup a b'",
    "/usr/bin/secret-tool lookup a b",
    "cmdkey /list",
    "cmd /c cmdkey /list",
    'powershell -Command "cmdkey /list"',
]


@pytest.mark.parametrize("line", CREDENTIAL_LINES)
def test_a_credential_stores_tool_is_refused_by_name(line: str, own_log: Path) -> None:
    with pytest.raises(guard.KeychainRefused, match="credential store's tool"):
        guard._hook("subprocess.Popen", _sh(line))
    entries, _ = guard.read_log(own_log)
    assert [entry["kind"] for entry in entries] == ["credential-tool"]


def test_a_windows_command_line_is_read_without_backslash_escapes() -> None:
    """``subprocess`` audits one command line on Windows: its backslashes are path separators."""
    line = r'C:\Windows\System32\cmd.exe /c "C:\Windows\System32\cmdkey.exe /list"'
    found = [argv[0] for argv in guard.commands(line, windows=True)]
    assert found == [r"C:\Windows\System32\cmd.exe", r"C:\Windows\System32\cmdkey.exe"]
    assert guard.refusal_kind(found[1]) == "credential-tool"


#: How a stand-in agent is named where it must be found on PATH (PATHEXT on Windows).
AGENT_SUFFIX = ".cmd" if os.name == "nt" else ""


@pytest.fixture
def agents_elsewhere(tmp_path: Path) -> Iterator[Path]:
    """A ``claude`` and a ``codex`` (scripts that exit 0) in a directory the guard is told is
    none of the run's: as the developer's real ones, which the suite must never start."""
    where = tmp_path / "elsewhere"
    where.mkdir()
    for name in ("claude", "codex"):
        tool = where / f"{name}{AGENT_SUFFIX}"
        tool.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        tool.chmod(0o755)
    before = guard.snapshot()
    guard.restore({**before, "allowed": (guard._norm(str(tmp_path / "run-root")),)})
    yield where
    guard.restore(before)


@pytest.mark.parametrize("name", ["claude", "codex"])
def test_a_real_agent_binary_is_refused_by_path_and_on_path(
    name: str, agents_elsewhere: Path, own_log: Path
) -> None:
    tool = str(agents_elsewhere / f"{name}{AGENT_SUFFIX}")
    with pytest.raises(guard.AgentBinaryRefused, match="real claude or codex"):
        guard._hook("subprocess.Popen", (tool, [tool, "--version"], None, None))
    env = {"PATH": str(agents_elsewhere)}
    with pytest.raises(guard.AgentBinaryRefused):
        guard._hook("subprocess.Popen", (name, [name, "-p", "hi"], None, env))
    with pytest.raises(guard.AgentBinaryRefused):
        guard._hook("subprocess.Popen", _sh(f"nohup {Path(tool).as_posix()} -p hi"))
    entries, _ = guard.read_log(own_log)
    assert [entry["kind"] for entry in entries] == ["agent"] * 3


@pytest.mark.skipif(os.name == "nt", reason="the stand-in is a script run by its #! line")
def test_a_real_agent_binary_started_for_real_is_refused_before_it_runs(
    agents_elsewhere: Path, own_log: Path
) -> None:
    with pytest.raises(guard.AgentBinaryRefused):
        subprocess.run([str(agents_elsewhere / "claude"), "--version"], capture_output=True)
    assert [entry["kind"] for entry in guard.read_log(own_log)[0]] == ["agent"]


def test_a_stand_in_agent_or_one_not_installed_is_not_refused(
    tmp_path: Path, own_log: Path
) -> None:
    """The tests' own ``claude`` lives in a run root (``tmp_path``, ``tests/``); a ``claude``
    on no ``PATH`` starts nothing for the guard to refuse."""
    stand_in = tmp_path / "bin" / f"claude{AGENT_SUFFIX}"
    stand_in.parent.mkdir()
    stand_in.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    assert guard.refusal_kind(str(stand_in)) is None
    assert guard.refusal_kind("claude", {"PATH": str(tmp_path / "bin")}) is None
    assert guard.refusal_kind("codex", {"PATH": str(tmp_path / "nowhere")}) is None
    assert refusals(own_log) == []


@pytest.mark.usefixtures("real_agent_binaries")
def test_real_agent_binaries_lifts_the_refusal_here_and_in_children(
    agents_elsewhere: Path, own_log: Path
) -> None:
    assert os.environ[guard.AGENTS_VAR] == "1"
    assert guard.refusal_kind(str(agents_elsewhere / f"claude{AGENT_SUFFIX}")) is None
    assert refusals(own_log) == []


def test_the_native_suites_are_the_files_their_runners_run() -> None:
    from tests import conftest

    runners = {name: (REPO / "tests" / f"run_{name}_native.py") for name in ("claude", "codex")}
    for name, runner in runners.items():
        assert f"test_{name}_native" in runner.read_text(encoding="utf-8")
    assert conftest.NATIVE_SUITES == ("test_claude_native", "test_codex_native")
