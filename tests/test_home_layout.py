"""rlx's home moves to ``~/.rlx``, laid out as docs/home-layout.md says; an existing home keeps
working as it is (#30, H1; the manager's decision: the variable says where, the folder says how).

Every test here names its own user home (``HOME``) and clears both home variables, or names one,
so nothing reads or writes this machine's ``~/.aisquare`` or ``~/.rlx``.
"""

from __future__ import annotations

import ast
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import claude_accounts as claude_accounts_core
from aisquare.core import paths
from aisquare.core.store import store_session
from aisquare.services import claude_accounts as accounts_service
from tests.old_home import NEW_PLACES, OLD_FILES, OLD_PLACES, make_old_home

REPO = Path(__file__).resolve().parents[1]

#: The old layout's names a module could join to the home itself: distinctive enough that a
#: join with one is the home's (``log``, ``instances`` or ``credentials`` are not).
OLD_NAMES = frozenset(
    {
        "context.db",
        "claude-accounts",
        "codex-accounts",
        "launch-seals",
        "fleet-tmux.conf",
        "remote.json",
        "remote-audit.log",
        "remote-dist",
        "remote-needs.json",
        "remote-push.json",
        "ngrok-authtoken",
        "usage-sweep.lock",
        "usage-share.lock",
        "active-ping.lock",
    }
)
ALLOWED = {
    REPO / "src" / "aisquare" / "core" / "paths.py",
    REPO / "tests" / "old_home.py",
    Path(__file__).resolve(),
}


@pytest.fixture
def user_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    """A user home of the test's own, with neither home variable set: the default home is
    found in it (``~/.rlx``, else ``~/.aisquare``)."""
    home = tmp_path / "user"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("USERPROFILE", str(home))
    monkeypatch.delenv(paths.HOME_ENV_VAR, raising=False)
    monkeypatch.delenv(paths.RLX_HOME_ENV, raising=False)
    yield home


def _joins(tree: ast.AST) -> Iterator[tuple[int, str]]:
    for node in ast.walk(tree):
        if (
            isinstance(node, ast.BinOp)
            and isinstance(node.op, ast.Div)
            and isinstance(node.right, ast.Constant)
            and node.right.value in OLD_NAMES
        ):
            yield node.lineno, str(node.right.value)
        if (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and node.func.attr == "joinpath"
        ):
            for arg in node.args:
                if isinstance(arg, ast.Constant) and arg.value in OLD_NAMES:
                    yield node.lineno, str(arg.value)


def test_no_module_and_no_test_joins_an_old_layout_name_to_the_home() -> None:
    """The layout lives in core/paths.py alone (the manager's condition 2 on #30): a module or
    a test that joins ``context.db`` (or another old-layout name) to a folder itself builds
    the old layout in a new home. It asks ``paths`` for the thing by name instead."""
    found: list[str] = []
    for root in (REPO / "src", REPO / "tests"):
        for module in sorted(root.rglob("*.py")):
            if module.resolve() in ALLOWED:
                continue
            tree = ast.parse(module.read_text(encoding="utf-8"), filename=str(module))
            found += [f"{module.relative_to(REPO)}:{line} {name}" for line, name in _joins(tree)]
    assert found == []


# --- where the home is, and how it is laid out ---------------------------------------------------


def test_a_new_user_gets_rlx_in_the_new_layout(user_home: Path) -> None:
    root, layout = paths.home_of()
    assert (root, layout) == (user_home / ".rlx", paths.NEW)
    for name, parts in NEW_PLACES.items():
        assert getattr(paths, name)() == root.joinpath(*parts), name
    assert not root.exists(), "found, not made: nothing is made by asking"


def test_an_existing_aisquare_home_keeps_its_layout(user_home: Path) -> None:
    old = make_old_home(user_home)
    root, layout = paths.home_of()
    assert (root, layout) == (old, paths.OLD)
    for name, parts in OLD_PLACES.items():
        assert getattr(paths, name)() == old.joinpath(*parts), name
    (user_home / ".rlx").mkdir()
    assert paths.home_of() == (user_home / ".rlx", paths.NEW), "~/.rlx, once there, wins"


