"""The suite's guard: an audit hook that keeps every test, and every Python child it starts, away
from the developer's credentials and live homes (#52, #54).

It refuses, by raising a ``PermissionError`` before anything runs or opens:

- **the macOS Keychain tool**, ``/usr/bin/security`` (or :data:`SENTINEL`, where conftest points
  the product's ``KEYCHAIN_TOOL``). A scratch ``HOME`` does not scope the Keychain: on a
  developer's Mac, ``security find-generic-password`` reads that person's own Claude Code login.
  ``test_persona_import``'s Ctrl-C test did, through the ``aisquare`` child it starts (#52,
  tester note #3183).
- **the other credential stores' tools**, ``secret-tool`` (libsecret) and ``cmdkey`` (Windows'
  Credential Manager), by name, wherever they are. The product runs neither today (#52's audit,
  #3381); the guard is there before anyone adds one.
- **a real agent binary**: a ``claude`` or ``codex`` that resolves outside the run's stand-in
  roots (the temp dir and the checkout, where the tests write theirs). The real ``claude`` reads
  the Keychain itself, out of reach of any Python hook. The native suites, which exist to run
  the real binaries, lift this (``tests/conftest.py``'s ``real_agent_binaries``).
- **the developer's real home**: opening, listing, writing, renaming or removing anything under
  the real home's ``.claude*`` (``~/.claude``, ``~/.claude.json``, a slot's ``~/.claude-<n>``)
  and ``.codex*``, its ``.aisquare`` and ``.rlx`` (the fleet's and the hub's live home), or the
  ``CLAUDE_CONFIG_DIR``, ``CODEX_HOME``, ``AISQUARE_HOME`` or ``RLX_HOME`` the shell named
  (:func:`guarded_paths`). conftest moves ``HOME`` for every test, so nothing should look there;
  this is what proves nothing does, even a test that undoes the move. A path under one of the
  run's own roots (its temp dir, the checkout) is not refused, should one sit inside a guarded
  directory. ``os.stat`` raises no audit event, so a test may still ask whether such a path
  exists; it can't read, list or change it.
- **anything written directly in the developer's real home** (or to the home itself): a file
  or project folder made, changed, renamed or removed beside the person's own, outside the
  run's roots (manager #3718). Reading there, and writing deeper, are left to the rules above.

A program is found in what a spawn starts the way a shell would (:func:`commands`): past
``VAR=value`` assignments and wrapper prefixes (``env``, ``nohup``, ``timeout``, ``xargs``,
``exec``, ``sudo`` and the like), into a shell's ``-c`` script in any option cluster (``-c``,
``-ec``, ``-lc``, ``-xc``), ``cmd /c``, ``powershell -Command``, ``$(...)`` and backticks. A word
counts only in command position, so ``grep security notes.txt`` is not the tool. A shell script
FILE stays out of reach of any audit hook; the moved ``HOME`` is what makes that moot.

The hook is installed twice over:

- in each pytest process, by ``tests/conftest.py`` (at import, then :func:`configure`);
- in every Python child that inherits the suite's environment, by ``sitecustomize.py`` in this
  directory, which conftest puts first on ``PYTHONPATH``. The child reads what to guard from
  :data:`GUARDED_VAR`, :data:`ALLOWED_VAR` and :data:`AGENTS_VAR`.

Each refusal is one JSON line in the file named by :data:`LOG_VAR`, holding the test, the kind,
and the program and its subcommand and ``-s`` service (never a ``-w`` value) or the path. conftest
reads the file after each test and fails the test, naming it: a refusal that product code
swallows as an ``OSError`` still fails the test that caused it.

A child test that needs the Keychain read through the suite's fake sets :data:`FAKE_VAR` to the
fake's path (``tests/test_claude_keychain.py``'s ``FakeKeychain``): ``sitecustomize`` points that
child's ``claude_accounts.KEYCHAIN_TOOL`` at it. The guard's own tests aim every probe at a
stand-in ``security`` they register in :data:`STAND_IN_VAR`, refused besides the real tool and
never instead, so not even a broken guard starts the real one.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import shutil
import sys
from collections.abc import Iterable, Mapping, Sequence
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

REAL_HOME_VAR = "RLX_TESTS_REAL_HOME"
"""The developer's real home, as the run found it, for a child (whose ``HOME`` is a test's)."""

GUARDED_VAR = "RLX_TESTS_GUARDED_PATHS"
"""The paths a child refuses (:func:`guarded_paths`), joined by ``os.pathsep``."""

ALLOWED_VAR = "RLX_TESTS_ALLOWED_PATHS"
"""The run's own roots (its temp dir, the checkout), joined by ``os.pathsep``: never refused,
and where a stand-in ``claude`` or ``codex`` may live."""

AGENTS_VAR = "RLX_TESTS_REAL_AGENTS"
"""``1`` in a native suite's children: a real ``claude`` or ``codex`` may run there."""

