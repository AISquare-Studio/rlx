"""The move of a home from before 0.10.2 to ``~/.rlx`` in the new layout (#31, H2).

Every test runs in a user home of its own (``HOME``), with neither home variable set unless it
names one, and on macOS's terms as far as the logins go: ``security`` is a FAKE, a script that
keeps its items in a table beside it and logs every argument list it is given. Nothing here
reads or writes this machine's ``~/.aisquare``, ``~/.rlx`` or login Keychain (the contract's
boundary on #31). POSIX only where the fake runs: it is a script, as the real one is a binary.
"""

from __future__ import annotations

import contextlib
import errno
import io
import json
import os
import runpy
import shutil
import sqlite3
import subprocess
import sys
import uuid
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import claude_accounts as accounts_core
from aisquare.core import launch_seal, locking, paths
from aisquare.core import program as prog
from aisquare.core.state import get_state, set_state
from aisquare.core.store import store_session
from aisquare.models import FleetAgent, LaunchSpec, ProjectInfo, TeamSession
from aisquare.services import diagnostics, home_migration, install_route, lifecycle, updates, watch
from aisquare.services import fleet as fleet_service
from aisquare.services.install_route import Captured, Facts, InstallRoute, LatestRelease
from aisquare.services.lifecycle import UpgradePlan
from tests.installer_seams import no_real_installer  # noqa: F401 — autouse, applied by import
from tests.old_home import OLD_FILES, make_old_home
from tests.test_claude_accounts import _sign_in
from tests.test_claude_keychain import _item
from tests.test_lifecycle_upgrade import Machine, Tool, machine, tool  # noqa: F401
from tests.test_ui_doctor_handoff import Terminal, terminal  # noqa: F401

POSIX_ONLY = pytest.mark.skipif(
    sys.platform == "win32", reason="the fake `security` is a POSIX script, as the real one is"
)
pytestmark = POSIX_ONLY

NOT_FOUND = (
    "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.\n"
)

SECURITY = """#!{python} -I
import json, pathlib, shlex, sys
here = pathlib.Path(__file__).parent
table = here / 'items.json'
def key(a):
    return a[a.index('-s') + 1] + '\\t' + a[a.index('-a') + 1]
def printed(data):
    if all(0x20 <= b < 0x7f for b in data):
        return data.decode('ascii') + '\\n'
    return data.hex() + '\\n'
def main(args, stdin, out, err):
    with open(here / 'calls.jsonl', 'a') as log:
        log.write(json.dumps(args) + '\\n')
    items = json.loads(table.read_text())
    def run(a, through):
        verb = a[0]
        if verb == 'find-generic-password':
            item = items.get(key(a))
            if item is None:
                err.write({not_found!r})
                return 44
            if item.get('exit'):
                err.write(item.get('stderr', ''))
                return item['exit']
            out.write(item['stdout'])
            return 0
        if verb == 'add-generic-password':
            data = bytes.fromhex(a[a.index('-X') + 1])
            if (here / 'corrupt').exists():
                data = data[:-1] + b'!'
            items[key(a)] = {{'stdout': printed(data), 'through': through}}
            table.write_text(json.dumps(items))
            return 0
        if verb == 'delete-generic-password':
            if items.pop(key(a), None) is None:
                err.write({not_found!r})
                return 44
            table.write_text(json.dumps(items))
            return 0
        return 2
    if args == ['-i']:
        for line in stdin.read().splitlines():
            run(shlex.split(line), 'stdin')
        return 0  # as `security -i` does, whatever a command it ran answered
    return run(args, 'argv')
if __name__ == '__main__':
    sys.exit(main(sys.argv[1:], sys.stdin, sys.stdout, sys.stderr))
"""


class FakeSecurity:
    """``security`` as far as an item's find, add (``-i`` on stdin, or argv) and delete go."""

    def __init__(self, root: Path) -> None:
        root.mkdir(parents=True)
        self.root = root
        self.tool = root / "security"
        self.table = root / "items.json"
        self.log = root / "calls.jsonl"
        self.table.write_text("{}", encoding="utf-8")
        self.tool.write_text(
            SECURITY.format(python=sys.executable, not_found=NOT_FOUND), encoding="utf-8"
        )
        self.tool.chmod(0o755)

    def items(self) -> dict[str, dict[str, Any]]:
        loaded: dict[str, dict[str, Any]] = json.loads(self.table.read_text(encoding="utf-8"))
        return loaded

    def put(self, service: str, text: str, **extra: Any) -> None:
        table = self.items()
        table[f"{service}\ttester"] = {"stdout": text + "\n", **extra}
        self.table.write_text(json.dumps(table), encoding="utf-8")

    def has(self, service: str) -> bool:
        return f"{service}\ttester" in self.items()

    def calls(self) -> list[list[str]]:
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines()]

    def in_process(self, patch: pytest.MonkeyPatch) -> None:
        """The script's own ``main`` answers a run of it in this process, given the same
        arguments and stdin and logging them alike. Everything else still runs.

        A test that runs the move a hundred times otherwise starts a Python per item read,
        written or deleted: a thousand starts, which put a whole CI shard past its time
        limit (run 45857783948)."""
        script = runpy.run_path(str(self.tool), run_name="fake_security")
        fake = str(self.tool)
        real_run = subprocess.run

        def run(args: Any, *rest: Any, **kwargs: Any) -> Any:
            if not isinstance(args, list | tuple) or not args or str(args[0]) != fake:
                return real_run(args, *rest, **kwargs)
            given: bytes | None = kwargs.get("input")
            stdin = io.StringIO("" if given is None else given.decode("utf-8"))
            out, err = io.StringIO(), io.StringIO()
            code = script["main"]([str(arg) for arg in args[1:]], stdin, out, err)
            return subprocess.CompletedProcess(
                args, code, out.getvalue().encode("utf-8"), err.getvalue().encode("utf-8")
            )

        patch.setattr(subprocess, "run", run)


@pytest.fixture
def user_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    home = tmp_path / "user"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("USERPROFILE", str(home))
    monkeypatch.delenv(paths.HOME_ENV_VAR, raising=False)
    monkeypatch.delenv(paths.RLX_HOME_ENV, raising=False)
    monkeypatch.delenv(accounts_core.SECURE_STORAGE_VAR, raising=False)
    monkeypatch.delenv("TMUX_PANE", raising=False)
    yield home


@pytest.fixture
def keychain(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[FakeSecurity]:
    fake = FakeSecurity(tmp_path / "keychain")
    monkeypatch.setattr(accounts_core, "keychain_platform", lambda: True)
    monkeypatch.setattr(accounts_core, "KEYCHAIN_TOOL", str(fake.tool))
    monkeypatch.setenv("USER", "tester")
    accounts_core.forget_keychain_reads()
    yield fake
    accounts_core.forget_keychain_reads()


MOVE = "~/.aisquare → ~/.rlx"
UI_FACTS = Facts(
    prefix=Path("/tools/aisquare-cli"),
    base_prefix=Path("/usr"),
    executable=Path("/tools/aisquare-cli/bin/python"),
    platform="darwin",
    python_version="3.12",
)
SESSION = str(uuid.UUID(int=7))
NOW = datetime(2026, 11, 25, 12, 0, tzinfo=UTC)


@pytest.fixture
def old_home(user_home: Path, keychain: FakeSecurity, tmp_path: Path) -> Path:
    """An old home with every kind of file, two Claude slots signed in through the fake
    Keychain and a Codex slot, a store whose rows name paths in it, and launch records."""
    root = make_old_home(user_home)
    for slot in (2, 4):
        account = accounts_core._managed(slot)
        _sign_in(account, f"slot{slot}@example.com", credentials=False)
        keychain.put(accounts_core.keychain_service(account), _item(f"token-{slot}"))
    codex = _slot(3, "codex")
    (codex / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": "sk-test"}), encoding="utf-8")
    transcript = _slot(2) / "projects" / "-captain" / f"{SESSION}.jsonl"
    transcript.parent.mkdir(parents=True)
    transcript.write_text("{}\n", encoding="utf-8")
    captain = paths.captain_dir()
    captain.mkdir(parents=True)
    app_root = tmp_path / "app"
    (app_root / ".aisquare-worktrees" / "coder").mkdir(parents=True)
    slot2, slot4 = str(_slot(2)), str(_slot(4))
    with store_session() as store:
        store.ensure_project(ProjectInfo(id="prj_home", root=captain))
        store.ensure_project(ProjectInfo(id="prj_app", root=app_root))
        rows = [
            ("agt-captain", "prj_home", captain, captain, False, 2, slot2),
            (
                "agt-coder",
                "prj_app",
                app_root,
                app_root / ".aisquare-worktrees" / "coder",
                True,
                4,
                slot4,
            ),
            ("agt-stray", "prj_app", tmp_path / "elsewhere", app_root, False, 4, slot4),
        ]
        for agent_id, project, project_root, cwd, worktree, slot, home in rows:
            store.upsert_fleet_agent(
                FleetAgent(
                    id=agent_id,
                    project_id=project,
                    label=agent_id.removeprefix("agt-"),
                    role="coder",
                    pane_id="%1",
                    cwd=cwd,
                    worktree=worktree,
                    account_slot=slot,
                    created_at=NOW,
                    ended_at=NOW,  # stopped: a quiet home
                )
            )
            record = launch_seal.LaunchRecord(
                id=agent_id,
                project_id=project,
                project_root=str(project_root),
                role="coder",
                agent="claude-code",
                binary="claude",
                cwd=str(cwd),
                worktree=worktree,
                account_slot=slot,
                account_home=home,
                spec=LaunchSpec(binary="claude"),
                session=launch_seal.SessionFacts(
                    id=SESSION, agent="claude-code", transcript=str(transcript), home=home
                ),
            )
            assert launch_seal.write(record) is None
            window = launch_seal.WindowPlace(tmux_socket="asq", pane_id="%1", created_at=NOW)
            assert launch_seal.placed(agent_id, window) is None
        store.upsert_claude_account(2, Path(slot2))
        store.upsert_claude_account(4, Path(slot4))
        store.upsert_agent_account("codex", 3, codex)
        store.upsert_session(
            TeamSession(
                id=SESSION,
                project_id="prj_home",
                started_at=NOW,
                last_seen_at=NOW,
                transcript_path=str(transcript),
                account=slot2,
            )
        )
    with contextlib.closing(sqlite3.connect(str(root / paths.OLD_STORE))) as connection, connection:
        connection.execute(
            "INSERT INTO team_event (id, project_id, kind, text, created_at) "
            "VALUES ('evt-1', 'prj_home', 'note', ?, ?)",
            (f"watch show {root}/watch/x.json", NOW.isoformat()),
        )
    paths.agents_registry_path().write_text(
        json.dumps({"connected": ["claude-code"], "connections": {"claude-code": [slot2, slot4]}}),
        encoding="utf-8",
    )
    accounts_core.forget_keychain_reads()
    return root


def _slot(slot: int, agent: str = "claude-code") -> Path:
    """Where the home keeps ``agent``'s account ``slot``, in its layout now (``core.paths``)."""
    return accounts_core._managed(slot, agent).config_dir


def _tree(root: Path) -> dict[str, bytes]:
    """Every regular file under ``root`` (no link followed), by its path, with its bytes."""
    found: dict[str, bytes] = {}
    for directory, _dirs, files in os.walk(root):
        for name in files:
            path = Path(directory) / name
            found[path.relative_to(root).as_posix()] = path.read_bytes()
    return found


def _moved(name: str) -> str:
    """Where the new layout keeps ``name`` (a path under an old home)."""
    first, _, rest = name.partition("/")
    place = paths.new_place(first) or (first,)
    return "/".join((*place, rest) if rest else place)


def _store_rows(root: Path, name: str) -> dict[str, list[tuple[Any, ...]]]:
    with contextlib.closing(sqlite3.connect(str(root / name))) as connection:
        tables = [
            t for (t,) in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")
        ]
        return {t: sorted(map(tuple, connection.execute(f'SELECT * FROM "{t}"'))) for t in tables}


def _accepted() -> set[str]:
    return set(home_migration._replayable_rows())


# --- acceptance 1: the move ----------------------------------------------------------------------


def test_an_old_home_moves_to_rlx_in_the_new_layout_signed_in_sealed_and_linked(
    old_home: Path, user_home: Path, keychain: FakeSecurity
) -> None:
    before = _tree(old_home)
    old_services = {
        slot: accounts_core.keychain_service(accounts_core._managed(slot)) for slot in (2, 4)
    }
    assert _accepted() == {"agt-captain", "agt-coder"}  # the stray's project root is not its own

    result = CliRunner().invoke(app, ["move-home"])

    assert result.exit_code == 0, result.output
    new = user_home / ".rlx"
    assert (user_home / ".aisquare").is_symlink()
    assert os.readlink(user_home / ".aisquare") == str(new)
    assert paths.home_of() == (new, paths.NEW)
    after = _tree(new)
    for name, body in before.items():  # every file, at its new place, as it was
        if name in ("agents.json",) or name.startswith(("launch-seals/", "context.db")):
            continue  # rewritten; checked below
        assert after.get(_moved(name)) == body, name
    for name in OLD_FILES:
        assert (new / _moved(name)).exists(), name
    assert not any(
        (new / name).exists() for name in ("context.db", "log", "launch-seals", "instances")
    )
    # The store's paths resolve, every one.
    with store_session() as store:
        home_project = store.get_project("prj_home")
        assert home_project is not None and home_project.root == new / "captain"
        coder = store.get_fleet_agent("agt-coder")
        assert coder is not None and coder.cwd.is_dir()
        rows = _store_rows(new, "rlx.db")
    assert str(old_home) not in json.dumps(
        {t: rows[t] for t in rows if t not in ("team_event",)}, default=str
    )
    # Seals: re-issued for the new paths, and the same rows replay; the stray still does not.
    assert _accepted() == {"agt-captain", "agt-coder"}
    captain = launch_seal.read("agt-captain")
    assert isinstance(captain, launch_seal.LaunchRecord)
    assert captain.account_home == str(new / "accounts" / "claude" / "2")
    assert captain.session is not None and Path(captain.session.transcript or "").is_file()
    assert captain.window is not None and captain.window.pane_id == "%1"
    # Each slot still reads as signed in; the logins moved to their new Keychain names.
    accounts_core.forget_keychain_reads()
    for account in accounts_core.all_managed_accounts():
        assert accounts_core.signed_in(account, fresh=True), account
        assert account.config_dir.is_relative_to(new / "accounts")
    for slot, old in old_services.items():
        assert not keychain.has(old), slot
        assert keychain.has(accounts_core.keychain_service(accounts_core._managed(slot)))
    # The backup is in the new home, and the prose that names the old home is counted.
    assert (new / "backups" / "home-migration" / paths.OLD_STORE).is_file()
    assert "team_event.text: 1" in result.output
    assert home_migration.DOWNGRADE_GAP in result.output.replace("\n", " ")


def test_no_secret_is_ever_on_a_command_line_and_the_order_is_copy_verify_then_delete(
    old_home: Path, keychain: FakeSecurity
) -> None:
    olds = [accounts_core.keychain_service(accounts_core._managed(slot)) for slot in (2, 4)]
    assert home_migration.move_home() is not None

    calls = keychain.calls()
    secrets = ("token-2", "token-4", _item("token-2").encode().hex()[:40])
    assert not [call for call in calls if any(s in " ".join(call) for s in secrets)]
    adds = [at for at, call in enumerate(calls) if call == ["-i"]]
    deletes = [at for at, call in enumerate(calls) if call[0] == "delete-generic-password"]
    assert len(adds) == 2 and len(deletes) == 2
    assert max(adds) < min(deletes), "every copy is made before any old item goes"
    for at in adds:  # each copy read back by Claude Code's own lookup before the next step
        verify = calls[at + 1]
        assert verify[:2] == ["find-generic-password", "-a"] and verify[3] == "-w"
    assert sorted(call[-1] for call in (calls[at] for at in deletes)) == sorted(olds)
    for item in keychain.items().values():
        assert item.get("through", "stdin") == "stdin"


# --- acceptance 2: a crash at each step, then a re-run ------------------------------------------


class Crash(BaseException):
    """The process dying: nothing after it runs, no roll-back, the journal as it was."""


def _crash_in(monkeypatch: pytest.MonkeyPatch, step: str, when: str) -> None:
    real = home_migration._MOVE_STEPS[step]

    def crashing(journal: home_migration.Journal) -> None:
        if when == "after":
            real(journal)
        raise Crash(step)

    monkeypatch.setitem(home_migration._MOVE_STEPS, step, crashing)


STEPS = [step for step in home_migration.STEPS if step != "done"]


@pytest.mark.parametrize("when", ["before", "after"])
@pytest.mark.parametrize("step", STEPS)
def test_a_crash_at_each_step_then_a_rerun_finishes_the_move_and_loses_no_file(
    old_home: Path,
    user_home: Path,
    keychain: FakeSecurity,
    monkeypatch: pytest.MonkeyPatch,
    step: str,
    when: str,
) -> None:
    before = _tree(old_home)
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, step, when)
        with pytest.raises(Crash):
            home_migration.move_home()

    assert home_migration.move_home() is not None

    new = user_home / ".rlx"
    assert paths.home_of() == (new, paths.NEW) and (user_home / ".aisquare").is_symlink()
    after = _tree(new)
    for name, body in before.items():
        if name == "agents.json" or name.startswith(("launch-seals/", "context.db")):
            assert _moved(name) in after, name
            continue
        assert after.get(_moved(name)) == body, name
    assert _accepted() == {"agt-captain", "agt-coder"}
    accounts_core.forget_keychain_reads()
    assert all(accounts_core.signed_in(a, fresh=True) for a in accounts_core.all_managed_accounts())


@pytest.mark.parametrize("at", [1, 5, -1], ids=["first-rename", "a-middle-one", "the-root"])
def test_a_crash_between_two_renames_resumes_or_rolls_back(
    old_home: Path,
    user_home: Path,
    keychain: FakeSecurity,
    monkeypatch: pytest.MonkeyPatch,
    at: int,
) -> None:
    before = _tree(old_home)
    found = home_migration.planned_move()
    assert found is not None
    renames = len(found.renames) + 1  # and the root's
    stop = at if at > 0 else renames
    made: list[int] = []
    real = home_migration._rename_once

    def rename(source: Path, target: Path) -> None:
        made.append(1)
        if len(made) == stop:
            real(source, target)
            raise Crash("between two renames")
        real(source, target)

    rows = _store_rows(old_home, paths.OLD_STORE)
    with pytest.MonkeyPatch.context() as crash:
        crash.setattr(home_migration, "_rename_once", rename)
        with pytest.raises(Crash):
            home_migration.move_home()

    assert home_migration.roll_back_move() is True

    assert not (user_home / ".rlx").exists()
    assert _same_but_store(_tree(old_home), before)
    assert _store_rows(old_home, paths.OLD_STORE) == rows
    assert paths.home_of() == (old_home, paths.OLD)
    for slot in (2, 4):  # the copies gone, the logins where they were
        account = accounts_core._managed(slot)
        assert keychain.has(accounts_core.keychain_service(account))
    assert len(keychain.items()) == 2


@pytest.mark.parametrize("step", STEPS[: STEPS.index(home_migration.FORWARD_ONLY)])
def test_a_crash_before_the_old_logins_go_rolls_back_to_the_home_as_it_was(
    old_home: Path,
    user_home: Path,
    keychain: FakeSecurity,
    monkeypatch: pytest.MonkeyPatch,
    step: str,
) -> None:
    before, items = _tree(old_home), keychain.items()
    rows = _store_rows(old_home, paths.OLD_STORE)
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, step, "after")
        with pytest.raises(Crash):
            home_migration.move_home()

    assert home_migration.roll_back_move() is True

    assert not os.path.lexists(user_home / ".rlx")
    assert not (user_home / ".aisquare").is_symlink()
    assert _same_but_store(_tree(old_home), before)
    assert _store_rows(old_home, paths.OLD_STORE) == rows
    assert keychain.items() == items
    assert home_migration.planned_move() is not None  # and it can be run again


