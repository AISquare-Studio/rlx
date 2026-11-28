"""Every place rlx uses the temporary directory, and how each stays shared (#57, #3445).

Each fleet agent, and each launch by hand, runs with a ``TMPDIR`` of its own
(``services.agent_tmp``). Anything that relied on ``tempfile.gettempdir()`` naming ONE place
for every process (a lock, a socket, a file another process reads) would split there: an
agent's ``rlx`` and the ``asq`` beside it would each see their own.

This scan holds every use under ``src/aisquare`` to the list below: per file, how many lines
use the temporary directory, and why each stays shared. None splits (:data:`SPLITS`): H2's
migration lock, the one that did, is beside the homes since #62. A new use fails here,
naming its file, until its author adds it with its reason. Lines that read ``TMUX_TMPDIR``
are tmux's own rule (``$TMUX_TMPDIR`` or ``/tmp``, never ``TMPDIR``), which no agent's
directory changes, and are not counted.
"""

from __future__ import annotations

import re
from pathlib import Path

import aisquare

SRC = Path(aisquare.__file__).parent

USES = re.compile(
    r"gettempdir|mkstemp|mkdtemp|NamedTemporaryFile|TemporaryDirectory|SpooledTemporaryFile"
    r"""|["'](?:TMPDIR|TEMP|TMP|/tmp|/var/tmp)["']"""
)

#: File (under ``src/aisquare``) -> (lines using the temporary directory, why it holds).
SHARED: dict[str, tuple[int, str]] = {
    # A rendezvous between processes: none reads TMPDIR.
    "services/captain/state.py": (
        2,
        "the captain's ui socket for a long home: /tmp/aisquare-<uid> on POSIX, a fixed "
        "path; Windows reads the temp dir, but Python there has no AF_UNIX "
        "(captain/actions.py), so nothing binds or dials it",
    ),
    "services/tidy.py": (
        2,
        "the system temp dir rlx tidy frees stale migration locks in, found without TMPDIR "
        "(macOS's per-user one by confstr, else /tmp): the same for every agent",
    ),
    # One process, or a path handed to the child explicitly.
    "core/editor.py": (1, "the file's path is the editor's argument"),
    "services/native_telemetry.py": (
        1,
        "mkdtemp in the launcher, its path handed to the receiver as --ready: now in the "
        "agent's own directory, and goes with it",
    ),
    "services/persona_import.py": (1, "a scratch dir whose paths go to the headless call"),
    "core/launch_seal.py": (1, "dir= explicit: a probe beside the record"),
    "services/explainability_ops.py": (1, "dir= explicit: doctor's scratch beside its target"),
    "services/personas.py": (3, "dir= explicit beside the destination; two docstring lines"),
    "services/remote_server.py": (2, "dir= explicit beside the destination; a docstring line"),
    # Models of what a sandboxed agent may write: read TMPDIR on purpose, and agree with the
    # agent because its launch sets the launcher's own TMPDIR first (agent_tmp.prepare).
    "core/agent_adapters/codex.py": (2, "Codex's writable roots: /tmp and $TMPDIR"),
    "services/store_broker.py": (3, "the dirs the broker does not hold read-only"),
    "core/sandbox_roots.py": (1, "temp roots are not recorded as repositories"),
    # The per-agent directory itself, and the rules around it.
    "services/agent_tmp.py": (2, "sets TMPDIR (TEMP, TMP), keeping the outer one"),
    "core/spawn.py": (2, "a tmux server started from an agent gets the outer TMPDIR back"),
    "services/usage_stats.py": (3, "throwaway-repository roots, <home>/tmp among them"),
    "core/workspace.py": (1, "a docstring"),
}


#: Uses that would SPLIT once each agent has its own TMPDIR: none. H2's migration lock was the
#: one (#3445), and #62 (#641) moved it beside the homes. A use listed here fails the suite
#: until it is moved to a fixed place.
SPLITS: dict[str, str] = {}


def _uses() -> dict[str, int]:
    found: dict[str, int] = {}
    for path in sorted(SRC.rglob("*.py")):
        lines = path.read_text(encoding="utf-8").splitlines()
        count = sum(1 for line in lines if USES.search(line) and "TMUX_TMPDIR" not in line)
        if count:
            found[path.relative_to(SRC).as_posix()] = count
    return found


def test_every_use_of_the_temp_dir_is_listed_with_how_it_stays_shared() -> None:
    found = _uses()
    listed = {name: count for name, (count, _why) in SHARED.items()}
    new = {name: count for name, count in found.items() if listed.get(name) != count}
    assert not new, (
        "a use of the temporary directory this list does not explain: "
        + ", ".join(
            f"{name} ({count} lines, listed {listed.get(name, 0)})" for name, count in new.items()
        )
        + ". Each agent has its own TMPDIR (#57): say in SHARED how it stays shared, or "
        "move it to a fixed place (beside the home)."
    )
    gone = sorted(set(listed) - set(found))
    assert not gone, f"listed, but no longer there: {gone}"


def test_no_use_of_the_temp_dir_splits_across_agents() -> None:
    assert not SPLITS, f"these would split once each agent has its own TMPDIR: {SPLITS}"
    assert set(SPLITS) <= set(SHARED), "a split use is listed among the uses too"


def test_the_scan_sees_a_new_use() -> None:
    """The scan can fail: a line that asks for the temp dir is counted."""
    assert USES.search("lock = Path(tempfile.gettempdir()) / 'x.lock'")
    assert USES.search('os.environ.get("TMPDIR")')
    assert not USES.search("nothing temporary here")