STAND_IN_VAR = "RLX_TESTS_REFUSED_STAND_INS"
"""Stand-in ``security`` programs refused exactly as the real tool is, BESIDES it, never instead:
the guard's own tests aim every probe at one, so even a broken guard never starts the real
``/usr/bin/security`` (manager #3718, the #3632 rule). Read at each spawn, so a child inherits it.
"""

HOME_NAME_PREFIXES = (".claude", ".codex")
"""Every entry of the real home whose name starts so is Claude Code's or Codex's state."""

HOME_DIRS = (".aisquare", ".rlx")
"""The real home's rlx homes: the fleet's and the hub's live state on the developer's machine."""

REAL_DIR_VARS = (
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "AISQUARE_PLAIN_CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "AISQUARE_HOME",
    "RLX_HOME",
)
"""The directories the shell may name for Claude Code, Codex and rlx: guarded as it named them."""

CREDENTIAL_TOOLS = frozenset({"secret-tool", "cmdkey"})
AGENT_BINARIES = frozenset({"claude", "codex"})

ADVICE = (
    "no test may run the real macOS Keychain tool: use the suite's fake security"
    " (tests/test_claude_keychain.py's FakeKeychain, through monkeypatch on"
    f" claude_accounts.KEYCHAIN_TOOL; for a child process, {FAKE_VAR}=<the fake's path>)"
)
ADVICE_BY_KIND = {
    "keychain": ADVICE,
    "credential-tool": "no test may run a credential store's tool (secret-tool, cmdkey)",
    "agent": (
        "no test may run a real claude or codex: put a stand-in first on PATH (tests/fakebin.py,"
        " tests/fake_agent.py); only the native suites run the real ones"
    ),
    "home": (
        "no test may open, list or change the developer's real Claude Code, Codex or rlx state:"
        " HOME is the test's own (tests/conftest.py's isolated_user_home); name a path under"
        " tmp_path instead"
    ),
}
ADVICE_BY_KIND["home-write"] = (
    "no test may create, change, rename or remove anything directly in the developer's real"
    " home: HOME is the test's own (tests/conftest.py's isolated_user_home); write under tmp_path"
)
REACHED = {
    "keychain": "reached the real macOS Keychain tool, and the guard refused it",
    "credential-tool": "ran a credential store's tool, and the guard refused it",
    "agent": "ran a real agent binary, and the guard refused it",
    "home": "reached the developer's real home, and the guard refused it",
    "home-write": "wrote directly in the developer's real home, and the guard refused it",
}

_SPAWNS = frozenset({"subprocess.Popen", "os.posix_spawn", "os.exec", "os.spawn", "os.system"})

#: Each file event, the positions of its path arguments and of its ``dir_fd`` arguments.
_FILE_EVENTS: dict[str, tuple[tuple[int, ...], tuple[int, ...]]] = {
    "open": ((0,), ()),
    "os.listdir": ((0,), ()),
    "os.scandir": ((0,), ()),
    "os.fwalk": ((0,), (4,)),
    "os.chdir": ((0,), ()),
    "os.mkdir": ((0,), (2,)),
    "os.remove": ((0,), (1,)),
    "os.rmdir": ((0,), (1,)),
    "os.rename": ((0, 1), (2, 3)),
    "os.link": ((0, 1), (2, 3)),
    "os.symlink": ((0, 1), (2,)),
    "os.chmod": ((0,), (2,)),
    "os.chown": ((0,), (3,)),
    "os.chflags": ((0,), ()),
    "os.utime": ((0,), (3,)),
    "os.truncate": ((0,), ()),
    "os.listxattr": ((0,), ()),
    "os.getxattr": ((0,), ()),
    "os.setxattr": ((0,), ()),
    "os.removexattr": ((0,), ()),
    "os.startfile": ((0,), ()),
    "shutil.copyfile": ((0, 1), ()),
    "shutil.copymode": ((0, 1), ()),
    "shutil.copystat": ((0, 1), ()),
    "shutil.copytree": ((0, 1), ()),
    "shutil.move": ((0, 1), ()),
    "shutil.rmtree": ((0,), (1,)),
    "sqlite3.connect": ((0,), ()),
    "glob.glob": ((0,), ()),
    "glob.glob/2": ((0, 2), (3,)),
    "tempfile.mkdtemp": ((0,), ()),
    "tempfile.mkstemp": ((0,), ()),
}

