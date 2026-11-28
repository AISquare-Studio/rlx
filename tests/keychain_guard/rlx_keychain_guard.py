"""No test reaches the real macOS Keychain tool: an audit hook refuses ``/usr/bin/security``.

A scratch ``HOME`` does not scope the Keychain. On a developer's Mac, a test that runs the
real ``security find-generic-password`` reads that person's own Claude Code login into a test
process. ``test_persona_import``'s Ctrl-C test did, through the ``aisquare`` child it starts
(#52, tester note #3183): conftest's ``no_real_keychain`` patches the pytest process only.

So the hook is installed twice over:

- in each pytest process, by ``tests/conftest.py``'s session fixture ``keychain_guard``;
- in every Python child that inherits the suite's environment, by ``sitecustomize.py`` in this
  directory, which conftest puts first on ``PYTHONPATH``.

Where the real tool (or :data:`SENTINEL`, the path conftest points the product's
``KEYCHAIN_TOOL`` at) would be started, by ``subprocess``, ``os.posix_spawn``, ``os.exec*``,
``os.spawn*`` or ``os.system``, the hook writes one line to the file named by
:data:`LOG_VAR` and raises :class:`KeychainRefused`. That is a ``PermissionError``, so the tool
never runs and the product carries on as with a tool it could not start. conftest reads the
file after each test and fails the test loudly, naming it. The line holds the test, the program,
the subcommand and the ``-s`` service, never a ``-w`` value.

A child test that needs the Keychain read through the suite's fake sets :data:`FAKE_VAR` to the
fake's path (``tests/test_claude_keychain.py``'s ``FakeKeychain``): ``sitecustomize`` points that
child's ``claude_accounts.KEYCHAIN_TOOL`` at it.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
from collections.abc import Sequence
from typing import Any

REAL_TOOL = "/usr/bin/security"
"""The program that reads the login Keychain (``core.claude_accounts.KEYCHAIN_TOOL``)."""

SENTINEL = "/nonexistent/rlx-tests/the-real-keychain-is-refused/security"
"""Where conftest points the product's ``KEYCHAIN_TOOL`` in every test. It is refused like the
real tool, and if the hook were ever missing it names no file, so nothing would run."""

LOG_VAR = "RLX_TESTS_KEYCHAIN_LOG"
"""The file each refusal is written to, one JSON line each, for conftest to read."""

FAKE_VAR = "RLX_TESTS_KEYCHAIN_TOOL"
"""A fake ``security`` a child process reads the Keychain through instead (``sitecustomize``)."""

ADVICE = (
    "no test may run the real macOS Keychain tool: use the suite's fake security"
    " (tests/test_claude_keychain.py's FakeKeychain, through monkeypatch on"
    f" claude_accounts.KEYCHAIN_TOOL; for a child process, {FAKE_VAR}=<the fake's path>)"
)

_SPAWNS = frozenset({"subprocess.Popen", "os.posix_spawn", "os.exec", "os.spawn", "os.system"})
_installed: list[bool] = []


class KeychainRefused(PermissionError):
    """Raised where the real Keychain tool would have started."""


def install() -> None:
    """Install the hook in this process, once: an audit hook can't be removed."""
    if not _installed:
        sys.addaudithook(_hook)
        _installed.append(True)


def installed() -> bool:
    return bool(_installed)


def refused(program: object, env: object = None) -> bool:
    """Whether starting ``program`` would start the real tool or :data:`SENTINEL`. A bare
    name is looked up on the ``PATH`` it would run with, so a fake ``security`` put first
    on ``PATH`` is not refused, and neither is a fake at a path of its own."""
    name = _text(program)
    if name is None:
        return False
    if name in (SENTINEL, REAL_TOOL):
        return True
    if os.sep in name or (os.altsep and os.altsep in name):
        return _same_file(name, REAL_TOOL)
    if name == "security":
        path = None
        if isinstance(env, dict):
            path = _text(env.get("PATH", env.get(b"PATH")))
        found = shutil.which(name, path=path if path is not None else os.environ.get("PATH"))
        return found is not None and _same_file(found, REAL_TOOL)
    return False


