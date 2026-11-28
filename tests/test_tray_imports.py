"""The tray costs the CLI, the hooks and the service nothing (#34, acceptance 2).

Every command and every hook imports ``cli/app.py``, and the service imports its parts. None
of them may load the tray: its module, its backends, the glyph's drawing, its settings page,
or the ``tray`` extra's libraries (pyobjc's AppKit, Foundation and objc; jeepney). Only
``rlx service tray``, the service's own child process, loads them.

Measured with ``python -X importtime`` in a fresh interpreter, which names every module a
statement imports, rather than with this one's ``sys.modules``, which holds all the suite has
loaded; module identity, not milliseconds, as ``test_import_cost_of_the_integration.py``
explains.
"""

from __future__ import annotations

import subprocess
import sys

import pytest

TRAY_MODULES = (
    "aisquare.services.tray",
    "aisquare.services.tray_process",
    "aisquare.services.tray_macos",
    "aisquare.services.tray_linux",
    "aisquare.core.glyph",
    "aisquare.cli.ui.views.settings_tray",
    "AppKit",
    "Foundation",
    "objc",
    "jeepney",
)


def _imported_by(code: str) -> set[str]:
    """Every module ``code`` imports in a fresh interpreter, by ``-X importtime``'s report."""
    result = subprocess.run(
        [sys.executable, "-X", "importtime", "-c", code],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr[-2000:]
    names = set()
    for line in result.stderr.splitlines():
        if line.startswith("import time:") and "|" in line:
            name = line.rsplit("|", 1)[1].strip()
            if name and name != "imported package":
                names.add(name)
    return names


@pytest.mark.parametrize(
    ("what", "code", "premise"),
    [
        ("the CLI", "import aisquare.cli.app", "aisquare.cli.service"),
        (
            "a hook",
            "import aisquare.cli.app, aisquare.cli.hook",
            "aisquare.cli.hook",
        ),
        (
            "the service",
            "from aisquare.services import service_host; service_host.default_parts()",
            "aisquare.services.service_tray",
        ),
    ],
    ids=["cli", "hook", "service"],
)
def test_nothing_of_the_tray_is_imported(what: str, code: str, premise: str) -> None:
    loaded = _imported_by(code)
    assert premise in loaded, f"the premise: {what} imports {premise}"
    leaked = [name for name in TRAY_MODULES if name in loaded]
    assert leaked == [], f"{what} now imports {leaked}"


def test_the_measurement_sees_the_tray_where_it_is_imported() -> None:
    """The control: the tray's Linux half does load the glyph and jeepney, so the test above
    is reading what it says it reads."""
    loaded = _imported_by("import aisquare.services.tray_linux")
    assert {"aisquare.services.tray", "aisquare.core.glyph", "jeepney"} <= loaded