#: Each event that creates, changes, renames or removes, and the positions of the paths it
#: writes: what :func:`home_write_refused` asks of. ``open`` writes when its flags say so.
_WRITES: dict[str, tuple[int, ...]] = {
    "open": (0,),
    "os.mkdir": (0,),
    "os.remove": (0,),
    "os.rmdir": (0,),
    "os.rename": (0, 1),
    "os.link": (1,),
    "os.symlink": (1,),
    "os.chmod": (0,),
    "os.chown": (0,),
    "os.chflags": (0,),
    "os.utime": (0,),
    "os.truncate": (0,),
    "os.setxattr": (0,),
    "os.removexattr": (0,),
    "shutil.copyfile": (1,),
    "shutil.copymode": (1,),
    "shutil.copystat": (1,),
    "shutil.copytree": (1,),
    "shutil.move": (0, 1),
    "shutil.rmtree": (0,),
    "sqlite3.connect": (0,),
    "tempfile.mkdtemp": (0,),
    "tempfile.mkstemp": (0,),
}
_WRITE_FLAGS = os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC | os.O_APPEND


class KeychainRefused(PermissionError):
    """Raised where the real Keychain tool (or another credential store's) would have started."""


class AgentBinaryRefused(PermissionError):
    """Raised where a real ``claude`` or ``codex`` would have started."""


class RealHomeRefused(PermissionError):
    """Raised where a test would have opened, listed or changed the developer's real state."""


_PATH_KINDS = frozenset({"home", "home-write"})
_EXCEPTIONS: dict[str, type[PermissionError]] = {
    "keychain": KeychainRefused,
    "credential-tool": KeychainRefused,
    "agent": AgentBinaryRefused,
    "home": RealHomeRefused,
    "home-write": RealHomeRefused,
}


class _State:
    installed = False
    # Normalised (``_norm``) guarded roots: a directory or file, and ``name*`` prefixes.
    roots: tuple[str, ...] = ()
    prefixes: tuple[str, ...] = ()
    starts: tuple[str, ...] = ()  # every root and prefix, for str.startswith's quick no
    allowed: tuple[str, ...] = ()
    homes: tuple[str, ...] = ()  # the real home, normalised: nothing is written directly in it
    agents_allowed = False
    home_allowed = False


def install() -> None:
    """Install the hook in this process, once: an audit hook can't be removed. A child (no
    :func:`configure` yet) takes what to guard from the environment its test gave it."""
    if not _State.installed:
        if not _State.starts and not _State.allowed:
            configure_from(os.environ)
        sys.addaudithook(_hook)
        _State.installed = True


def installed() -> bool:
    return _State.installed


def guarded_paths(real_home: str | os.PathLike[str], environ: Mapping[str, str]) -> list[str]:
    """What the guard refuses for a developer whose home is ``real_home`` and whose shell set
    ``environ``: ``<home>/.claude*`` and ``<home>/.codex*`` (a trailing ``*`` marks a name
    prefix), ``<home>/.aisquare`` and ``<home>/.rlx``, and each :data:`REAL_DIR_VARS` directory
    the shell named."""
    home = os.path.abspath(os.fspath(real_home))
    found = [os.path.join(home, name) + "*" for name in HOME_NAME_PREFIXES]
    found += [os.path.join(home, name) for name in HOME_DIRS]
    for name in REAL_DIR_VARS:
        value = environ.get(name, "")
        if value.strip():
            found.append(os.path.abspath(os.path.expanduser(value)))
    return list(dict.fromkeys(found))


