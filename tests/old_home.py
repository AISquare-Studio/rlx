"""A home from before 0.10.2 (``~/.aisquare``), in its own layout, with a file of every kind: the
fixture #30's acceptance 2 runs every command against. The one place outside ``core/paths.py``
that names the old layout's files (``tests/test_home_layout.py`` guards the rest).
"""

from __future__ import annotations

import json
from pathlib import Path

#: Every kind of file an old home holds, by its place in the old layout.
OLD_FILES: dict[str, str] = {
    "config.toml": "",
    "state.json": json.dumps({"theme": "rlx"}),
    "agents.json": json.dumps({}),
    "credentials": "",
    "claude-accounts/2/.aisquare-account.json": json.dumps({"slot": 2}),
    "codex-accounts/3/.aisquare-account.json": json.dumps({"slot": 3}),
    "cache/claude-accounts/2/.keep": "",
    "launch-seals/.keep": "",
    "fleet-tmux.conf": "set -g mouse on\n",
    "instances/.keep": "",
    "remote.json": json.dumps({"version": 2}),
    "remote-audit.log": "",
    "remote-dist/index.html": "<!doctype html>",
    "remote-needs.json": json.dumps({}),
    "remote-push.json": json.dumps({}),
    "ngrok-authtoken": "",
    "log/capture.log": "",
    "projects/prj_old/manager/state.md": "# state\n",
    "briefs/prj_old/brf_1/evidence.txt": "",
    "watch/.keep": "",
    "run/.keep": "",
}


def make_old_home(user_home: Path) -> Path:
    """``user_home/.aisquare`` laid out as before 0.10.2, every kind of file in it; its root.

    ``context.db`` is not written here: the store makes it, in the old layout, the first time
    a command opens it in this home (its other files say how the home is laid out)."""
    root = user_home / ".aisquare"
    for name, body in OLD_FILES.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")
    return root


#: Where the old layout keeps each thing ``core.paths`` names, relative to its home.
OLD_PLACES: dict[str, tuple[str, ...]] = {
    "db_path": ("context.db",),
    "log_dir": ("log",),
    "credentials_path": ("credentials",),
    "launch_seals_dir": ("launch-seals",),
    "tmux_conf_path": ("fleet-tmux.conf",),
    "instances_dir": ("instances",),
    "remote_state_path": ("remote.json",),
    "remote_dist_dir": ("remote-dist",),
    "usage_sweep_lock_path": ("usage-sweep.lock",),
    "ngrok_token_path": ("ngrok-authtoken",),
}

#: The same things in the new layout (docs/home-layout.md).
NEW_PLACES: dict[str, tuple[str, ...]] = {
    "db_path": ("rlx.db",),
    "log_dir": ("logs",),
    "credentials_path": ("accounts", "aisquare", "credentials"),
    "launch_seals_dir": ("fleet", "launch-seals"),
    "tmux_conf_path": ("fleet", "tmux.conf"),
    "instances_dir": ("run", "instances"),
    "remote_state_path": ("remote", "remote.json"),
    "remote_dist_dir": ("remote", "dist"),
    "usage_sweep_lock_path": ("run", "locks", "usage-sweep.lock"),
    "ngrok_token_path": ("remote", "ngrok-authtoken"),
}
