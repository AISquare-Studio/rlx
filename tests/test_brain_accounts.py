"""The brain's ``accounts`` (task #14): the Claude Code accounts rlx knows, read-only, for
rlxd's connector map. Each one's slot, a managed slot's two homes, and a mark of who is
signed in there, never the account's id or email."""

from __future__ import annotations

import hashlib
import json
import os
import tempfile
from pathlib import Path

from aisquare.core import claude_accounts as core
from aisquare.models import ClaudeAccount
from aisquare.services import brain


def _sign_in(account: ClaudeAccount, email: str, uuid: str | None) -> None:
    """Write the ``.claude.json`` a signed-in Claude Code leaves for ``account``."""
    target = core.claude_json_path(account)
    target.parent.mkdir(parents=True, exist_ok=True)
    oauth: dict[str, str] = {"emailAddress": email, "organizationName": "Fixture Org"}
    if uuid is not None:
        oauth["accountUuid"] = uuid
    target.write_text(json.dumps({"oauthAccount": oauth}), encoding="utf-8")


def _sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _files(*roots: Path) -> dict[str, tuple[int, int]]:
    """Every file under ``roots``, with its size and modification time."""
    seen: dict[str, tuple[int, int]] = {}
    for root in roots:
        for folder, _dirs, names in os.walk(root):
            for name in names:
                path = Path(folder) / name
                stat = path.stat()
                seen[str(path)] = (stat.st_size, stat.st_mtime_ns)
    return seen


def test_each_claude_code_slot_is_listed_with_its_homes_and_a_mark_of_its_login() -> None:
    second = core.create_account()
    _sign_in(second, "someone@example.com", "uuid-of-slot-2")
    _sign_in(core.default_account(), "plain@example.com", None)
    result = brain.answer("accounts", {}, brain.METHODS)
    assert result["ok"] is True, result
    rows = result["result"]["accounts"]
    assert [row["slot"] for row in rows] == [1, second.slot]
    plain, managed = rows
    assert plain == {
        "slot": 1,
        "label": None,
        "config_dir": None,
        "tmp_dir": None,
        "login": _sha256("plain@example.com"),
    }
    assert managed["config_dir"] == str(second.config_dir)
    assert managed["tmp_dir"] == str(second.tmp_dir)
    assert managed["login"] == _sha256("uuid-of-slot-2")
    # The id and the emails stay in the brain.
    text = json.dumps(result)
    for secret in ("someone@example.com", "plain@example.com", "uuid-of-slot-2"):
        assert secret not in text


def test_a_slot_no_one_is_signed_in_to_has_no_mark() -> None:
    second = core.create_account()
    rows = brain.answer("accounts", {}, brain.METHODS)["result"]["accounts"]
    assert [row["login"] for row in rows if row["slot"] == second.slot] == [None]


def test_another_agents_slot_is_not_listed() -> None:
    codex = core.create_account("codex")
    rows = brain.answer("accounts", {}, brain.METHODS)["result"]["accounts"]
    assert codex.slot not in [row["slot"] for row in rows]


def test_the_list_writes_nothing() -> None:
    second = core.create_account()
    _sign_in(second, "someone@example.com", "uuid-of-slot-2")
    # The test's own homes: rlx's, and the one the accounts read as ``~`` (conftest's
    # isolated_agent_home points it at a temp folder).
    roots = [Path(os.environ["AISQUARE_HOME"]), core._home()]
    assert all(
        str(root).startswith(tempfile.gettempdir()) or "pytest" in str(root) for root in roots
    )
    before = _files(*roots)
    brain.answer("accounts", {}, brain.METHODS)
    assert _files(*roots) == before