def configure(
    guarded: Iterable[str],
    allowed: Iterable[str],
    *,
    real_home: str | os.PathLike[str] | None = None,
    agents_allowed: bool = False,
    home_allowed: bool = False,
) -> None:
    """What this process refuses: ``guarded`` paths (:func:`guarded_paths`'s form), never a path
    under one of ``allowed``; anything created, changed, renamed or removed directly in
    ``real_home`` (:func:`home_write_refused`); a real agent binary unless ``agents_allowed``;
    none of the paths while ``home_allowed``."""
    roots: list[str] = []
    prefixes: list[str] = []
    for entry in guarded:
        if not entry:
            continue
        is_prefix = entry.endswith("*")
        for form in _forms(entry.rstrip("*")):
            (prefixes if is_prefix else roots).append(form)
    _State.roots = tuple(dict.fromkeys(roots))
    _State.prefixes = tuple(dict.fromkeys(prefixes))
    _State.starts = _State.roots + _State.prefixes
    own = (form for entry in allowed if entry for form in _forms(entry))
    _State.allowed = tuple(dict.fromkeys(own))
    _State.homes = tuple(_forms(os.fspath(real_home))) if real_home else ()
    _State.agents_allowed = agents_allowed
    _State.home_allowed = home_allowed


def configure_from(environ: Mapping[str, str]) -> None:
    """:func:`configure` from what a test's environment says (a child's ``sitecustomize``)."""
    configure(
        _split_paths(environ.get(GUARDED_VAR, "")),
        _split_paths(environ.get(ALLOWED_VAR, "")),
        real_home=environ.get(REAL_HOME_VAR) or None,
        agents_allowed=environ.get(AGENTS_VAR) == "1",
    )


def lift(*, agents: bool | None = None, home: bool | None = None) -> tuple[bool, bool]:
    """Lift (or put back) the agent-binary refusal or the real-home one in this process; returns
    what was in force before, to put back."""
    before = (_State.agents_allowed, _State.home_allowed)
    if agents is not None:
        _State.agents_allowed = agents
    if home is not None:
        _State.home_allowed = home
    return before


def guarded_now() -> tuple[tuple[str, ...], tuple[str, ...], tuple[str, ...]]:
    """The normalised roots, prefixes and allowed roots in force (for the guard's own tests)."""
    return _State.roots, _State.prefixes, _State.allowed


def snapshot() -> dict[str, Any]:
    """What is in force now, for :func:`restore` (the guard's own tests aim it at a scratch
    "real home" and put the run's back after)."""
    names = ("roots", "prefixes", "starts", "allowed", "homes", "agents_allowed", "home_allowed")
    return {name: getattr(_State, name) for name in names}


def restore(state: dict[str, Any]) -> None:
    for name, value in state.items():
        setattr(_State, name, value)


# --- the programs a spawn starts -----------------------------------------------------------


def refused(program: object, env: object = None) -> bool:
    """Whether starting ``program`` would start the real Keychain tool or :data:`SENTINEL`. A
    bare name is looked up on the ``PATH`` it would run with, so a fake ``security`` put first
    on ``PATH`` is not refused, and neither is a fake at a path of its own."""
    name = _text(program)
    if name is None:
        return False
    tools = (REAL_TOOL, *_split_paths(os.environ.get(STAND_IN_VAR, "")))
    if name == SENTINEL or name in tools:
        return True
    if _has_separator(name):
        return any(_same_file(name, tool) for tool in tools)
    if _program_name(name) == "security":
        found = shutil.which(name, path=_env_path(env))
        return found is not None and any(_same_file(found, tool) for tool in tools)
    return False


def refusal_kind(program: object, env: object = None) -> str | None:
    """Which refusal starting ``program`` falls under, if any: ``keychain``,
    ``credential-tool`` or ``agent``."""
    name = _text(program)
    if not name:
        return None
    if refused(name, env):
        return "keychain"
    base = _program_name(name)
    if base in CREDENTIAL_TOOLS:
        return "credential-tool"
    if base in AGENT_BINARIES and not _State.agents_allowed:
        found = name if _has_separator(name) else shutil.which(name, path=_env_path(env))
        if found is not None and os.path.isfile(found) and not _is_stand_in(found):
            return "agent"
    return None