def _same_but_store(after: dict[str, bytes], before: dict[str, bytes]) -> bool:
    """The trees equal but for the store's own file, which SQLite's restore rewrites page by
    page (its rows are compared apart)."""

    def kept(tree: dict[str, bytes]) -> dict[str, bytes]:
        return {name: body for name, body in tree.items() if not name.startswith(paths.OLD_STORE)}

    return kept(after) == kept(before)


def test_after_the_old_logins_go_the_move_only_goes_on(
    old_home: Path, user_home: Path, keychain: FakeSecurity, monkeypatch: pytest.MonkeyPatch
) -> None:
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, "forget", "after")
        with pytest.raises(Crash):
            home_migration.move_home()

    with pytest.raises(home_migration.MigrationFailed, match="can only be finished"):
        home_migration.roll_back_move()
    assert home_migration.move_home() is not None
    assert home_migration.planned_move() is None


# --- the check after the rewrite: never loosened -------------------------------------------------


def test_a_reseal_that_fails_its_check_rolls_the_move_back_whole(
    old_home: Path, user_home: Path, keychain: FakeSecurity, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Mutant: the records re-issued with the captain's left at its old paths. The check
    refuses it as a restart would, and the move rolls back: no ~/.rlx, every login back."""
    before, items = _tree(old_home), keychain.items()
    real = launch_seal.reissue

    def leaves_the_captain(move: Any) -> tuple[list[str], list[str]]:
        def but_the_captain(value: object) -> object:
            if isinstance(value, dict) and value.get("id") == "agt-captain":
                return value
            return move(value)

        return real(but_the_captain)

    monkeypatch.setattr(launch_seal, "reissue", leaves_the_captain)

    with pytest.raises(home_migration.MigrationFailed, match="agt-captain") as failed:
        home_migration.move_home()

    assert failed.value.rolled_back
    assert not os.path.lexists(user_home / ".rlx")
    assert _same_but_store(_tree(old_home), before)
    assert keychain.items() == items
    assert _accepted() == {"agt-captain", "agt-coder"}


def test_a_seal_for_a_path_outside_the_home_is_left_as_it_is_and_still_refused(
    old_home: Path, user_home: Path, tmp_path: Path
) -> None:
    stray = launch_seal.read("agt-stray")
    assert isinstance(stray, launch_seal.LaunchRecord)
    with store_session() as store:
        project = store.get_project("prj_app")
    assert project is not None
    assert fleet_service._replay_refusal(project, stray) is not None

    assert home_migration.move_home() is not None

    moved = launch_seal.read("agt-stray")
    assert isinstance(moved, launch_seal.LaunchRecord)
    assert moved.project_root == str(tmp_path / "elsewhere")  # not the home's: never moved
    with store_session() as store:
        project = store.get_project("prj_app")
    assert project is not None
    assert "is not the one it was spawned in" in (
        fleet_service._replay_refusal(project, moved) or ""
    )


# --- acceptance 3: a home that is not quiet -------------------------------------------------------


def _an_instance(home: Path) -> str:
    folder = paths.instances_dir()
    folder.mkdir(parents=True, exist_ok=True)
    now = datetime.now(UTC).isoformat()
    record = {
        "id": "0123456789ab",
        "pid": 4242,
        "host": "another-mac",
        "version": "0.10.2",
        "schema": 1,
        "kind": "native",
        "tty": None,
        "started_at": now,
        "heartbeat": now,
    }
    (folder / "0123456789ab.json").write_text(json.dumps(record), encoding="utf-8")
    return "rlx 0.10.2 (native on another-mac, pid 4242)"


@pytest.mark.parametrize("what", ["instance", "watcher", "pane"])
def test_a_live_instance_watcher_or_pane_refuses_naming_it_and_nothing_moves(
    old_home: Path, user_home: Path, keychain: FakeSecurity, what: str
) -> None:
    held: list[int] = []
    if what == "instance":
        named = _an_instance(old_home)
    elif what == "watcher":
        watch.watch_dir().mkdir(parents=True, exist_ok=True)
        fd = locking.open_lock_file(watch.lock_path())
        locking.acquire(fd, wait_s=0)
        held.append(fd)
        watch.pid_path().write_text("777", encoding="utf-8")
        named = "the watcher (pid 777)"
    else:
        tmux = shutil.which("tmux")
        if tmux is None:
            pytest.skip("no tmux here")
        import subprocess

        subprocess.run(
            [tmux, "-L", "asq", "-f", os.devnull, "new-session", "-d", "-s", "t", "sleep 60"],
            check=True,
        )
        named = "1 live pane on rlx's tmux server 'asq'"
    before = _tree(old_home)
    try:
        with pytest.raises(home_migration.MigrationRefused) as refused:
            home_migration.move_home()
        assert any(named in line for line in refused.value.running), refused.value.running
        result = CliRunner().invoke(app, ["move-home"])
        assert result.exit_code == 1 and named in result.output.replace("\n", " ")
    finally:
        for fd in held:
            os.close(fd)
    assert _tree(old_home) == before
    assert not os.path.lexists(user_home / ".rlx")
    assert len(keychain.items()) == 2


# --- the Keychain ---------------------------------------------------------------------------


def test_a_copy_that_reads_back_otherwise_stops_the_move_before_anything_moves(
    old_home: Path, user_home: Path, keychain: FakeSecurity
) -> None:
    before, items = _tree(old_home), keychain.items()
    (keychain.root / "corrupt").write_text("", encoding="utf-8")

    with pytest.raises(home_migration.MigrationFailed, match="read back differently") as failed:
        home_migration.move_home()

    assert failed.value.rolled_back
    assert keychain.items() == items  # the copy deleted, the old logins as they were
    assert _same_but_store(_tree(old_home), before)
    assert not os.path.lexists(user_home / ".rlx")


def test_a_locked_keychain_stops_the_move_naming_it(
    old_home: Path, user_home: Path, keychain: FakeSecurity
) -> None:
    service = accounts_core.keychain_service(accounts_core._managed(4))
    keychain.put(service, "", exit=36, stderr="User interaction is not allowed.\n")

    with pytest.raises(home_migration.MigrationFailed, match="Keychain is locked"):
        home_migration.move_home()

    assert not os.path.lexists(user_home / ".rlx")
    assert home_migration.planned_move() is not None


def test_a_slot_with_no_login_is_moved_with_nothing_copied(
    old_home: Path, user_home: Path, keychain: FakeSecurity
) -> None:
    service = accounts_core.keychain_service(accounts_core._managed(4))
    table = keychain.items()
    del table[f"{service}\ttester"]
    keychain.table.write_text(json.dumps(table), encoding="utf-8")

    moved = home_migration.move_home()

    assert moved is not None and moved.copied == ["claude-code 2"]


def test_the_copy_keeps_the_items_bytes_as_claude_code_wrote_them(keychain: FakeSecurity) -> None:
    """Data ``security -w`` prints as hex (a byte it will not print) is copied as its bytes,
    and the same lookup prints the copy as it printed the item."""
    data = '{"claudeAiOauth":{"accessToken":"café"}}'.encode()
    keychain.put("Claude Code-credentials-0ld", data.hex())
    old = accounts_core.keychain_item_data("Claude Code-credentials-0ld", "tester")
    assert old.data == data

    assert accounts_core.add_keychain_item("Claude Code-credentials-new", "tester", data) is None

    assert (
        accounts_core.keychain_item_data("Claude Code-credentials-new", "tester").printed
        == old.printed
    )


def test_an_item_too_long_for_a_line_on_stdin_is_never_put_on_a_command_line(
    keychain: FakeSecurity,
) -> None:
    why = accounts_core.add_keychain_item("Claude Code-credentials-new", "tester", b"x" * 3000)

    assert why is not None and "too long" in why
    assert keychain.calls() == []


# --- what the move rewrites --------------------------------------------------------------------


def test_every_helpers_old_place_moves_to_the_place_it_names_in_the_new_layout(
    user_home: Path,
) -> None:
    """The move's table and the helpers agree: each path a helper names in an old home is,
    moved, the path it names in a new one."""
    helpers = {
        name: getattr(paths, name)
        for name in (
            "db_path",
            "log_dir",
            "credentials_path",
            "launch_seals_dir",
            "tmux_conf_path",
            "instances_dir",
            "remote_state_path",
            "remote_audit_path",
            "remote_dist_dir",
            "remote_needs_path",
            "remote_push_path",
            "usage_sweep_lock_path",
            "usage_share_lock_path",
            "active_ping_lock_path",
            "ngrok_token_path",
        )
    }
    calls: dict[str, Any] = {name: helper for name, helper in helpers.items()}
    calls["agent_accounts_dir"] = lambda: paths.agent_accounts_dir("codex-accounts")
    calls["ngrok_config_path"] = lambda: paths.ngrok_config_path("remote-ngrok-v3.yml")
    make_old_home(user_home)
    old = {name: call() for name, call in calls.items()}
    shutil.rmtree(user_home / ".aisquare")
    (user_home / ".rlx").mkdir()
    new = {name: call() for name, call in calls.items()}
    mover = home_migration._Mover(
        [str(user_home / ".aisquare")], str(user_home / ".rlx"), home_spelled=None
    )
    for name in calls:
        assert Path(mover.text(str(old[name]))) == new[name], name


def test_the_mover_moves_paths_under_the_home_and_nothing_else() -> None:
    mover = home_migration._Mover(["/u/.aisquare"], "/u/.rlx", home_spelled="~/.aisquare")
    assert mover.text("/u/.aisquare/claude-accounts/2/x") == "/u/.rlx/accounts/claude/2/x"
    assert mover.text("/u/.aisquare/context.db-wal") == "/u/.rlx/rlx.db-wal"
    assert mover.text("/u/.aisquare/captain") == "/u/.rlx/captain"
    assert mover.text("/u/.aisquare") == "/u/.rlx"
    assert mover.text("cd '/u/.aisquare/log' && ls") == "cd '/u/.rlx/logs' && ls"
    assert mover.text("~/.aisquare/instances") == "~/.rlx/run/instances"
    for left in ("/u/.aisquare2/log", "/v/.aisquare/log", "/elsewhere/claude-accounts/2"):
        assert mover.text(left) == left
    assert mover.text(mover.text("/u/.aisquare/log")) == "/u/.rlx/logs"  # idempotent
    stored = json.dumps({"/u/.aisquare/codex-accounts/3": ["/u/.aisquare/credentials"]})
    assert json.loads(mover.stored(stored)) == {
        "/u/.rlx/accounts/codex/3": ["/u/.rlx/accounts/aisquare/credentials"]
    }
    named = home_migration._Mover(["/h"], "/h", home_spelled=None)
    assert named.text("/h/launch-seals/a.json /h/logs") == "/h/fleet/launch-seals/a.json /h/logs"


def test_a_folder_the_backup_makes_is_moved_too_and_no_old_name_is_left(
    old_home: Path, user_home: Path
) -> None:
    """An old home with no log folder: the backup opens the store, which makes one, as any
    command does. Listed when the renames began, before the backup, it was left behind under
    its old name. Mutant: the renames listed when the move starts."""
    shutil.rmtree(paths.log_dir())
    assert not paths.log_dir().exists()

    assert home_migration.move_home() is not None

    new = user_home / ".rlx"
    left = [name for name in os.listdir(new) if paths.new_place(name) is not None]
    assert left == [], left
    assert (new / "logs").is_dir()


def test_a_named_home_is_laid_out_anew_in_place_with_no_link(
    tmp_path: Path, user_home: Path, keychain: FakeSecurity, monkeypatch: pytest.MonkeyPatch
) -> None:
    named = tmp_path / "named"
    make_old_home(user_home)
    shutil.move(str(user_home / ".aisquare"), named)
    monkeypatch.setenv(paths.RLX_HOME_ENV, str(named))
    assert paths.home_of() == (named, paths.OLD)
    with store_session():
        pass
    before = _tree(named)

    moved = home_migration.move_home()

    assert moved is not None and moved.named and not moved.linked
    assert paths.home_of() == (named, paths.NEW)
    assert not os.path.lexists(user_home / ".rlx") and not os.path.lexists(user_home / ".aisquare")
    after = _tree(named)
    for name, body in before.items():
        if not name.startswith(paths.OLD_STORE):
            assert after.get(_moved(name)) == body, name
    assert (named / "rlx.db").is_file() and not (named / paths.OLD_STORE).exists()


def test_each_accounts_hooks_name_its_new_place(
    old_home: Path, user_home: Path, keychain: FakeSecurity
) -> None:
    from aisquare.core import agents as agent_core

    for agent, slot in (("claude-code", 2), ("codex", 3)):
        assert agent_core.install_hooks(agent, _slot(slot, agent))
    assert any(str(old_home) in one for one in agent_core.hook_commands("codex", _slot(3, "codex")))

    assert home_migration.move_home() is not None

    new = user_home / ".rlx"
    for agent, place in (("claude-code", "claude/2"), ("codex", "codex/3")):
        commands = agent_core.hook_commands(agent, new / "accounts" / place)
        assert commands, agent
        assert not any(str(old_home) in command for command in commands), commands
    codex_hooks = agent_core.hook_commands("codex", new / "accounts" / "codex" / "3")
    assert any(str(new / "accounts" / "codex" / "3") in command for command in codex_hooks)


# --- condition 1: nothing else moves a home -------------------------------------------------------


def test_no_ordinary_command_or_hook_moves_an_old_home(old_home: Path, user_home: Path) -> None:
    runner = CliRunner()
    runner.invoke(app, [])
    for command in (["doctor"], ["fleet", "ls"], ["accounts", "list"]):
        runner.invoke(app, command)
    runner.invoke(app, ["hook", "session-start"], input=json.dumps({"session_id": SESSION}))

    assert not os.path.lexists(user_home / ".rlx")
    assert not (user_home / ".aisquare").is_symlink()
    assert not (old_home / "backups").exists()
    assert paths.home_of() == (old_home, paths.OLD)


def test_a_dry_run_says_what_would_move_and_moves_nothing(old_home: Path, user_home: Path) -> None:
    before = _tree(old_home)

    result = CliRunner().invoke(app, ["move-home", "--dry-run"])

    assert result.exit_code == 0, result.output
    assert "~/.aisquare → ~/.rlx" in result.output
    assert _tree(old_home) == before and not os.path.lexists(user_home / ".rlx")


def test_a_home_in_the_new_layout_has_nothing_to_move(user_home: Path) -> None:
    (user_home / ".rlx").mkdir()

    result = CliRunner().invoke(app, ["--json", "move-home"])

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["moved"] is False


# --- rlx upgrade runs it -------------------------------------------------------------------------


@dataclass
class Installed:
    """The fakes' record: each process the upgrade ran, and the release PyPI says is latest
    (the new install says it too)."""

    script: Path
    captured: list[list[str]] = field(default_factory=list)
    latest: str = "0.10.2"


@pytest.fixture
def installed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Installed]:
    """A uv tool install of 0.10.2, the release PyPI's latest, with uv, PyPI and the new
    install's processes as fakes: the new install's ``move-home`` is this one's, run here."""
    from tests.test_lifecycle_upgrade import _OURS_PINNED, _facts, _prefix, _receipt

    prefix = _prefix(tmp_path / "uv", _receipt(_OURS_PINNED))
    script = prefix / "bin" / "aisquare"
    script.write_text("#!/bin/sh\n", encoding="utf-8")
    script.chmod(0o755)
    found = _facts(prefix)
    world = Installed(script)

    def run_captured(argv: Any, *, timeout: float) -> Captured:
        world.captured.append(list(argv))
        if argv[-1] == "--version":
            return Captured(0, f"aisquare {world.latest}\n")
        if list(argv[1:]) == ["--json", "move-home"]:
            # A process of its own in a real upgrade: here, the CLI's state put back after it.
            outer = get_state()
            done = CliRunner().invoke(app, list(argv[1:]))
            set_state(outer)
            return Captured(done.exit_code, done.stdout, "")
        return Captured(0, "connected\n")

    monkeypatch.setattr(install_route, "facts", lambda: found)
    monkeypatch.setattr(install_route, "find_uv", lambda: "/usr/bin/uv")
    monkeypatch.setattr(lifecycle, "__version__", "0.10.2")
    monkeypatch.setattr(install_route, "fetch_latest", lambda *a, **k: LatestRelease(world.latest))
    monkeypatch.setattr(install_route, "run_installer", lambda argv, *, env, to_stderr: 0)
    monkeypatch.setattr(install_route, "run_captured", run_captured)
    yield world


def test_upgrade_on_the_latest_release_moves_an_old_home(
    old_home: Path, user_home: Path, installed: Installed
) -> None:
    """0.10.1's upgrade installs 0.10.2 and knows no move: 0.10.2's own upgrade, with nothing
    newer to install, makes it."""
    runner = CliRunner()
    checked = runner.invoke(app, ["--json", "upgrade", "--check"])
    assert json.loads(checked.stdout)["home_move"] == "~/.aisquare → ~/.rlx"
    dry = runner.invoke(app, ["upgrade", "--dry-run"])
    assert dry.exit_code == 0 and "its home would move: ~/.aisquare → ~/.rlx" in dry.output
    assert not os.path.lexists(user_home / ".rlx")

    result = runner.invoke(app, ["upgrade", "--yes"])

    assert result.exit_code == 0, result.output
    assert "✓ the home moved: ~/.aisquare → ~/.rlx" in result.output
    assert paths.home_of() == (user_home / ".rlx", paths.NEW)


def test_an_upgrade_moves_the_home_in_the_new_install_then_rewrites_its_hooks_there(
    old_home: Path, user_home: Path, installed: Installed, monkeypatch: pytest.MonkeyPatch
) -> None:
    from tests.test_lifecycle_upgrade import _hooked

    _hooked(_slot(2), installed.script)  # this install's hooks
    # The new install's `move-home` is this process (the fake), whose hook refresh names
    # this process's rlx: the new install's own, as it is in a real upgrade.
    monkeypatch.setattr(lifecycle, "runs_this_install", lambda binary, found: True)
    installed.latest = "0.10.3"
    planned = json.loads(CliRunner().invoke(app, ["--json", "upgrade", "--dry-run"]).stdout)
    assert planned["home_move"] == "~/.aisquare → ~/.rlx"
    assert any(str(old_home) in site for site in planned["refresh_hooks"]), planned

    result = CliRunner().invoke(app, ["upgrade", "--yes"])

    assert result.exit_code == 0, result.output
    verbs = [argv[1:] for argv in installed.captured]
    moved_at = verbs.index(["--json", "move-home"])
    assert verbs[moved_at - 1] == ["--version"] or verbs[moved_at - 1][-1] == "--version"
    refreshes = [argv for argv in verbs[moved_at + 1 :] if "refresh-hooks" in argv]
    assert refreshes, verbs
    for argv in refreshes:
        assert str(old_home) not in " ".join(argv), argv
    assert "✓ the home moved" in result.output


def test_an_upgrade_that_cannot_move_the_home_says_so_and_exits_1(
    old_home: Path, user_home: Path, installed: Installed
) -> None:
    named = _an_instance(old_home)

    result = CliRunner().invoke(app, ["upgrade", "--yes"])

    assert result.exit_code == 1
    said = result.output.replace("\n", " ")
    assert "✗ the home was not moved" in said and named in said
    assert not os.path.lexists(user_home / ".rlx")


# --- the app's Update and the Doctor (#1985) ----------------------------------------------------


@pytest.fixture
def running(monkeypatch: pytest.MonkeyPatch, tool: Tool, user_home: Path) -> Tool:  # noqa: F811
    """The ``tool`` install at 0.9.0, in a user home of the test's own."""
    monkeypatch.setattr(updates, "__version__", "0.9.0")
    monkeypatch.setattr(updates, "check_enabled", lambda: True)
    return tool


def test_the_daily_check_offers_the_move_of_an_old_home_with_no_newer_release(
    running: Tool,
    machine: Machine,  # noqa: F811
    user_home: Path,
) -> None:
    machine.latest = LatestRelease("0.9.0")
    make_old_home(user_home)

    offer = updates.check_for_release()

    assert offer is not None and offer.home_only and offer.home_move == MOVE
    assert (offer.version, offer.current, offer.runnable, offer.announce) == (
        "0.9.0",
        "0.9.0",
        True,
        False,
    )
    machine.latest = LatestRelease("0.9.1")
    newer = updates.check_for_release(datetime_later())
    assert newer is not None and newer.version == "0.9.1" and newer.home_move == MOVE
    assert not newer.home_only


def datetime_later() -> Any:
    """A day after now: the next check asks PyPI again."""
    from datetime import UTC, datetime, timedelta

    return datetime.now(UTC) + timedelta(days=2)


def test_one_click_on_the_latest_release_moves_the_home_and_reopens(
    runner: CliRunner,
    tool: Tool,  # noqa: F811
    machine: Machine,  # noqa: F811
    terminal: Terminal,  # noqa: F811
    old_home: Path,
    user_home: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(lifecycle, "__version__", "0.9.0")
    machine.latest = LatestRelease("0.9.0")
    updates.record_press([])

    result = runner.invoke(app, ["upgrade", "--one-click"])

    assert result.exit_code == 0, result.output
    assert machine.installs == [], "nothing newer: nothing installed"
    assert "✓ the home moved: ~/.aisquare → ~/.rlx" in result.output
    assert paths.home_of() == (user_home / ".rlx", paths.NEW)
    assert terminal.prompts == [] and terminal.reopened == [["ui", "--after-update"]]


def test_one_click_on_a_busy_home_says_what_runs_and_reopens_after_enter(
    runner: CliRunner,
    tool: Tool,  # noqa: F811
    machine: Machine,  # noqa: F811
    terminal: Terminal,  # noqa: F811
    old_home: Path,
    user_home: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(lifecycle, "__version__", "0.9.0")
    machine.latest = LatestRelease("0.9.0")
    named = _an_instance(old_home)

    result = runner.invoke(app, ["upgrade", "--one-click"])

    assert result.exit_code == 1
    assert named in result.output.replace("\n", " ")
    assert not os.path.lexists(user_home / ".rlx")
    assert len(terminal.prompts) == 1 and terminal.reopened == [["ui", "--after-update"]]


def test_the_doctor_names_a_home_laid_out_before_0_10_2_and_asq_names_no_command(
    user_home: Path,
    runner: CliRunner,
) -> None:
    assert diagnostics._check_home_layout() is None, "a new home: no row"
    make_old_home(user_home)

    row = diagnostics._check_home_layout()

    assert row is not None and row.status.value == "warn" and row.name == "home-layout"
    assert row.detail.startswith(f"laid out before 0.10.2 ({MOVE})")
    assert f"`{prog.word()} upgrade` moves it" in row.detail
    assert row.ui_fix == diagnostics.HOME_LAYOUT_UI_FIX and "upgrade" not in row.ui_fix
    done = runner.invoke(app, ["--json", "doctor"])
    rows = {check["name"]: check for check in json.loads(done.stdout)}
    assert rows["home-layout"]["status"] == "warn"


def test_an_install_rlx_does_not_upgrade_still_has_its_home_moved_by_upgrade(
    old_home: Path,
    user_home: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The reviewer's m6: on Homebrew or pipx, rlx installs nothing, but the home's move is its
    own: `rlx upgrade` makes it, then says how that install upgrades. So Doctor's and the
    app's way to the move (the ⬆ row's one-click upgrade) is there on every install."""
    plan = UpgradePlan(
        route=InstallRoute(install_route.HOMEBREW, UI_FACTS, formula="aisquare-cli"),
        current="0.9.0",
        target=None,
        latest=None,
        argv=("brew", "upgrade", "aisquare-cli"),
        env={},
        reason="Homebrew installs are upgraded with brew",
        home_move=MOVE,
    )
    monkeypatch.setattr(lifecycle, "upgrade_plan", lambda *a, **k: plan)

    result = CliRunner().invoke(app, ["upgrade", "--yes"])

    assert result.exit_code == 0, result.output
    said = result.output.replace("\n", " ")
    assert "✓ the home moved: ~/.aisquare → ~/.rlx" in said
    assert "upgrade this install with: brew upgrade aisquare-cli" in said
    assert paths.home_of() == (user_home / ".rlx", paths.NEW)


# --- the rlx service (#32): stopped for the move, started again after (the brief, item 4) --------


@pytest.fixture
def a_service(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    """The rlx service as the move meets it: running until asked to stop, then started again;
    each ask kept, in order, beside each rename the move makes."""
    from types import SimpleNamespace

    from aisquare.services import service_host

    calls: list[str] = []

    def stop(*, timeout: float = 0) -> bool:
        calls.append("stop")
        return True

    def start(*, timeout: float = 0, argv: Any = None) -> Any:
        calls.append("start")
        return SimpleNamespace(pid=4321)

    real = home_migration._rename_once

    def rename(source: Path, target: Path) -> None:
        calls.append("rename")
        real(source, target)

    monkeypatch.setattr(service_host, "stop_service", stop)
    monkeypatch.setattr(service_host, "start_service", start)
    monkeypatch.setattr(home_migration, "_rename_once", rename)
    return calls


def test_the_move_stops_the_service_first_and_starts_it_again_after(
    old_home: Path, user_home: Path, a_service: list[str]
) -> None:
    moved = home_migration.move_home()

    assert moved is not None
    assert a_service[0] == "stop" and a_service[-1] == "start", a_service
    assert a_service.count("stop") == a_service.count("start") == 1
    assert "rename" in a_service[1:-1], "every rename between the stop and the start"
    assert "the rlx service runs again (pid 4321)" in moved.notes
    assert paths.home_of() == (user_home / ".rlx", paths.NEW)


def test_a_refused_move_starts_the_service_again_and_says_so(
    old_home: Path, user_home: Path, a_service: list[str]
) -> None:
    named = _an_instance(old_home)

    with pytest.raises(home_migration.MigrationRefused) as refused:
        home_migration.move_home()

    assert a_service == ["stop", "start"]
    assert any(named in line for line in refused.value.running)
    assert "the rlx service runs again (pid 4321)" in refused.value.reason
    assert not os.path.lexists(user_home / ".rlx")


def test_a_service_that_will_not_stop_refuses_the_move_and_nothing_moves(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from aisquare.services import service_host

    def stuck(*, timeout: float = 0) -> bool:
        raise service_host.ServiceError("the rlx service (pid 7) did not stop within 30 s")

    monkeypatch.setattr(service_host, "stop_service", stuck)
    before = _tree(old_home)

    with pytest.raises(home_migration.MigrationRefused, match="did not stop within 30 s"):
        home_migration.move_home()

    assert _tree(old_home) == before and not os.path.lexists(user_home / ".rlx")


def test_a_drop_of_the_cache_stops_the_service_and_starts_it_again_refused_or_not(
    old_home: Path, user_home: Path, a_service: list[str]
) -> None:
    """The reviewer's m4: untested, a drop that never stopped the service, or never started it
    again, survived. It stops it, as the move does, and starts it again after, refused or not."""
    assert home_migration.move_home() is not None
    a_service.clear()
    scratch = paths.cache_dir() / "scratch.txt"
    scratch.parent.mkdir(parents=True, exist_ok=True)
    scratch.write_text("a session's", encoding="utf-8")
    named = _an_instance(user_home / ".rlx")

    with pytest.raises(home_migration.MigrationRefused) as refused:
        home_migration.drop_cache()

    assert any(named in line for line in refused.value.running) and scratch.exists()
    assert a_service == ["stop", "start"]
    for record in paths.instances_dir().iterdir():
        record.unlink()
    a_service.clear()

    home_migration.drop_cache()

    assert a_service == ["stop", "start"] and not paths.cache_dir().exists()


def test_a_service_that_will_not_stop_refuses_the_drop_saying_only_that(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The reviewer's n2: the refusal read "the cache was not dropped: the rlx service would not
    stop: the home was not moved", the move's words in a drop made after it."""
    from aisquare.services import service_host

    assert home_migration.move_home() is not None
    scratch = paths.cache_dir() / "scratch.txt"
    scratch.parent.mkdir(parents=True, exist_ok=True)
    scratch.write_text("kept", encoding="utf-8")

    def stuck(*, timeout: float = 0) -> bool:
        raise service_host.ServiceError("the rlx service (pid 7) did not stop within 30 s")

    monkeypatch.setattr(service_host, "stop_service", stuck)

    result = CliRunner().invoke(app, ["move-home", "--drop-cache"])

    said = result.output.replace("\n", " ")
    assert result.exit_code == 1 and scratch.exists(), said
    assert "the cache was not dropped: the rlx service (pid 7) did not stop within 30 s" in said
    assert "not moved" not in said


def test_a_drop_of_the_cache_holds_the_home_s_move_lock_and_removes_it_renamed_aside(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The reviewer's n4: the drop took no lock, so a move or a second drop could run beside
    it; and an agent started between the quiet check and the removal had its scratch removed
    with the rest. The drop holds the move's lock, and renames the cache aside before removing
    it, so a cache made after the check is not reached."""
    assert home_migration.move_home() is not None
    cache = paths.cache_dir()
    (cache / "old").mkdir(parents=True, exist_ok=True)
    (cache.with_name("cache.dropping-1")).mkdir()  # one a crashed drop left aside

    key, named = home_migration._move_lock_key()
    with (
        home_migration._one_at_a_time(key, named=named),
        pytest.raises(home_migration.MigrationRefused, match="a drop of its cache"),
    ):
        home_migration.drop_cache()
    assert (cache / "old").exists()

    real = shutil.rmtree
    made_after_the_check: list[Path] = []

    def rmtree(path: Any, *args: Any, **kwargs: Any) -> None:
        if not made_after_the_check and Path(path).name == f"cache.dropping-{os.getpid()}":
            (cache / "new-session").mkdir(parents=True)  # an agent's, after the rename
            made_after_the_check.append(cache / "new-session")
        real(path, *args, **kwargs)

    monkeypatch.setattr(shutil, "rmtree", rmtree)
    home_migration.drop_cache()

    assert (cache / "new-session").is_dir() and not (cache / "old").exists()
    assert [p.name for p in cache.parent.iterdir() if p.name.startswith("cache.")] == []


def test_an_upgrade_starts_the_service_again_only_once_the_home_has_moved(
    old_home: Path, user_home: Path, installed: Installed, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The upgrade stops the service before its installer (#32); it starts it again after the
    new install moved the home, so it runs on ~/.rlx. Mutant: started again right after the
    version check, it was running when the move looked, and the move refused."""
    installed.latest = "0.10.3"
    monkeypatch.setattr(lifecycle, "_stop_the_service", lambda command: True)

    def start(service_host: Any, plan: Any, version: Any) -> str:
        installed.captured.append(["rlx", "service", "start"])
        return "the rlx service runs again (pid 9)"

    monkeypatch.setattr(lifecycle, "_start_the_service_again", start)

    result = CliRunner().invoke(app, ["upgrade", "--yes"])

    assert result.exit_code == 0, result.output
    verbs = [argv[1:] for argv in installed.captured]
    assert verbs.index(["--json", "move-home"]) < verbs.index(["service", "start"]), verbs
    assert "✓ the home moved" in result.output


# --- round 2 (the tester's #2071) ----------------------------------------------------------


def test_a_roll_back_after_the_hooks_step_puts_codex_s_trust_of_its_hooks_back(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Blocker 1: the refresh rewrote config.toml's [hooks.state] and the trust record beside
    hooks.json; a roll-back put hooks.json back alone, so the old hooks lost their trust."""
    from aisquare.core import agents as agent_core

    codex = _slot(3, "codex")
    assert agent_core.install_hooks("codex", codex)
    files_before = {path.name: path.read_bytes() for path in codex.iterdir() if path.is_file()}
    registry_before = paths.agents_registry_path().read_bytes()
    assert agent_core.hook_trust_recorded("codex", codex), "the premise: trusted before"

    def full(journal: home_migration.Journal) -> None:
        raise OSError(28, "No space left on device")

    monkeypatch.setitem(home_migration._MOVE_STEPS, "link", full)
    with pytest.raises(home_migration.MigrationFailed) as failed:
        home_migration.move_home()

    assert failed.value.rolled_back
    files_after = {path.name: path.read_bytes() for path in codex.iterdir() if path.is_file()}
    assert files_after == files_before
    assert paths.agents_registry_path().read_bytes() == registry_before
    assert agent_core.hook_trust_recorded("codex", codex)


def _snapshot(user_home: Path, keychain: FakeSecurity, into: Path) -> None:
    shutil.copytree(user_home, into / "user", symlinks=True)
    shutil.copy2(keychain.table, into / "items.json")


def _restore(user_home: Path, keychain: FakeSecurity, saved: Path) -> None:
    for entry in user_home.iterdir():
        if entry.is_symlink() or entry.is_file():
            entry.unlink()
        else:
            shutil.rmtree(entry)
    for entry in (saved / "user").iterdir():
        target = user_home / entry.name
        if entry.is_dir() and not entry.is_symlink():
            shutil.copytree(entry, target, symlinks=True)
        else:
            shutil.copy2(entry, target, follow_symlinks=False)
    shutil.copy2(saved / "items.json", keychain.table)
    accounts_core.forget_keychain_reads()


@pytest.mark.parametrize("failing", ["check", "hooks", "link"])
def test_a_crash_at_each_write_of_a_roll_back_is_finished_by_the_next_run(
    old_home: Path,
    user_home: Path,
    keychain: FakeSecurity,
    tmp_path: Path,
    failing: str,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Blocker 2: the roll-back is journaled phase by phase, each file put back and each rename
    reversed written down, so a crash at any of its journal writes (before the write lands, or
    after) is finished by the next run, and the home is as it was: no stray copy of the seals,
    no slot left at its new place, every login where it was. The move runs some seventy times
    here, so the fake ``security`` answers in this process."""
    from aisquare.core import agents as agent_core

    keychain.in_process(monkeypatch)
    assert agent_core.install_hooks("codex", _slot(3, "codex"))
    saved = tmp_path / "saved"
    saved.mkdir()
    _snapshot(user_home, keychain, saved)
    before = _tree(old_home)
    rows = _store_rows(old_home, paths.OLD_STORE)
    items = keychain.items()
    real_save = home_migration._save_move_journal

    def stopped_at(n: int, when: str) -> int:
        """Run the failing move, crashing at the n-th journal write of its roll-back; how
        many writes the roll-back made when n is past the last."""
        seen = [0]

        def save(journal: home_migration.Journal) -> None:
            if journal.rolling_back:
                seen[0] += 1
                if seen[0] == n and when == "before":
                    raise Crash("before a roll-back write")
                real_save(journal)
                if seen[0] == n:
                    raise Crash("after a roll-back write")
                return
            real_save(journal)

        def fails(journal: home_migration.Journal) -> None:
            raise OSError(28, "No space left on device")

        with pytest.MonkeyPatch.context() as crash:
            crash.setattr(home_migration, "_save_move_journal", save)
            crash.setitem(home_migration._MOVE_STEPS, failing, fails)
            with contextlib.suppress(Crash, home_migration.MigrationFailed):
                home_migration.move_home()
        return seen[0]

    writes = stopped_at(10_000, "after")
    _restore(user_home, keychain, saved)
    assert writes > 5, writes
    for n in range(1, writes + 1):
        for when in ("before", "after"):
            stopped_at(n, when)
            try:
                home_migration.move_home()  # the next run: it finishes the roll-back
            except home_migration.MigrationFailed as exc:
                assert exc.rolled_back, (n, when, exc.reason)
            case = (failing, n, when)
            if os.path.lexists(user_home / ".rlx"):
                # Crashed before the roll-back was written down at all: the journal still says
                # a move, and the next run finishes it, whole.
                assert (n, when) == (1, "before"), case
                assert paths.home_of() == (user_home / ".rlx", paths.NEW), case
                assert (user_home / ".aisquare").is_symlink(), case
                assert home_migration.planned_move() is None, case
                assert _accepted() == {"agt-captain", "agt-coder"}, case
            else:
                assert _same_but_store(_tree(old_home), before), case
                assert _store_rows(old_home, paths.OLD_STORE) == rows, case
                assert keychain.items() == items, case
                assert not (old_home / "backups").exists(), case
                assert home_migration.planned_move() is not None, case
            _restore(user_home, keychain, saved)


def test_a_variable_naming_aisquare_itself_moves_it_to_rlx_and_links_it(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Blocker 3: the fleet hands every window the home by both names, resolved; a shell
    started from one moved ~/.aisquare in place, and an rlx without the variable then read an
    empty old home. Named so, it is the default home: moved to ~/.rlx and linked."""
    for variable in (paths.RLX_HOME_ENV, paths.HOME_ENV_VAR):
        monkeypatch.setenv(variable, os.path.realpath(old_home))
    found = home_migration.planned_move()
    assert found is not None and not found.named and found.new_root == user_home / ".rlx"

    moved = home_migration.move_home()

    assert moved is not None and not moved.named and moved.linked
    assert (user_home / ".aisquare").is_symlink() and (user_home / ".rlx" / "rlx.db").is_file()
    assert paths.home_of()[1] == paths.NEW and paths.db_path().resolve() == (
        user_home / ".rlx" / "rlx.db"
    )
    monkeypatch.delenv(paths.RLX_HOME_ENV)
    monkeypatch.delenv(paths.HOME_ENV_VAR)
    assert paths.home_of() == (user_home / ".rlx", paths.NEW)
    with store_session() as store:
        assert {p.id for p in store.list_projects(all=True)} >= {"prj_home", "prj_app"}


def test_a_crash_after_the_root_rename_with_the_variable_set_is_finished(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    for variable in (paths.RLX_HOME_ENV, paths.HOME_ENV_VAR):
        monkeypatch.setenv(variable, os.path.realpath(old_home))
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, "link", "before")
        with pytest.raises(Crash):
            home_migration.move_home()

    assert home_migration.move_home() is not None
    assert (user_home / ".aisquare").is_symlink() and home_migration.planned_move() is None


def test_upgrade_check_says_in_words_that_the_home_would_move(
    old_home: Path, user_home: Path, installed: Installed
) -> None:
    """Blocker 4: only --json said it."""
    result = CliRunner().invoke(app, ["upgrade", "--check"])

    assert result.exit_code == 0, result.output
    said = result.output.replace("\n", " ")
    assert f"home: laid out before 0.10.2 — `{prog.word()} upgrade` moves it ({MOVE})" in said
    assert "nothing newer to install" in said and "nothing to upgrade" not in said


def test_a_row_refused_before_the_move_and_allowed_after_it_rolls_the_move_back(
    old_home: Path, user_home: Path, keychain: FakeSecurity
) -> None:
    """Blocker 5 (condition 4, never loosen): a project whose root is already ~/.rlx/captain
    and a record that names ~/.aisquare/captain is refused before; the move's rewrite of the
    record alone would have let it through. The two sets must be equal."""
    captain = paths.captain_dir()
    with store_session() as store:
        store.ensure_project(ProjectInfo(id="prj_plant", root=user_home / ".rlx" / "captain"))
        store.upsert_fleet_agent(
            FleetAgent(
                id="agt-plant",
                project_id="prj_plant",
                label="plant",
                role="coder",
                pane_id="%2",
                cwd=captain,
                created_at=NOW,
                ended_at=NOW,
            )
        )
    record = launch_seal.LaunchRecord(
        id="agt-plant",
        project_id="prj_plant",
        project_root=str(captain),
        role="coder",
        agent="claude-code",
        binary="claude",
        cwd=str(captain),
        spec=LaunchSpec(binary="claude"),
    )
    assert launch_seal.write(record) is None
    assert "agt-plant" not in _accepted()
    before, items = _tree(old_home), keychain.items()

    with pytest.raises(home_migration.MigrationFailed, match="agt-plant, refused before") as failed:
        home_migration.move_home()

    assert failed.value.rolled_back
    assert not os.path.lexists(user_home / ".rlx") and keychain.items() == items
    assert _same_but_store(_tree(old_home), before)


def _a_long_item(token: str) -> str:
    """A Claude Code item with many MCP logins: longer than `security -i`'s line holds."""
    item = json.loads(_item(token))
    item["mcpOAuth"] = {
        f"server-{n}": {"accessToken": f"mcp-{n}-" + "x" * 120, "refreshToken": "r" * 120}
        for n in range(8)
    }
    return json.dumps(item, separators=(",", ":"))


def test_a_login_too_long_to_copy_is_pinned_to_its_old_name_and_never_on_argv(
    old_home: Path, user_home: Path, keychain: FakeSecurity
) -> None:
    """#2071's note, the manager's requirement: an item too long for `security -i` keeps its
    old name; the slot is pinned with CLAUDE_SECURESTORAGE_CONFIG_DIR, recorded in its marker,
    and still reads as signed in after the move. Nothing of it is copied or deleted."""
    four = accounts_core._managed(4)
    old_dir = str(four.config_dir)
    old_service = accounts_core.keychain_service(four)
    long_item = _a_long_item("token-4")
    assert len(long_item) > 2000
    keychain.put(old_service, long_item)

    moved = home_migration.move_home()

    assert (
        moved is not None
        and moved.pinned == ["claude-code 4"]
        and moved.copied == ["claude-code 2"]
    )
    assert keychain.has(old_service), "the pinned login stays where it is"
    assert not [call for call in keychain.calls() if "-X" in call or "token-4" in " ".join(call)]
    four_now = accounts_core._managed(4)
    assert four_now.config_dir.is_relative_to(user_home / ".rlx")
    assert accounts_core.keychain_pin(four_now) == old_dir
    env = accounts_core.launch_env(four_now)
    assert env[accounts_core.SECURE_STORAGE_VAR] == old_dir
    assert accounts_core.keychain_service(four_now) == old_service
    accounts_core.forget_keychain_reads()
    assert accounts_core.signed_in(four_now, fresh=True)
    # Another slot's launch or read in a pinned slot's environment names that slot's own item.
    two_now = accounts_core._managed(2)
    other = accounts_core.apply_launch_env(dict(env), two_now)
    assert accounts_core.SECURE_STORAGE_VAR not in other
    assert accounts_core.KEYCHAIN_PIN_VAR not in other
    assert accounts_core.keychain_service(two_now, env) == accounts_core.keychain_service(two_now)
    assert "kept under their old names" in " ".join(home_migration.said_of_move(moved))


def test_a_roll_back_takes_a_pin_away(
    old_home: Path, user_home: Path, keychain: FakeSecurity
) -> None:
    four = accounts_core._managed(4)
    keychain.put(accounts_core.keychain_service(four), _a_long_item("token-4"))
    items = keychain.items()
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, "rewrite", "before")
        with pytest.raises(Crash):
            home_migration.move_home()

    assert home_migration.roll_back_move() is True

    assert accounts_core.keychain_pin(accounts_core._managed(4)) is None
    assert keychain.items() == items and not os.path.lexists(user_home / ".rlx")


# --- round 3 (the reviewer's #2076, the manager's #2078) ----------------------------------


def test_while_a_move_is_unfinished_no_ordinary_command_runs_and_the_move_finishes(
    old_home: Path, user_home: Path
) -> None:
    """M3: after a crash, an ordinary command made a new empty context.db and log/ in the half
    moved home, and the move could never be finished. While the journal is open every command
    but the move refuses, a hook does nothing, and the next move finishes."""
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, "rewrite", "before")
        with pytest.raises(Crash):
            home_migration.move_home()
    new = user_home / ".rlx"
    before = _tree(new)
    assert paths.unfinished_home_move() is not None
    runner = CliRunner()

    for command in ([], ["doctor"], ["fleet", "ls"], ["accounts", "list"]):
        result = runner.invoke(app, command)
        assert result.exit_code == 1, (command, result.output)
        assert "part-way through its move" in result.output, command
    as_json = runner.invoke(app, ["--json", "status"])
    assert as_json.exit_code == 1 and json.loads(as_json.stdout)["error"] == "home_mid_move"
    hook = runner.invoke(app, ["hook", "session-start"], input=json.dumps({"session_id": SESSION}))
    assert hook.exit_code == 0 and hook.output == ""

    assert _tree(new) == before, "nothing written into the half-moved home"
    assert not (user_home / ".aisquare").exists()
    done = runner.invoke(app, ["move-home"])
    assert done.exit_code == 0, done.output
    assert paths.unfinished_home_move() is None
    assert runner.invoke(app, ["fleet", "ls"]).exit_code == 0


def _an_execed_launch(tmp_path: Path) -> Any:
    """An `rlx launch` that became its agent: the record written by `launchers.execing`, then
    the process exec'd into a stand-in agent that runs until killed."""
    import subprocess

    script = (
        "import os, sys\n"
        "from aisquare.core import launchers\n"
        "launchers.execing('claude')\n"
        "sys.stdout.write('ready\\n'); sys.stdout.flush()\n"
        "os.execv('/bin/sleep', ['sleep', '60'])\n"
    )
    process = subprocess.Popen(
        [sys.executable, "-c", script], stdout=subprocess.PIPE, env=dict(os.environ)
    )
    assert process.stdout is not None and process.stdout.readline() == b"ready\n"
    return process


def test_an_rlx_launch_that_became_its_agent_refuses_the_move(
    old_home: Path, user_home: Path, tmp_path: Path
) -> None:
    """M4: on POSIX `rlx launch` execs its agent and kept no record, so the move ran under an
    agent whose hooks write into the home and whose login is refreshed in the item step 8
    deletes. It is recorded before the exec, the lock held by the agent, and counted."""
    from aisquare.core import launchers

    process = _an_execed_launch(tmp_path)
    try:
        sessions = launchers.exec_sessions()
        assert [s.pid for s in sessions] == [process.pid]
        assert launchers.waiting_launchers() == (), "not the upgrade's Windows refusal"
        before = _tree(old_home)
        with pytest.raises(home_migration.MigrationRefused) as refused:
            home_migration.move_home()
        named = f"{prog.word()} launch of claude (pid {process.pid})"
        assert any(named in line for line in refused.value.running), refused.value.running
        assert _tree(old_home) == before
    finally:
        process.kill()
        process.wait()
    assert launchers.exec_sessions() == (), "gone with its agent"
    assert home_migration.move_home() is not None


def test_a_roll_back_takes_the_lock_and_refuses_a_busy_home(
    old_home: Path, user_home: Path
) -> None:
    """m1: --rollback ran with no lock and no look at what runs."""
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, "rewrite", "before")
        with pytest.raises(Crash):
            home_migration.move_home()
    named = _an_instance(user_home / ".rlx")

    result = CliRunner().invoke(app, ["move-home", "--rollback"])

    assert result.exit_code == 1 and named in result.output.replace("\n", " ")
    assert paths.unfinished_home_move() is not None, "nothing undone"
    for name in os.listdir(paths.instances_dir()):
        (paths.instances_dir() / name).unlink()
    assert home_migration.roll_back_move() is True
    assert not os.path.lexists(user_home / ".rlx")


def test_a_move_killed_at_its_timeout_is_said_unfinished_from_the_home_s_state(
    old_home: Path, user_home: Path, installed: Installed, monkeypatch: pytest.MonkeyPatch
) -> None:
    """m2: a run killed at the 600 s timeout said nothing, and was said rolled back."""
    installed.latest = "0.10.3"
    real = install_route.run_captured

    def killed(argv: Any, *, timeout: float) -> Captured:
        if list(argv[1:]) == ["--json", "move-home"]:
            with pytest.MonkeyPatch.context() as crash:
                _crash_in(crash, "rewrite", "before")
                with contextlib.suppress(Crash):
                    home_migration.move_home()
            return Captured(None, error="no answer in 600 s")
        return real(argv, timeout=timeout)

    monkeypatch.setattr(install_route, "run_captured", killed)

    result = CliRunner().invoke(app, ["upgrade", "--yes"])

    assert result.exit_code == 1
    said = result.output.replace("\n", " ")
    assert "the new install's move did not finish (no answer in 600 s)" in said
    assert f"run {prog.word()} upgrade again to finish it" in said
    assert "it is as it was" not in said


def test_after_a_move_at_a_terminal_the_cache_is_offered_and_can_be_dropped(
    old_home: Path,
    user_home: Path,
    installed: Installed,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """m3, the owner's must-handle 5: the regenerable cache is offered to be dropped."""
    (paths.cache_dir() / "big.bin").write_bytes(b"x" * 4096)
    answers = iter([True, True])
    asked: list[str] = []

    def confirm(question: str, default: bool = False) -> bool:
        asked.append(question)
        return next(answers)

    monkeypatch.setattr("aisquare.cli.install._stdin_is_a_terminal", lambda: True)
    monkeypatch.setattr("aisquare.cli.install.typer.confirm", confirm)

    result = CliRunner().invoke(app, ["upgrade"])

    assert result.exit_code == 0, result.output
    assert asked[0].endswith(f"Move its home, {MOVE}?") and asked[1].startswith("Drop its cache")
    assert "✓ the cache was dropped" in result.output
    assert not (user_home / ".rlx" / "cache").exists()


def test_off_a_terminal_the_cache_is_said_and_move_home_drops_it_after(
    old_home: Path, user_home: Path, installed: Installed
) -> None:
    (paths.cache_dir() / "big.bin").write_bytes(b"x" * 4096)

    result = CliRunner().invoke(app, ["upgrade", "--yes"])

    assert result.exit_code == 0, result.output
    assert f"`{prog.word()} move-home --drop-cache` drops it" in result.output.replace("\n", " ")
    assert (user_home / ".rlx" / "cache" / "big.bin").is_file()
    dropped = CliRunner().invoke(app, ["move-home", "--drop-cache"])
    assert dropped.exit_code == 0 and "the cache was dropped" in dropped.output
    assert not (user_home / ".rlx" / "cache").exists()


def test_the_move_renames_and_never_copies(old_home: Path, user_home: Path) -> None:
    """m4: each folder and file keeps its inode across the move (a copy-then-delete mutant
    gives each a new one); the same volume, a rename."""
    picks = {
        "slot": _slot(2),
        "store": old_home / paths.OLD_STORE,
        "cache file": accounts_core.tmp_root() / "2" / ".keep",
        "project": old_home / "projects" / "prj_old",
    }
    before = {name: (os.stat(path).st_dev, os.stat(path).st_ino) for name, path in picks.items()}

    assert home_migration.move_home() is not None

    new = user_home / ".rlx"
    after = {
        "slot": new / "accounts" / "claude" / "2",
        "store": new / "rlx.db",
        "cache file": accounts_core.tmp_root() / "2" / ".keep",
        "project": new / "projects" / "prj_old",
    }
    for name, path in after.items():
        assert (os.stat(path).st_dev, os.stat(path).st_ino) == before[name], name


def test_the_dialog_names_everything_the_move_waits_for() -> None:
    """m7: the dialog counted the frame's fleet agents only; it says what the move's quiet
    check would name, from the plan."""
    from aisquare.cli.ui.update import UpdateScreen

    plan = UpgradePlan(
        route=InstallRoute(install_route.UV_TOOL, UI_FACTS),
        current="0.9.0",
        target=None,
        latest=LatestRelease("0.9.0"),
        argv=("uv",),
        env={},
        reason=None,
        home_move=MOVE,
        home_busy=("the watcher (pid 7)", "1 live pane on rlx's tmux server 'asq'"),
    )
    said = UpdateScreen(plan, 0, "0.9.0", checking=False).body().plain

    assert "2 things run on this home (the watcher (pid 7); 1 live pane" in said
    assert "the home moves only once nothing does" in said


def test_the_plan_reads_what_runs_but_the_service(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    named = _an_instance(old_home)
    assert any(named in line for line in lifecycle._running_on_home())
    monkeypatch.setattr(home_migration, "running_on_home", lambda **k: ["x"] if k else ["y"])
    assert lifecycle._running_on_home() == ("x",), "asked with the service left out"


# --- round 4 (the tester's #2271) ----------------------------------------------------------


@pytest.mark.parametrize(
    "spelling",
    ["as-given", "resolved", "trailing-slash", "an-alias", "rlx-home-only", "aisquare-home-only"],
)
def test_after_the_move_a_variable_naming_aisquare_reads_every_slot_signed_in(
    old_home: Path,
    user_home: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    spelling: str,
) -> None:
    """Blocker 1: a shell rc, or a shell a fleet window started, keeps AISQUARE_HOME naming
    ~/.aisquare. After the move it is the link, and the home read through it spelled each
    slot's folder from ~/.aisquare: its Keychain name hashed a spelling no item has, and both
    Claude slots read signed out. It is the default home: ~/.rlx, every path spelled from it."""
    assert home_migration.move_home() is not None
    old = user_home / ".aisquare"
    alias = tmp_path / "alias"
    alias.symlink_to(old)
    value = {
        "as-given": str(old),
        "resolved": os.path.realpath(old),
        "trailing-slash": f"{old}/",
        "an-alias": str(alias),
        "rlx-home-only": str(old),
        "aisquare-home-only": str(old),
    }[spelling]
    if spelling != "aisquare-home-only":
        monkeypatch.setenv(paths.RLX_HOME_ENV, value)
    if spelling != "rlx-home-only":
        monkeypatch.setenv(paths.HOME_ENV_VAR, value)
    accounts_core.forget_keychain_reads()

    assert paths.home_of() == (user_home / ".rlx", paths.NEW)
    slots = accounts_core.all_managed_accounts()
    assert {a.slot for a in slots} == {2, 3, 4}
    for account in slots:
        assert account.config_dir.is_relative_to(user_home / ".rlx"), account
        assert accounts_core.signed_in(account, fresh=True), account


def test_before_the_move_a_variable_naming_aisquare_reads_it_as_it_is(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv(paths.HOME_ENV_VAR, str(old_home))

    assert paths.home_of() == (old_home, paths.OLD)
    assert paths.db_path() == old_home / paths.OLD_STORE


@pytest.mark.parametrize("variable", [paths.RLX_HOME_ENV, paths.HOME_ENV_VAR])
@pytest.mark.parametrize("place", ["fresh", "a-link-to-nothing"])
def test_a_variable_naming_aisquare_is_read_as_no_variable_on_a_fresh_or_purged_home(
    user_home: Path,
    keychain: FakeSecurity,
    monkeypatch: pytest.MonkeyPatch,
    variable: str,
    place: str,
) -> None:
    """The reviewer's MAJOR 1, round 4: such a variable was read by a rule of its own. On a
    machine with no home yet rlx made ``~/.aisquare`` in the new layout, which a process
    without the variable read as old (a split store). And once ``uninstall --purge`` removed
    ``~/.rlx`` and left ``~/.aisquare`` linking to it, the fleet's ``RLX_HOME=~/.rlx`` named
    ``~/.aisquare`` through the link, and every command refused it as in the way. It is read as
    no variable: the home a plain process reads, before a command and after it."""
    old, new = user_home / ".aisquare", user_home / ".rlx"
    values = [str(old), f"{old}/"]
    if place == "a-link-to-nothing":
        old.symlink_to(new)  # what the purge leaves once the home has moved
        values.append(str(new))  # every fleet window's RLX_HOME
    plain = paths.home_of({})
    assert plain == (new, paths.NEW)
    runner = CliRunner()

    for value in values:
        monkeypatch.setenv(variable, value)
        assert paths.home_of() == plain, value
        for command in (["fleet", "ls"], ["accounts", "list"]):
            result = runner.invoke(app, command)
            assert result.exit_code == 0, (value, command, result.output)
        assert paths.home_of({}) == plain == paths.home_of(), value
        assert paths.db_path().is_relative_to(new), value
    assert old.is_symlink() if place == "a-link-to-nothing" else not os.path.lexists(old)


def test_the_cache_is_never_dropped_while_anything_runs_on_the_home(
    old_home: Path, user_home: Path
) -> None:
    """Blocker 2: after the move, `move-home --drop-cache` (which the upgrade tells a person to
    run later) dropped the cache with no look at what runs, and the cache holds each slot's
    CLAUDE_CODE_TMPDIR: a live session's scratch went with it."""
    assert home_migration.move_home() is not None
    scratch = accounts_core.tmp_root() / "2" / "session" / "output.txt"
    scratch.parent.mkdir(parents=True)
    scratch.write_text("a background task's output", encoding="utf-8")
    named = _an_instance(user_home / ".rlx")

    refused = CliRunner().invoke(app, ["move-home", "--drop-cache"])

    assert refused.exit_code == 1
    said = refused.output.replace("\n", " ")
    assert "the cache was not dropped" in said and named in said
    assert scratch.is_file(), "the live session's scratch is still there"
    for name in os.listdir(paths.instances_dir()):
        (paths.instances_dir() / name).unlink()
    dropped = CliRunner().invoke(app, ["move-home", "--drop-cache"])
    assert dropped.exit_code == 0, dropped.output
    assert not scratch.exists()


def test_the_offer_after_a_move_refuses_too_while_anything_runs(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from aisquare.cli import install as install_cli

    assert home_migration.move_home() is not None
    scratch = accounts_core.tmp_root() / "2" / "keep.txt"
    scratch.write_text("x" * 64, encoding="utf-8")
    named = _an_instance(user_home / ".rlx")
    said: list[str] = []
    monkeypatch.setattr(install_cli, "_stdin_is_a_terminal", lambda: True)
    monkeypatch.setattr("aisquare.cli.install.typer.confirm", lambda *a, **k: True)
    monkeypatch.setattr(install_cli, "_say", said.append)

    install_cli._offer_the_cache(yes=False)

    assert any("the cache was not dropped" in line and named in line for line in said), said
    assert scratch.is_file()


def test_aisquare_made_again_before_the_link_is_kept_aside_and_the_link_made(
    old_home: Path, user_home: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """N2: something made ~/.aisquare again between the root's rename and the link (an
    asq-statusline under the variable); the move finished unlinked, and then read as both homes
    being there."""
    real = home_migration._MOVE_STEPS["link"]

    def made_again(journal: home_migration.Journal) -> None:
        stray = user_home / ".aisquare" / "cache" / "statusline" / "x.json"
        stray.parent.mkdir(parents=True)
        stray.write_text("{}", encoding="utf-8")
        real(journal)

    monkeypatch.setitem(home_migration._MOVE_STEPS, "link", made_again)

    moved = home_migration.move_home()

    assert moved is not None and moved.linked
    assert (user_home / ".aisquare").is_symlink()
    kept = [p for p in user_home.iterdir() if p.name.startswith(".aisquare-made-during-the-move")]
    assert len(kept) == 1 and (kept[0] / "cache" / "statusline" / "x.json").is_file()
    assert any("was made again during the move" in note for note in moved.notes)
    assert home_migration.planned_move() is None


def test_upgrade_check_on_a_home_part_way_through_its_move_says_so(
    old_home: Path, user_home: Path, installed: Installed
) -> None:
    """N4: it said the home would move, as of a home not begun."""
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, "rewrite", "before")
        with pytest.raises(Crash):
            home_migration.move_home()

    result = CliRunner().invoke(app, ["upgrade", "--check"])

    assert result.exit_code == 0, result.output
    assert "part-way: a run of the move stopped and the next finishes it" in result.output.replace(
        "\n", " "
    )


# --- round 4, the reviewer's #2295 -----------------------------------------------------------


#: Every way ``os`` replaces a process.
OS_EXECS = frozenset(
    {"execv", "execve", "execvp", "execvpe", "execl", "execle", "execlp", "execlpe"}
)

#: Each ``os.exec*`` of the package that execs no agent, by module and function, and why it
#: needs no record (the manager's m1 on #31: allowed by name, with the reason).
EXECS_OF_NO_AGENT = {
    "cli/fleet.py::_exec_attach": "`tmux attach`: a client of the fleet's tmux server, whose "
    "panes the quiet check reads from the server itself",
    "core/selfcli.py::exec_self": "rlx itself on this install (the app's Update and Uninstall, "
    "`upgrade --reopen`), which the move's guard and lock hold to as any rlx command",
    "services/install_route.py::exec_replace": "the package manager removing rlx, "
    "uninstall's last step: no agent and no rlx left running",
}


def _execs_with_no_record_first(package: Path) -> list[str]:
    """Each ``os.exec*`` under ``package`` with no ``launchers.execing`` before it in its own
    function, but those :data:`EXECS_OF_NO_AGENT` names; and any ``from os import exec*``,
    which would hide one from this reading."""
    import ast

    found: list[str] = []
    for module in sorted(package.rglob("*.py")):
        name = module.relative_to(package).as_posix()
        tree = ast.parse(module.read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if (
                isinstance(node, ast.ImportFrom)
                and node.module == "os"
                and any(alias.name in OS_EXECS for alias in node.names)
            ):
                found.append(f"{name}: from os import {[a.name for a in node.names]}")
            if not isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
                continue
            calls = sorted(
                (call for call in ast.walk(node) if isinstance(call, ast.Call)),
                key=lambda call: (call.lineno, call.col_offset),
            )
            recorded = False
            for call in calls:
                func = call.func
                if not (isinstance(func, ast.Attribute) and isinstance(func.value, ast.Name)):
                    continue
                if (func.value.id, func.attr) == ("launchers", "execing"):
                    recorded = True
                elif func.value.id == "os" and func.attr in OS_EXECS and not recorded:
                    where = f"{name}::{node.name}"
                    if where not in EXECS_OF_NO_AGENT:
                        found.append(f"{where}: os.{func.attr} at line {call.lineno}")
    return found


def _execing_commands(package: Path) -> dict[str, str]:
    """Each ``launchers.execing`` call under ``package``, by module and function: the command
    it records (``launch`` when it names none)."""
    import ast

    found: dict[str, str] = {}
    for module in sorted(package.rglob("*.py")):
        tree = ast.parse(module.read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if not isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
                continue
            for call in ast.walk(node):
                func = call.func if isinstance(call, ast.Call) else None
                if not (
                    isinstance(call, ast.Call)
                    and isinstance(func, ast.Attribute)
                    and isinstance(func.value, ast.Name)
                    and (func.value.id, func.attr) == ("launchers", "execing")
                ):
                    continue
                given = [k.value for k in call.keywords if k.arg == "command"]
                command = given[0].value if given and isinstance(given[0], ast.Constant) else None
                found[f"{module.relative_to(package).as_posix()}::{node.name}"] = str(
                    command or "launch"
                )
    return found


def test_every_exec_of_an_agent_records_itself_first() -> None:
    """MAJOR 1: `accounts run` and `team spawn --exec` exec'd agents on managed slots with no
    record, and the move ran under them. The reviewer's m1: the guard read only os.execve and
    os.execvpe, so an execvp with no record passed, and took a record anywhere in the function.
    Every os.exec* of the package now comes after launchers.execing in its own function, but
    the three that exec no agent, each named with why."""
    package = Path(__file__).resolve().parents[1] / "src" / "aisquare"
    assert _execs_with_no_record_first(package) == []
    for where in EXECS_OF_NO_AGENT:
        module, function = where.split("::")
        assert f"def {function}(" in (package / module).read_text(encoding="utf-8"), where
    # The reviewer's n1: each names the command that started it ("rlx launch of sleep" was an
    # `accounts run`). A new path that execs an agent is added here, with its own name.
    assert _execing_commands(package) == {
        "cli/launch.py::_exec": "launch",
        "cli/accounts.py::_exec": "accounts run",
        "cli/team.py::spawn": "team spawn",
    }


@pytest.mark.parametrize(
    ("body", "caught"),
    [
        ("    os.execvp(argv[0], argv)\n", True),
        ("    os.execl('/bin/agent', 'agent')\n", True),
        ("    os.execvp(argv[0], argv)\n    launchers.execing('claude')\n", True),
        ("    launchers.execing('claude')\n    os.execvp(argv[0], argv)\n", False),
    ],
    ids=["execvp-with-no-record", "execl-with-no-record", "a-record-after", "a-record-before"],
)
def test_the_exec_guard_catches_any_exec_with_no_record_before_it(
    tmp_path: Path, body: str, caught: bool
) -> None:
    """The reviewer's surviving mutant: an execvp in `team spawn` with no record."""
    (tmp_path / "cli").mkdir()
    (tmp_path / "cli" / "team.py").write_text(f"def _spawn(argv):\n{body}", encoding="utf-8")
    (tmp_path / "cli" / "other.py").write_text("from os import execv\n", encoding="utf-8")

    found = _execs_with_no_record_first(tmp_path)

    assert ("cli/team.py::_spawn" in " ".join(found)) is caught, found
    assert any("from os import" in why for why in found), found


def test_an_agent_accounts_run_execs_is_counted_by_the_quiet_check(
    old_home: Path, user_home: Path
) -> None:
    import subprocess

    script = (
        "import os, sys\n"
        "from aisquare.cli import accounts\n"
        "sys.stdout.write('ready\\n'); sys.stdout.flush()\n"
        "accounts._exec('/bin/sleep', ['sleep', '60'], dict(os.environ))\n"
    )
    process = subprocess.Popen(
        [sys.executable, "-c", script], stdout=subprocess.PIPE, env=dict(os.environ)
    )
    try:
        assert process.stdout is not None and process.stdout.readline() == b"ready\n"
        for _ in range(100):  # until the exec has happened and the lock is the agent's
            if home_migration.running_on_home():
                break
            import time

            time.sleep(0.05)
        with pytest.raises(home_migration.MigrationRefused) as refused:
            home_migration.move_home()
        named = f"rlx accounts run of sleep (pid {process.pid})"
        assert any(line.startswith(named) for line in refused.value.running), refused.value
    finally:
        process.kill()
        process.wait()


#: Nested past what ``json.loads`` takes: a ``RecursionError``, which is no ``ValueError``.
TOO_DEEP = b"[" * 100_000 + b"]" * 100_000

#: What an open move's journal holds, readable.
OPEN_JOURNAL = json.dumps({"version": 1, "steps": ["backup"]})


def _bytes(body: bytes) -> Callable[[Path, pytest.MonkeyPatch], None]:
    def make(journal: Path, patch: pytest.MonkeyPatch) -> None:
        journal.write_bytes(body)

    return make


def _a_folder(journal: Path, patch: pytest.MonkeyPatch) -> None:
    journal.mkdir()


def _a_link_to_nothing(journal: Path, patch: pytest.MonkeyPatch) -> None:
    journal.symlink_to(journal.with_name("gone.json"))


def _mode_000(journal: Path, patch: pytest.MonkeyPatch) -> None:
    if getattr(os, "geteuid", lambda: 1)() == 0:
        pytest.skip("root opens any file")
    journal.write_text(OPEN_JOURNAL, encoding="utf-8")
    journal.chmod(0)


def _an_io_error(journal: Path, patch: pytest.MonkeyPatch) -> None:
    """A disk that fails the read: each reader of the journal reads it through
    ``paths.read_regular_text``."""
    journal.write_text(OPEN_JOURNAL, encoding="utf-8")
    real = paths.read_regular_text

    def read(path: Path) -> str:
        if path == journal:
            raise OSError(errno.EIO, os.strerror(errno.EIO), str(path))
        return real(path)

    patch.setattr(paths, "read_regular_text", read)


@pytest.mark.parametrize("moved", [False, True], ids=["before-the-move", "after-the-move"])
@pytest.mark.parametrize(
    ("make", "said"),
    [
        (_bytes(b"{not json"), "cannot read"),
        (_bytes(b"{}"), "cannot read"),
        (_bytes(json.dumps({"version": 99, "steps": ["backup"]}).encode()), "a newer rlx's move"),
        (_bytes(b"\xff\xfe\xfa"), "cannot read (UnicodeDecodeError)"),
        (
            _bytes(b'{"version": 1, "steps": ["backup"], "old_root": "\xff\xfe"}'),
            "cannot read (UnicodeDecodeError)",
        ),
        (_bytes(TOO_DEEP), "cannot read (RecursionError)"),
        (_a_folder, "cannot read (not a regular file)"),
        (_a_link_to_nothing, "cannot read (a link to nothing)"),
        (_mode_000, "cannot read (Permission denied)"),
        (_an_io_error, f"cannot read ({os.strerror(errno.EIO)})"),
    ],
    ids=[
        "garbage",
        "no-journal-shape",
        "a-newer-rlx",
        "bad-utf8",
        "bad-utf8-in-json",
        "deep",
        "a-folder",
        "a-link-to-nothing",
        "mode-000",
        "an-io-error",
    ],
)
def test_a_journal_this_rlx_cannot_take_up_is_refused_with_its_way_out_everywhere(
    old_home: Path,
    user_home: Path,
    installed: Installed,
    monkeypatch: pytest.MonkeyPatch,
    make: Callable[[Path, pytest.MonkeyPatch], None],
    said: str,
    moved: bool,
) -> None:
    """MAJOR 2: the guard read such a journal as open and the move read it as none, so every
    command refused and the move said there was nothing to do: locked out for good. One reading
    for both, and the way out said; once the file is moved aside rlx runs again.

    Round 4's tester: bytes that are not UTF-8, or nesting too deep to parse, escaped that
    reading as a traceback from every command, the hook's exit 1 failing each of Claude Code's
    hook events, in the old home and in the moved one alike. And the manager's ruling on N2
    (#2948): anything at the journal's path that cannot be read is unfinished, in both readers:
    a folder, a link to nothing, a file that cannot be opened, a read that fails.

    Round 6 (the reviewer's MAJOR 1, round 5): with its copy gone too, the way out is the one
    the home's state allows. Only a home that shows nothing moved is told to move the journal
    aside. A moved one is told to leave the home as it is, and one this user may not read is
    told to make it readable again."""
    if moved:
        assert home_migration.move_home() is not None
    journal = (user_home / ".rlx" if moved else old_home).joinpath(*paths.MOVE_JOURNAL)
    journal.parent.mkdir(parents=True, exist_ok=True)
    journal.unlink(missing_ok=True)
    journal.with_name(home_migration.JOURNAL_COPY).unlink(missing_ok=True)
    make(journal, monkeypatch)
    runner = CliRunner()
    newer = "a newer rlx" in said
    if "Permission denied" in said:
        advice = ["readable again"]
    elif moved:
        advice = [] if newer else ["made, though maybe not finished", "leave the home as it is"]
    else:
        advice = ["aside"] if newer else ["nothing moved yet", "aside"]

    for command in (
        ["fleet", "ls"],
        ["doctor"],
        ["move-home"],
        ["move-home", "--rollback"],
        ["move-home", "--dry-run"],
    ):
        result = runner.invoke(app, command)
        assert result.exit_code == 1, (command, result.output)
        assert isinstance(result.exception, SystemExit), (command, result.exception)
        out = result.output.replace("\n", " ")
        assert said in out and all(words in out for words in advice), (command, out)
        assert not moved or "aside" not in out, (command, out)
    as_json = runner.invoke(app, ["--json", "status"])
    assert as_json.exit_code == 1 and json.loads(as_json.stdout)["error"] == "home_mid_move"
    check = runner.invoke(app, ["upgrade", "--check"])
    assert check.exit_code == 0 and said in check.output.replace("\n", " "), check.output
    hook = runner.invoke(app, ["hook", "session-start"], input=json.dumps({"session_id": SESSION}))
    assert hook.exit_code == 0 and hook.output == "", (hook.output, hook.exception)

    journal.rename(journal.with_name("journal.json.aside"))
    assert runner.invoke(app, ["fleet", "ls"]).exit_code == 0


def test_only_a_home_whose_root_cannot_be_looked_at_reads_as_no_move(
    old_home: Path, user_home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The manager's narrowing (#31 round 6, the reviewer's m1): a move stopped before its
    rewrite, then backups/ at mode 000, read as no move: commands ran on the half-moved home,
    and the move said there was nothing to do. A folder inside the home that cannot be looked
    into is unfinished, refused with the way out; only a home whose ROOT cannot be looked at
    is no move, as uninstall keeps such a home (round 19 of #257)."""
    if getattr(os, "geteuid", lambda: 1)() == 0:
        pytest.skip("root looks into any folder")
    assert paths.unfinished_home_move() is None
    (user_home / "no-backups-folder").mkdir()
    (user_home / "no-backups-folder" / "backups").write_text("", encoding="utf-8")
    assert paths.unfinished_home_move_at(user_home / "no-backups-folder") is None
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, "rewrite", "before")
        with pytest.raises(Crash):
            home_migration.move_home()
    new = user_home / ".rlx"
    journal = new.joinpath(*paths.MOVE_JOURNAL)
    runner = CliRunner()
    for folder in (new / "backups", journal.parent):
        folder.chmod(0)
        try:
            assert paths.unfinished_home_move() == journal, folder
            refused = runner.invoke(app, ["fleet", "ls"])
            move = runner.invoke(app, ["move-home"])
            hook = runner.invoke(
                app, ["hook", "session-start"], input=json.dumps({"session_id": SESSION})
            )
        finally:
            folder.chmod(0o700)
        for result in (refused, move):
            out = result.output.replace("\n", " ")
            assert result.exit_code == 1 and "(Permission denied)" in out, out
            assert "readable again" in out and "aside" not in out, out
        assert hook.exit_code == 0 and hook.output == ""

    # #257's case: the home's root itself behind a folder this user cannot enter.
    locked = tmp_path / "locked"
    named = locked / "inner" / "home"
    named.joinpath(*paths.MOVE_JOURNAL[:-1]).mkdir(parents=True)
    named.joinpath(*paths.MOVE_JOURNAL).write_text(OPEN_JOURNAL, encoding="utf-8")
    locked.chmod(0)
    try:
        assert paths.unfinished_home_move_at(named) is None
    finally:
        locked.chmod(0o700)
    assert paths.unfinished_home_move_at(named) == named.joinpath(*paths.MOVE_JOURNAL)

    assert runner.invoke(app, ["move-home"]).exit_code == 0, "readable again: it finishes"
    assert paths.home_of() == (new, paths.NEW) and paths.unfinished_home_move() is None


@pytest.mark.parametrize("moved", [False, True], ids=["before-the-move", "after-the-move"])
def test_a_fifo_at_the_journal_s_path_is_refused_and_hangs_nothing(
    old_home: Path, user_home: Path, moved: bool
) -> None:
    """The tester's NB1 (round 5): a FIFO at the journal's path hung every command and the hook
    in its open. Only a regular file is read; anything else there is unreadable, so unfinished,
    and refused with the way out."""
    import threading

    if moved:
        assert home_migration.move_home() is not None
    journal = (user_home / ".rlx" if moved else old_home).joinpath(*paths.MOVE_JOURNAL)
    journal.parent.mkdir(parents=True, exist_ok=True)
    journal.unlink(missing_ok=True)
    journal.with_name(home_migration.JOURNAL_COPY).unlink(missing_ok=True)
    if sys.platform != "win32":  # the file is POSIX-only; mypy's win32 run is told so
        os.mkfifo(journal)
    answers: list[object] = []

    def read() -> None:
        answers.append(paths.unfinished_home_move())
        answers.append(home_migration.journal_refusal(journal))

    reader = threading.Thread(target=read, daemon=True)
    reader.start()
    reader.join(10)

    assert not reader.is_alive(), "a read of the FIFO blocked"
    assert answers[0] == journal and "cannot read (not a regular file)" in str(answers[1])
    runner = CliRunner()
    for command in (["fleet", "ls"], ["move-home"], ["move-home", "--rollback"]):
        result = runner.invoke(app, command)
        assert result.exit_code == 1 and "not a regular file" in result.output, result.output
    hook = runner.invoke(app, ["hook", "session-start"], input=json.dumps({"session_id": SESSION}))
    assert hook.exit_code == 0 and hook.output == ""


def test_a_named_home_s_move_lock_holds_against_another_spelling_of_it(
    user_home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The tester's NB2 (round 5): a named home's lock was keyed by its spelling, so the same
    home through a link took a lock of its own and could run a second move beside the first.
    It is keyed by the real path."""
    real = make_old_home(tmp_path / "named-user")
    named = tmp_path / "named-home"
    real.rename(named)
    link = tmp_path / "a-link-to-it"
    link.symlink_to(named)
    monkeypatch.setenv(paths.RLX_HOME_ENV, str(link))
    found = home_migration.planned_move()
    assert found is not None and found.named and found.old_root == link

    with home_migration._one_at_a_time(str(named), named=True):
        with pytest.raises(home_migration.MigrationRefused, match="another move of this home"):
            home_migration.move_home()
        with pytest.raises(home_migration.MigrationRefused, match="a drop of its cache"):
            home_migration.drop_cache()

    assert not named.joinpath(*paths.MOVE_JOURNAL).exists(), "nothing began"


def _stopped_at_rename(n: int) -> None:
    """A move run until its ``n``-th rename is made (0: none) and stopped there, before the
    journal writes it down."""
    made: list[int] = []
    real = home_migration._rename_once

    def rename(source: Path, target: Path) -> None:
        real(source, target)
        made.append(1)
        if len(made) == n:
            raise Crash(f"after rename {n}")

    with pytest.MonkeyPatch.context() as crash:
        if n == 0:
            _crash_in(crash, "move", "before")
        else:
            crash.setattr(home_migration, "_rename_once", rename)
        with pytest.raises(Crash):
            home_migration.move_home()


def _whole(user_home: Path) -> None:
    """The home moved and whole: in the new layout, linked, every slot signed in."""
    assert paths.home_of() == (user_home / ".rlx", paths.NEW)
    assert (user_home / ".aisquare").is_symlink()
    assert paths.unfinished_home_move() is None and home_migration.planned_move() is None
    accounts_core.forget_keychain_reads()
    for slot in (2, 4):
        assert accounts_core.signed_in(accounts_core._managed(slot), fresh=True), slot
    assert _accepted() == {"agt-captain", "agt-coder"}


@pytest.mark.parametrize("n", [1, 2, 3, 5, 8])
def test_a_journal_unreadable_after_a_stop_inside_the_renames_is_finished_from_its_copy(
    old_home: Path, user_home: Path, n: int
) -> None:
    """The reviewer's MAJOR 1 (round 5), its probe: a move stopped at rename #n, its journal
    then unreadable. The docs' "nothing moved" matched from #3 on, and following it (the
    journal aside) lost every slot from rlx's view and locked every later move out. The
    journal's copy takes it up: never "nothing moved", and `rlx move-home` ends whole."""
    _stopped_at_rename(n)
    journal = paths.unfinished_home_move()
    assert journal is not None
    journal.write_bytes(b"\xff\xfe not a journal")
    runner = CliRunner()

    refused = runner.invoke(app, ["fleet", "ls"])

    out = refused.output.replace("\n", " ")
    assert refused.exit_code == 1 and "part-way through its move" in out, out
    assert "nothing moved" not in out and "aside" not in out, out
    done = runner.invoke(app, ["move-home"])
    assert done.exit_code == 0, done.output
    _whole(user_home)
    moved = (user_home / ".rlx").joinpath(*paths.MOVE_JOURNAL)
    copy = moved.with_name(home_migration.JOURNAL_COPY)
    assert json.loads(moved.read_text(encoding="utf-8")) == json.loads(
        copy.read_text(encoding="utf-8")
    ), "the journal written whole again"


@pytest.mark.parametrize("n", [0, 1, 3, 8])
def test_with_both_copies_unreadable_only_a_home_that_shows_nothing_moved_is_told_aside(
    old_home: Path, user_home: Path, n: int
) -> None:
    """Both the journal and its copy unreadable: the way out is the one the home shows. Only
    nothing moved gets "move that file aside", and following it then ends whole; part-way is
    never called nothing moved, and is never told to move its journal aside."""
    _stopped_at_rename(n)
    journal = paths.unfinished_home_move()
    assert journal is not None
    journal.write_bytes(b"\xff\xfe")
    journal.with_name(home_migration.JOURNAL_COPY).write_bytes(b"\xff\xfe")
    runner = CliRunner()

    refused = runner.invoke(app, ["fleet", "ls"])

    out = refused.output.replace("\n", " ")
    assert refused.exit_code == 1 and "cannot read (UnicodeDecodeError)" in out, out
    if n == 0:
        assert "nothing moved yet" in out and "aside" in out, out
        journal.rename(journal.with_name("journal.json.aside"))
        done = runner.invoke(app, ["move-home"])
        assert done.exit_code == 0, done.output
        _whole(user_home)
    else:
        assert "the move part-way" in out and "nothing moved" not in out, out
        assert "aside" not in out and "leave the home as it is" in out, out


def _asides(folder: Path) -> list[str]:
    """What the move set aside in its folder (``ASIDE_MARK``), by name."""
    return sorted(p.name for p in folder.iterdir() if home_migration.ASIDE_MARK in p.name)


@pytest.mark.parametrize("then", ["move-home", "rollback"])
def test_a_folder_at_the_journal_s_path_part_way_is_set_aside_and_the_way_out_works(
    old_home: Path, user_home: Path, then: str
) -> None:
    """The tester's repro A (round 6): a folder at journal.json with the copy readable. Every
    command said `rlx move-home` finishes it, and move-home and --rollback each ended in an
    IsADirectoryError traceback, run after run. The folder is set aside, never deleted, and
    each way out ends whole, or as it was."""
    before = _tree(old_home)
    _stopped_at_rename(3)
    journal = paths.unfinished_home_move()
    assert journal is not None
    journal.unlink()
    journal.mkdir()
    runner = CliRunner()
    assert "part-way through its move" in runner.invoke(app, ["fleet", "ls"]).output

    result = runner.invoke(
        app, ["move-home"] if then == "move-home" else ["move-home", "--rollback"]
    )

    assert result.exit_code == 0 and result.exception is None, (result.output, result.exception)
    if then == "move-home":
        _whole(user_home)
        assert _asides((user_home / ".rlx").joinpath(*paths.MOVE_JOURNAL[:-1])), "kept aside"
    else:
        assert _same_but_store(_tree(old_home), before) and not (user_home / ".rlx").exists()
        assert _asides(old_home.joinpath(*paths.MOVE_JOURNAL[:-1])), "kept aside"
    assert runner.invoke(app, ["fleet", "ls"]).exit_code == 0


def test_a_folder_at_a_finished_move_s_journal_is_set_aside_by_move_home(
    old_home: Path, user_home: Path
) -> None:
    """Repro A after a finished move: fleet ls stayed refused, and move-home was a traceback."""
    assert home_migration.move_home() is not None
    journal = (user_home / ".rlx").joinpath(*paths.MOVE_JOURNAL)
    journal.unlink()
    journal.mkdir()
    runner = CliRunner()
    assert runner.invoke(app, ["fleet", "ls"]).exit_code == 1

    done = runner.invoke(app, ["move-home"])

    assert done.exit_code == 0 and done.exception is None, (done.output, done.exception)
    assert runner.invoke(app, ["fleet", "ls"]).exit_code == 0
    assert _asides(journal.parent) and journal.is_file()
    _whole(user_home)


def test_a_folder_at_the_copy_s_path_before_any_move_is_set_aside_and_the_home_moves(
    old_home: Path, user_home: Path
) -> None:
    """The tester's repro B: a folder at journal-copy.json on a home never moved. The move
    refused, left journal.json behind, and every later run was a traceback."""
    folder = old_home.joinpath(*paths.MOVE_JOURNAL[:-1])
    (folder / home_migration.JOURNAL_COPY).mkdir(parents=True)
    runner = CliRunner()
    assert runner.invoke(app, ["fleet", "ls"]).exit_code == 0

    done = runner.invoke(app, ["move-home"])

    assert done.exit_code == 0 and done.exception is None, (done.output, done.exception)
    _whole(user_home)
    assert _asides((user_home / ".rlx").joinpath(*paths.MOVE_JOURNAL[:-1])), "kept aside"


def _failing_writes(
    patch: pytest.MonkeyPatch, errno_: int, *, names: tuple[str, ...], after: int = 0
) -> None:
    """A FAKE writer for the journal: each write of a file named in ``names`` past the first
    ``after`` of them fails with ``errno_``, as a full disk or a bad sector would (never by
    filling a disk). Every other write is the real one."""
    from aisquare.core.atomic import write_replacing as real

    seen = [0]

    def write(path: Path, *args: Any, **kwargs: Any) -> Any:
        if Path(path).name in names:
            seen[0] += 1
            if seen[0] > after:
                raise OSError(errno_, os.strerror(errno_), str(path))
        return real(path, *args, **kwargs)

    patch.setattr(home_migration, "write_replacing", write)


JOURNALS = (paths.MOVE_JOURNAL[-1], home_migration.JOURNAL_COPY)


@pytest.mark.parametrize("errno_", [errno.ENOSPC, errno.EIO], ids=["ENOSPC", "EIO"])
def test_a_fresh_move_whose_copy_cannot_be_written_leaves_no_journal_and_is_not_moved(
    old_home: Path, user_home: Path, errno_: int
) -> None:
    """The tester's round-6 note: the copy's write failing left journal.json behind, so the
    home read part-way though 'the home was not moved', and the next run was a traceback."""
    before = _tree(old_home)
    runner = CliRunner()
    with pytest.MonkeyPatch.context() as disk:
        _failing_writes(disk, errno_, names=(home_migration.JOURNAL_COPY,))
        refused = runner.invoke(app, ["move-home"])

    out = refused.output.replace("\n", " ")
    assert refused.exit_code == 1 and isinstance(refused.exception, SystemExit), out
    assert "the home was not moved" in out and os.strerror(errno_) in out, out
    assert paths.unfinished_home_move() is None and _tree(old_home) == before
    assert not old_home.joinpath(*paths.MOVE_JOURNAL[:-1]).exists()
    assert runner.invoke(app, ["fleet", "ls"]).exit_code == 0
    assert runner.invoke(app, ["move-home"]).exit_code == 0
    _whole(user_home)


@pytest.mark.parametrize("errno_", [errno.ENOSPC, errno.EIO], ids=["ENOSPC", "EIO"])
def test_a_resume_that_cannot_write_its_journal_names_the_file_and_the_next_run_finishes(
    old_home: Path, user_home: Path, errno_: int
) -> None:
    _stopped_at_rename(3)
    runner = CliRunner()
    with pytest.MonkeyPatch.context() as disk:
        _failing_writes(disk, errno_, names=JOURNALS)
        refused = runner.invoke(app, ["move-home"])

    out = refused.output.replace("\n", " ")
    assert refused.exit_code == 1 and isinstance(refused.exception, SystemExit), out
    assert "journal.json could not be written" in out and os.strerror(errno_) in out, out
    assert "run the move again: it goes on from where it stopped" in out, out
    done = runner.invoke(app, ["move-home"])
    assert done.exit_code == 0, done.output
    _whole(user_home)


@pytest.mark.parametrize("after", [0, 2], ids=["its-first-write", "a-later-write"])
def test_a_roll_back_that_cannot_write_names_the_file_and_the_next_finishes_it(
    old_home: Path, user_home: Path, after: int
) -> None:
    before = _tree(old_home)
    _stopped_at_rename(3)
    runner = CliRunner()
    with pytest.MonkeyPatch.context() as disk:
        _failing_writes(disk, errno.EIO, names=JOURNALS, after=after)
        refused = runner.invoke(app, ["move-home", "--rollback"])

    out = refused.output.replace("\n", " ")
    assert refused.exit_code == 1 and isinstance(refused.exception, SystemExit), out
    assert "was not rolled back" in out and "could not be written (Input/output error)" in out
    assert "roll it back again: it goes on from where it stopped" in out, out
    undone = runner.invoke(app, ["move-home", "--rollback"])
    assert undone.exit_code == 0, undone.output
    assert _same_but_store(_tree(old_home), before) and not (user_home / ".rlx").exists()


def test_a_newer_rlx_s_copy_is_refused_as_its_move_never_resumed(
    old_home: Path, user_home: Path
) -> None:
    """The reviewer's m1 (round 6): an unreadable journal and a copy a newer rlx wrote. The
    copy was taken up and the newer release's move resumed by this one, its newer fields
    dropped. It is that release's move: refused, with its way out."""
    _stopped_at_rename(3)
    journal = paths.unfinished_home_move()
    assert journal is not None
    copy = journal.with_name(home_migration.JOURNAL_COPY)
    newer = json.loads(copy.read_text(encoding="utf-8"))
    newer["version"] = 99
    copy.write_text(json.dumps(newer), encoding="utf-8")
    journal.write_bytes(b"\xff\xfe")
    held = copy.read_bytes()
    runner = CliRunner()

    for command in (["fleet", "ls"], ["move-home"], ["move-home", "--rollback"]):
        result = runner.invoke(app, command)
        out = result.output.replace("\n", " ")
        assert result.exit_code == 1 and "a newer rlx's move" in out, (command, out)
        assert "finish it with that release" in out and "finishes it" not in out, out

    assert copy.read_bytes() == held and journal.read_bytes() == b"\xff\xfe", "never resumed"


def test_a_finished_move_whose_journal_rotted_is_written_whole_again_by_move_home(
    old_home: Path, user_home: Path
) -> None:
    """The journal is kept until the next release. Rotted after the move finished, its copy
    says done: every command says the move is part-way until `rlx move-home` writes the
    journal again from the copy, and rlx runs."""
    assert home_migration.move_home() is not None
    journal = (user_home / ".rlx").joinpath(*paths.MOVE_JOURNAL)
    journal.write_bytes(b"\x00\x00")
    runner = CliRunner()
    assert runner.invoke(app, ["fleet", "ls"]).exit_code == 1

    assert runner.invoke(app, ["move-home"]).exit_code == 0

    assert paths.unfinished_home_move() is None
    assert runner.invoke(app, ["fleet", "ls"]).exit_code == 0
    _whole(user_home)


@pytest.mark.parametrize(
    "command",
    [
        ["uninstall"],
        ["fleet", "reap", "--finished", "--dry-run"],
        ["project", "fresh", "--dry-run"],
    ],
    ids=["uninstall-not-dry", "another-dry-run", "a-third-dry-run"],
)
def test_mid_move_only_uninstall_s_dry_run_passes_as_only_asking(
    old_home: Path, user_home: Path, command: list[str]
) -> None:
    """The reviewer's m4: a guard that let every --dry-run through, or every uninstall, was
    not caught. Only uninstall's dry run (and a help page) asks without changing a file."""
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, "rewrite", "before")
        with pytest.raises(Crash):
            home_migration.move_home()

    result = CliRunner().invoke(app, command, input="n\n")

    assert result.exit_code == 1 and "part-way through its move" in result.output, result.output


def test_mid_move_help_and_uninstall_s_dry_run_still_answer_and_a_dry_run_says_part_way(
    old_home: Path, user_home: Path
) -> None:
    with pytest.MonkeyPatch.context() as crash:
        _crash_in(crash, "rewrite", "before")
        with pytest.raises(Crash):
            home_migration.move_home()
    runner = CliRunner()

    assert runner.invoke(app, ["fleet", "--help"]).exit_code == 0
    assert "part-way through its move" not in runner.invoke(app, ["uninstall", "--dry-run"]).output
    dry = runner.invoke(app, ["move-home", "--dry-run"])
    assert dry.exit_code == 0 and "the home's move is part-way (a run stopped)" in dry.output