@pytest.mark.parametrize("variable", [paths.RLX_HOME_ENV, paths.HOME_ENV_VAR])
def test_either_variable_names_the_home_and_its_contents_say_how(
    user_home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, variable: str
) -> None:
    named = tmp_path / "named"
    monkeypatch.setenv(variable, str(named))
    assert paths.home_of() == (named, paths.NEW), "nothing there yet: a new home"
    named.mkdir()
    assert paths.home_of() == (named, paths.NEW), "empty: a new home"
    (named / "launch-seals").mkdir()
    assert paths.home_of() == (named, paths.OLD), "an old layout's folder: an old home"
    (named / "rlx.db").write_text("")
    assert paths.home_of() == (named, paths.NEW), "rlx.db: a new home, whatever else is there"


def test_rlx_home_wins_over_aisquare_home(
    user_home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv(paths.HOME_ENV_VAR, str(tmp_path / "old-name"))
    monkeypatch.setenv(paths.RLX_HOME_ENV, str(tmp_path / "new-name"))
    assert paths.aisquare_home() == tmp_path / "new-name"
    monkeypatch.delenv(paths.RLX_HOME_ENV)
    assert paths.aisquare_home() == tmp_path / "old-name"


def test_a_window_carries_the_home_by_both_names(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Every agent the fleet starts is handed the home; RLX_HOME, which wins, travels with it,
    and is unset for the window when this process has none, so a tmux server's stale one
    cannot name another home."""
    _, env = accounts_service.carry_environment(
        ["claude"], {paths.HOME_ENV_VAR: str(tmp_path / "home")}, cwd=tmp_path
    )
    assert env.get(paths.HOME_ENV_VAR) == str(tmp_path / "home")
    assert paths.RLX_HOME_ENV in accounts_service.carried_vars()


# --- the commands, in a new home and in an old one -----------------------------------------------

COMMANDS = [["doctor"], ["fleet", "ls"], ["accounts", "list"]]


def test_a_new_users_first_commands_make_rlx_and_never_aisquare(user_home: Path) -> None:
    """Acceptance 1: the first command makes ``~/.rlx``, laid out as documented; nothing ever
    makes ``~/.aisquare``. Bare ``rlx`` (usage, at no terminal) makes neither."""
    runner = CliRunner()
    bare = runner.invoke(app, [])
    assert bare.exit_code == 2
    assert not (user_home / ".aisquare").exists()
    for command in COMMANDS:
        runner.invoke(app, command)
        assert not (user_home / ".aisquare").exists(), command
    with store_session():
        pass
    home = user_home / ".rlx"
    assert (home / "rlx.db").is_file()
    for parts in paths.NEW_LAYOUT_FOLDERS:
        assert home.joinpath(*parts).is_dir(), parts
    assert not (home / "context.db").exists() and not (home / "log").exists()


def test_an_old_home_runs_every_command_as_before_on_its_own_files(user_home: Path) -> None:
    """Acceptance 2: an existing ``~/.aisquare`` with every kind of file and no ``~/.rlx``:
    each command reads its old paths, the store is its ``context.db``, nothing is moved,
    and no ``~/.rlx`` appears."""
    old = make_old_home(user_home)
    runner = CliRunner()
    for command in COMMANDS:
        result = runner.invoke(app, command)
        assert "Traceback" not in result.output, (command, result.output)
    with store_session():
        pass
    assert (old / "context.db").is_file() and not (old / "rlx.db").exists()
    assert not (user_home / ".rlx").exists()
    for name in OLD_FILES:
        assert (old / name).exists(), name
    assert claude_accounts_core.accounts_root().parent == old


@pytest.mark.skipif(sys.platform == "win32", reason="POSIX homes")
def test_statusline_finds_the_home_by_the_same_rule(
    user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from aisquare import statusline

    assert statusline.home() == user_home / ".rlx"
    make_old_home(user_home)
    assert statusline.home() == user_home / ".aisquare"
    monkeypatch.setenv(paths.RLX_HOME_ENV, str(user_home / "elsewhere"))
    assert statusline.home() == user_home / "elsewhere"