SHELLS = frozenset({"sh", "bash", "zsh", "dash", "ksh", "mksh", "fish", "ash", "busybox"})
SEPARATORS = frozenset({";", "&&", "||", "|", "&", "(", ")", "|&", ";;", "\n"})
_ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=.*", re.S)
_PUNCTUATION = ";&|()"
#: wrapper -> (its options that take a value, the positional values it takes before the command)
WRAPPERS: dict[str, tuple[frozenset[str], int]] = {
    "env": (frozenset({"-u", "--unset", "-C", "--chdir", "-S", "--split-string"}), 0),
    "nohup": (frozenset(), 0),
    "timeout": (frozenset({"-s", "--signal", "-k", "--kill-after"}), 1),
    "xargs": (frozenset({"-I", "-L", "-n", "-P", "-s", "-d", "-E", "-a"}), 0),
    "command": (frozenset(), 0),
    "exec": (frozenset({"-a"}), 0),
    "nice": (frozenset({"-n", "--adjustment"}), 0),
    "stdbuf": (frozenset({"-i", "-o", "-e"}), 0),
    "time": (frozenset({"-f", "-o"}), 0),
    "sudo": (frozenset({"-u", "-g", "-C", "-D", "-h", "-p", "-U"}), 0),
    "doas": (frozenset({"-u", "-C"}), 0),
    "caffeinate": (frozenset({"-t", "-w"}), 0),
    "builtin": (frozenset(), 0),
}


def commands(line: str, *, windows: bool = False, depth: int = 0) -> list[list[str]]:
    """Each simple command of the shell line ``line``, as the words from its command word on:
    past assignments and wrappers, and recursively into shells' ``-c`` scripts, ``cmd /c``,
    ``powershell -Command``, ``$(...)`` and backticks. ``windows`` reads it as a Windows
    command line (no backslash escapes), which is what ``subprocess`` audits there."""
    if depth > 8:
        return []
    found: list[list[str]] = []
    for inner in _substitutions(line):
        found += commands(inner, windows=windows, depth=depth + 1)
    simple: list[str] = []
    for token in [*_tokens(_without_substitutions(line), windows=windows), ";"]:
        if token in SEPARATORS or set(token) <= set(_PUNCTUATION):
            found += argv_commands(simple, windows=windows, depth=depth)
            simple = []
        else:
            simple.append(token)
    return found


def argv_commands(argv: Sequence[str], *, windows: bool = False, depth: int = 0) -> list[list[str]]:
    """The simple command an argument list runs (from its command word on, past assignments and
    wrappers), then whatever a shell's ``-c`` (or ``cmd /c``, ``powershell -Command``) in it
    runs, recursively."""
    words = list(argv)
    while words and _ASSIGNMENT.fullmatch(words[0]):
        words.pop(0)
    while words and _program_name(words[0]) in WRAPPERS:
        takes_value, positional = WRAPPERS[_program_name(words[0])]
        words.pop(0)
        while words and words[0].startswith("-") and words[0] != "-":
            option = words.pop(0)
            if option == "--":
                break
            if option in takes_value and words:
                words.pop(0)
        while words and _ASSIGNMENT.fullmatch(words[0]):  # env VAR=value
            words.pop(0)
        for _ in range(positional):
            if words:
                words.pop(0)
    if not words:
        return []
    found = [words]
    script = _shell_script(_program_name(words[0]), words[1:])
    if script is not None:  # a POSIX shell's script is a POSIX line wherever it was started
        inner_windows = _program_name(words[0]) in ("cmd", "powershell", "pwsh")
        found += commands(script, windows=inner_windows, depth=depth + 1)
    return found


def _shell_script(name: str, args: list[str]) -> str | None:
    if name in SHELLS:
        for at, arg in enumerate(args):
            if re.fullmatch(r"-[A-Za-z]*c[A-Za-z]*", arg) or arg == "--command":
                return args[at + 1] if at + 1 < len(args) else None
            if not arg.startswith(("-", "+")):
                return None  # a script FILE: out of any audit hook's reach
        return None
    if name == "cmd":
        for at, arg in enumerate(args):
            if arg.lower() in ("/c", "/k", "/r"):
                return " ".join(args[at + 1 :])
        return None
    if name in ("powershell", "pwsh"):
        for at, arg in enumerate(args):
            if arg.lower() in ("-command", "-c"):
                return " ".join(args[at + 1 :])
        return None
    return None


def _tokens(line: str, *, windows: bool) -> list[str]:
    try:
        lexer = shlex.shlex(line, posix=not windows, punctuation_chars=_PUNCTUATION)
        lexer.whitespace_split = True
        lexer.commenters = ""
        tokens = list(lexer)
    except ValueError:  # unbalanced quotes: words as the whitespace has them
        tokens = line.split()
    if windows:
        tokens = [_unquoted(token) for token in tokens]
    return tokens


