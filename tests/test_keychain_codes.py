"""``accounts list`` says each slot's Keychain as a code (handoff 0.10.3, 4.3).

rlxapp never reads the Keychain itself: rlx's ``/usr/bin/security`` is the only reader
(E2), so the app needs to be told why a read failed to say the fix. Each slot of
``rlx --json accounts list`` carries ``keychain``, one of ``core.KeychainCode``:

- ``ok``: the Keychain answered (the item, or that there is none);
- ``locked``: errSecInteractionNotAllowed (36) or errSecUserCanceled (128): "unlock your
  login keychain";
- ``denied``: errSecAuthFailed (51), Deny on the item's access prompt: "allow access in
  Keychain Access";
- ``timeout``: ``security`` gave no answer in time;
- ``failed``: any other exit, or no ``security`` to run: never folded into ``ok``;
- ``none``: no Keychain is read for the slot, off macOS or for a Codex slot.

The ``security`` here is a stand-in that runs on all three OSes. ``core.KEYCHAIN_TOOL`` is
this interpreter (an absolute path, so nothing is looked up on PATH, and no PATHEXT is
needed on Windows), and the reader's first argument, ``find-generic-password``, is the
script it runs, found in the working directory the test moves to. It answers each service
from a table and logs its arguments. It prints no words a reader could classify, so the
exit code alone decides each code.
"""

from __future__ import annotations

import json
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import claude_accounts as core
from aisquare.models import ClaudeAccount
from tests.test_claude_accounts import _installed, _sign_in

STAND_IN = """\
import json, pathlib, sys, time
here = pathlib.Path(__file__).resolve().parent
args = sys.argv[1:]
with open(here / "calls.jsonl", "a", encoding="utf-8") as log:
    log.write(json.dumps(args) + "\\n")
table = json.loads((here / "answers.json").read_text(encoding="utf-8"))
answer = table.get(args[args.index("-s") + 1], {"exit": 44})
time.sleep(answer.get("sleep", 0))
sys.stdout.write(answer.get("stdout", ""))
sys.stderr.write(answer.get("stderr", ""))
sys.exit(answer.get("exit", 0))
"""

ITEM = json.dumps(
    {
        "claudeAiOauth": {
            "accessToken": "sk-ant-oat01-keychain-codes",
            "refreshToken": "sk-ant-ort01-never-read",
            "expiresAt": 4102444800000,
            "subscriptionType": "max",
        }
    },
    separators=(",", ":"),
)
"""The item's text, the JSON Claude Code writes, as ``security -w`` prints it."""

ERR_SEC_INTERACTION_NOT_ALLOWED = 36  # -25308, low byte
ERR_SEC_USER_CANCELED = 128  # -128, low byte
ERR_SEC_AUTH_FAILED = 51  # -25293, low byte


class Security:
    """The stand-in ``security``: its script, its table of answers by service, its log."""

    def __init__(self, root: Path) -> None:
        root.mkdir(parents=True)
        self.root = root
        (root / "find-generic-password").write_text(STAND_IN, encoding="utf-8")
        self.answers = root / "answers.json"
        self.answers.write_text("{}", encoding="utf-8")
        self.log = root / "calls.jsonl"

    def answer(
        self, account: ClaudeAccount, *, exit: int = 0, stdout: str = "", sleep: float = 0
    ) -> None:
        """What a read of ``account``'s item answers. Its stderr is a bare OSStatus line,
        with no word ``_keychain_problem`` could read a kind from."""
        table = json.loads(self.answers.read_text(encoding="utf-8"))
        table[core.keychain_service(account)] = {
            "exit": exit,
            "stdout": stdout + ("\n" if stdout else ""),
            "stderr": f"security: SecKeychainSearchCopyNext: exit {exit}\n" if exit else "",
            "sleep": sleep,
        }
        self.answers.write_text(json.dumps(table), encoding="utf-8")

    def calls(self) -> list[list[str]]:
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines()]

    def asked(self, account: ClaudeAccount) -> int:
        service = core.keychain_service(account)
        return sum(1 for args in self.calls() if args[args.index("-s") + 1] == service)