def _hook(event: str, args: tuple[Any, ...]) -> None:
    if event not in _SPAWNS:
        return
    program, argv, env = _spawned(event, args)
    if refused(program, env):
        _refuse(program, argv)


def _spawned(event: str, args: tuple[Any, ...]) -> tuple[object, list[object], object]:
    """The program, its arguments and its environment, from a spawn event's arguments. A
    command line (``os.system``, a shell's ``-c``, or Windows, where ``subprocess`` audits
    one string) is read word by word for the tool."""
    if event == "os.system":
        return _from_command(None, _text(args[0]) or "", None)
    if event == "subprocess.Popen":
        executable, argv, _cwd, env = args
    elif event == "os.spawn":
        _mode, executable, argv, env = args
    else:  # os.posix_spawn, os.exec: (path, argv, env)
        executable, argv, env = args
    if not isinstance(argv, (list, tuple)):
        return _from_command(executable, _text(argv) or "", env)
    words: list[object] = [_text(word) or "" for word in argv]
    if len(words) >= 3 and words[1] in ("-c", "/c", "/C"):
        program, tail, _ = _from_command(None, " ".join(map(str, words[2:])), env)
        if program is not None and _names_the_tool(program):
            return program, tail, env
    return (executable if executable is not None else (words[0] if words else None)), words, env


def _from_command(
    executable: object, command: str, env: object
) -> tuple[object, list[object], object]:
    words: list[object] = [word.strip("\"'") for word in command.split()]
    for at, word in enumerate(words):
        if _names_the_tool(word):
            return word, words[at:], env
    return (executable if executable is not None else (words[0] if words else None)), words, env


def _names_the_tool(word: object) -> bool:
    name = _text(word) or ""
    return name == "security" or name.endswith(("/security", "\\security"))


def _refuse(program: object, argv: Sequence[object]) -> None:
    words = [_text(word) or "" for word in argv]
    subcommand = words[1] if len(words) > 1 else ""
    service = words[words.index("-s") + 1] if "-s" in words[:-1] else ""
    entry = {
        "test": os.environ.get("PYTEST_CURRENT_TEST", "(outside a test)"),
        "pid": os.getpid(),
        "program": _text(program),
        "subcommand": subcommand,
        "service": service,
    }
    log = os.environ.get(LOG_VAR)
    if log:
        with open(log, "a", encoding="utf-8") as out:
            out.write(json.dumps(entry) + "\n")
    raise KeychainRefused(
        f"rlx test guard: refused {entry['program']} {subcommand} {service!r}".rstrip()
        + f" in {entry['test']}: {ADVICE}"
    )


def read_log(log: str | os.PathLike[str], start: int = 0) -> tuple[list[dict[str, Any]], int]:
    """The refusals written to ``log`` from byte ``start`` on, and where the file ends."""
    try:
        with open(log, "rb") as source:
            source.seek(start)
            data = source.read()
    except FileNotFoundError:
        return [], start
    entries = [json.loads(line) for line in data.decode("utf-8").splitlines() if line.strip()]
    return entries, start + len(data)


def failure(nodeid: str, entries: Sequence[dict[str, Any]]) -> str:
    """What a test that reached the real Keychain tool fails with: the test, each refusal."""
    ran = "; ".join(
        f"{entry.get('program')} {entry.get('subcommand')} {entry.get('service')!r}"
        f" (pid {entry.get('pid')}, {entry.get('test')})"
        for entry in entries
    )
    refused_it = "reached the real macOS Keychain tool, and the guard refused it"
    return f"{nodeid} {refused_it}: {ran}. {ADVICE}"


def _text(value: object) -> str | None:
    if value is None:
        return None
    if isinstance(value, bytes):
        return os.fsdecode(value)
    if isinstance(value, os.PathLike):
        return os.fsdecode(os.fspath(value))
    return value if isinstance(value, str) else None


def _same_file(path: str, other: str) -> bool:
    try:
        return os.path.realpath(path) == os.path.realpath(other)
    except (OSError, ValueError):
        return False