def _unquoted(token: str) -> str:
    return token[1:-1] if len(token) > 1 and token[0] == token[-1] == '"' else token


def _substitutions(line: str) -> list[str]:
    """The text inside each ``$(...)`` (balanced) and `` `...` `` of ``line``."""
    found: list[str] = []
    at = 0
    while (start := line.find("$(", at)) >= 0:
        end = _closing(line, start + 1)
        found.append(line[start + 2 : end])
        at = end + 1
    found += re.findall(r"`([^`]*)`", line)
    return found


def _without_substitutions(line: str) -> str:
    out: list[str] = []
    at = 0
    while (start := line.find("$(", at)) >= 0:
        out.append(line[at:start] + "SUBST")
        at = _closing(line, start + 1) + 1
    out.append(line[at:])
    return re.sub(r"`[^`]*`", "SUBST", "".join(out))


def _closing(line: str, opening: int) -> int:
    depth = 0
    for at in range(opening, len(line)):
        if line[at] == "(":
            depth += 1
        elif line[at] == ")":
            depth -= 1
            if depth == 0:
                return at
    return len(line)


def _spawned(event: str, args: tuple[Any, ...]) -> tuple[list[list[str]], object]:
    """Each command a spawn event starts (from its command word on), and its environment."""
    if event == "os.system":
        return commands(_text(args[0]) or "", windows=os.name == "nt"), None
    if event == "subprocess.Popen":
        executable, argv, _cwd, env = args
    elif event == "os.spawn":
        _mode, executable, argv, env = args
    else:  # os.posix_spawn, os.exec: (path, argv, env)
        executable, argv, env = args
    if isinstance(argv, (list, tuple)):
        found = argv_commands([_text(word) or "" for word in argv])
    else:  # Windows' subprocess audits one command line
        found = commands(_text(argv) or "", windows=os.name == "nt")
    program = _text(executable)
    if program:  # what really starts, whatever its argv[0] says
        if found and _program_name(found[0][0]) == _program_name(program):
            found[0] = [program, *found[0][1:]]
        else:
            found.insert(0, [program, *(found[0][1:] if found else [])])
    return found, env


def _hook(event: str, args: tuple[Any, ...]) -> None:
    if event in _SPAWNS:
        started, env = _spawned(event, args)
        for argv in started:
            kind = refusal_kind(argv[0], env)
            if kind is not None:
                _refuse(kind, program=argv[0], argv=argv)
    elif (_State.starts or _State.homes) and not _State.home_allowed:
        where = _FILE_EVENTS.get(event)
        if where is not None:
            _check_paths(event, args, *where)


# --- the real home -------------------------------------------------------------------------


def _check_paths(
    event: str, args: tuple[Any, ...], paths: tuple[int, ...], dir_fds: tuple[int, ...]
) -> None:
    relative_to_fd = any(at < len(args) and isinstance(args[at], int) for at in dir_fds)
    writes = _WRITES.get(event, ())
    if event == "open" and not _opens_to_write(args):
        writes = ()
    for at in paths:
        if at >= len(args):
            continue
        path = _text(args[at])
        if not path or (relative_to_fd and not os.path.isabs(path)):
            continue
        if home_refused(path):
            _refuse("home", event=event, path=path)
        if at in writes and home_write_refused(path):
            _refuse("home-write", event=event, path=path)


def _opens_to_write(args: tuple[Any, ...]) -> bool:
    """Whether an ``open`` event's mode or flags create, truncate, append or write."""
    mode = args[1] if len(args) > 1 else None
    flags = args[2] if len(args) > 2 else None
    if isinstance(mode, str) and any(letter in mode for letter in "wax+"):
        return True
    return isinstance(flags, int) and bool(flags & _WRITE_FLAGS)


def home_write_refused(path: str | os.PathLike[str]) -> bool:
    """Whether writing ``path`` creates, changes, renames or removes something DIRECTLY in the
    developer's real home (or the home itself), outside the run's own roots (manager #3718): a
    project folder made beside the person's own, a file dropped in ``~``. Deeper paths are the
    tripwire's guarded roots' business (:func:`home_refused`)."""
    if not _State.homes:
        return False
    norm = _norm(os.fspath(path))
    if norm is None:
        return False
    if norm not in _State.homes and os.path.dirname(norm) not in _State.homes:
        return False
    return not any(_under(norm, own) for own in _State.allowed)