@pytest.fixture
def security(
    fake_home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Iterator[Security]:
    """macOS, as far as the Keychain goes, read through the stand-in."""
    stand_in = Security(tmp_path / "security")
    monkeypatch.chdir(stand_in.root)
    monkeypatch.setattr(core, "keychain_platform", lambda: True)
    monkeypatch.setattr(core, "KEYCHAIN_TOOL", sys.executable)
    # A cold interpreter on a loaded CI runner: only the hung read is meant to time out.
    monkeypatch.setattr(core, "KEYCHAIN_TIMEOUT_SECONDS", 30.0)
    monkeypatch.setenv("USER", "tester")
    core.forget_keychain_reads()
    yield stand_in
    core.forget_keychain_reads()


def _slot(email: str) -> ClaudeAccount:
    """A managed Claude Code slot signed in as ``email``: on macOS, ``.claude.json`` alone."""
    account = core.create_account()
    _sign_in(account, email, credentials=False)
    return account


@pytest.mark.parametrize(
    ("status", "expected"),
    [
        (ERR_SEC_INTERACTION_NOT_ALLOWED, "locked"),
        (ERR_SEC_USER_CANCELED, "locked"),
        (ERR_SEC_AUTH_FAILED, "denied"),
    ],
    ids=["errSecInteractionNotAllowed", "errSecUserCanceled", "errSecAuthFailed"],
)
def test_each_osstatus_security_exits_with_is_its_code(
    security: Security, status: int, expected: str
) -> None:
    two = _slot("two@example.com")
    security.answer(two, exit=status)
    assert core.keychain_code(two) == expected
    assert security.asked(two) == 1  # the stand-in answered, nothing else did


def test_a_read_that_answered_with_the_item_is_ok(security: Security) -> None:
    two = _slot("two@example.com")
    security.answer(two, stdout=ITEM)
    assert core.keychain_code(two) == "ok"
    creds = core.stored_login(two).credentials
    assert creds is not None and creds.access_token == "sk-ant-oat01-keychain-codes"
    assert security.asked(two) == 1  # read once: the code came from the memo


def test_no_item_is_ok_a_sign_in_to_do_not_a_keychain_to_fix(security: Security) -> None:
    two = _slot("two@example.com")  # the stand-in answers 44, errSecItemNotFound
    assert core.keychain_code(two) == "ok"
    assert core.stored_login(two).credentials is None
    assert security.asked(two) == 1


def test_a_hung_security_is_timeout(security: Security, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(core, "KEYCHAIN_TIMEOUT_SECONDS", 1.0)
    two = _slot("two@example.com")
    security.answer(two, stdout=ITEM, sleep=60)
    assert core.keychain_code(two) == "timeout"


@pytest.mark.parametrize("how", ["another-exit", "no-security"])
def test_a_read_that_failed_otherwise_is_failed_never_ok(
    security: Security, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, how: str
) -> None:
    two = _slot("two@example.com")
    if how == "another-exit":
        security.answer(two, exit=25)
    else:
        monkeypatch.setattr(core, "KEYCHAIN_TOOL", str(tmp_path / "no-such-security"))
    assert core.keychain_code(two) == "failed"


def test_off_macos_and_for_codex_no_keychain_is_read_and_the_code_is_none(
    security: Security, monkeypatch: pytest.MonkeyPatch
) -> None:
    codex = core.create_account("codex")
    assert core.keychain_code(codex) == "none"  # its login is auth.json, even on macOS
    monkeypatch.setattr(core, "keychain_platform", lambda: False)
    two = _slot("two@example.com")
    security.answer(two, exit=ERR_SEC_AUTH_FAILED)
    assert core.keychain_code(two) == "none"
    assert core.keychain_code(core.default_account()) == "none"
    assert security.calls() == []


def test_accounts_list_json_gives_every_slot_its_keychain(
    security: Security, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``rlx --json accounts list``: ``keychain`` on every slot, Codex's too, each slot's
    own, and the listing asks each item once (the code reads what the row read)."""
    _installed(monkeypatch)
    plain = core.default_account()
    _sign_in(plain, "one@example.com", credentials=False)
    security.answer(plain, stdout=ITEM)
    locked, denied, failed = _slot("two@"), _slot("three@"), _slot("four@")
    security.answer(locked, exit=ERR_SEC_INTERACTION_NOT_ALLOWED)
    security.answer(denied, exit=ERR_SEC_AUTH_FAILED)
    security.answer(failed, exit=25)
    codex = core.create_account("codex")

    listed = runner.invoke(app, ["--json", "accounts", "list"])

    assert listed.exit_code == 0, listed.output
    rows = json.loads(listed.stdout)["accounts"]
    codes = {(row["account"]["agent"], row["account"]["slot"]): row["keychain"] for row in rows}
    assert codes == {
        ("claude-code", plain.slot): "ok",
        ("claude-code", locked.slot): "locked",
        ("claude-code", denied.slot): "denied",
        ("claude-code", failed.slot): "failed",
        ("codex", codex.slot): "none",
    }
    for account in (plain, locked, denied, failed):
        assert security.asked(account) == 1, account.slot

    monkeypatch.setattr(core, "keychain_platform", lambda: False)  # Linux, Windows
    off_mac = runner.invoke(app, ["--json", "accounts", "list"])
    assert off_mac.exit_code == 0, off_mac.output
    assert [row["keychain"] for row in json.loads(off_mac.stdout)["accounts"]] == ["none"] * 5