def home_refused(path: str | os.PathLike[str]) -> bool:
    """Whether ``path`` is under a guarded root, and not under one of the run's own roots that
    lies inside that guarded root (a temp dir inside ``~/.rlx``, say). A guarded root inside a
    run root stays guarded: a shell's ``AISQUARE_HOME`` under ``/tmp`` is still the shell's."""
    norm = _norm(os.fspath(path))
    if norm is None or not norm.startswith(_State.starts):
        return False
    hits = [root for root in _State.roots if _under(norm, root)]
    hits += [prefix for prefix in _State.prefixes if norm.startswith(prefix)]
    for hit in hits:
        inside = [own for own in _State.allowed if own.startswith(hit) and _under(norm, own)]
        if not inside:
            return True
    return False


def _under(norm: str, root: str) -> bool:
    return norm == root or norm.startswith(root.rstrip(os.sep) + os.sep)


def _is_stand_in(path: str) -> bool:
    norm = _norm(os.path.realpath(path))
    return norm is not None and any(_under(norm, root) for root in _State.allowed)


# --- the record ----------------------------------------------------------------------------


def _refuse(
    kind: str,
    *,
    program: object = None,
    argv: Sequence[object] = (),
    event: str = "",
    path: str = "",
) -> None:
    entry: dict[str, Any] = {
        "test": os.environ.get("PYTEST_CURRENT_TEST", "(outside a test)"),
        "pid": os.getpid(),
        "kind": kind,
    }
    if kind in _PATH_KINDS:
        entry.update(event=event, path=path, program=None, subcommand="", service="")
        said = f"{event} {path}"
    else:
        words = [_text(word) or "" for word in argv]
        subcommand = words[1] if len(words) > 1 else ""
        service = words[words.index("-s") + 1] if kind == "keychain" and "-s" in words[:-1] else ""
        entry.update(program=_text(program), subcommand=subcommand, service=service)
        said = f"{entry['program']} {subcommand} {service!r}".rstrip()
    log = os.environ.get(LOG_VAR)
    if log:
        with open(log, "a", encoding="utf-8") as out:
            out.write(json.dumps(entry) + "\n")
    raise _EXCEPTIONS[kind](
        f"rlx test guard: refused {said} in {entry['test']}: {ADVICE_BY_KIND[kind]}"
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
    """What a test the guard refused fails with: the test, and each refusal, by kind."""
    said: list[str] = []
    for kind in dict.fromkeys(entry.get("kind", "keychain") for entry in entries):
        ran = "; ".join(
            (
                f"{entry.get('event')} {entry.get('path')}"
                if kind in _PATH_KINDS
                else f"{entry.get('program')} {entry.get('subcommand')} {entry.get('service')!r}"
            )
            + f" (pid {entry.get('pid')}, {entry.get('test')})"
            for entry in entries
            if entry.get("kind", "keychain") == kind
        )
        said.append(f"{nodeid} {REACHED[kind]}: {ran}. {ADVICE_BY_KIND[kind]}")
    return " ".join(said)


# --- helpers -------------------------------------------------------------------------------


def _forms(path: str) -> list[str]:
    """``path`` normalised as spelt and as resolved (``/var`` is ``/private/var`` on a Mac)."""
    found = []
    for form in (path, _realpath(path)):
        norm = _norm(form) if form else None
        if norm and norm not in found:
            found.append(norm)
    return found


def _norm(path: str) -> str | None:
    try:
        norm = os.path.normcase(os.path.abspath(path))
    except (OSError, ValueError):  # a cwd that's gone, a NUL
        return None
    return norm.casefold() if sys.platform == "darwin" else norm


def _realpath(path: str) -> str | None:
    try:
        return os.path.realpath(path)
    except (OSError, ValueError):
        return None


def _split_paths(value: str) -> list[str]:
    return [part for part in value.split(os.pathsep) if part]


def _program_name(word: str) -> str:
    name = re.split(r"[\\/]", word)[-1].lower()
    for suffix in (".exe", ".cmd", ".bat", ".ps1"):
        if name.endswith(suffix):
            return name[: -len(suffix)]
    return name


def _has_separator(name: str) -> bool:
    return os.sep in name or bool(os.altsep and os.altsep in name)


def _env_path(env: object) -> str | None:
    if isinstance(env, dict):
        path = _text(env.get("PATH", env.get(b"PATH")))
        if path is not None:
            return path
    return os.environ.get("PATH")


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
