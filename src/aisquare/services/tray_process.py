"""``python -m aisquare.services.tray_process``: the tray's own process, which the rlx service
starts and stops (``service_tray.TrayPart``, #34).

A module of its own so that ``services.tray`` is imported once, as itself: run with ``-m``, a
module runs as ``__main__``, and the OS's half importing ``services.tray`` would load a second
copy of it beside the first.
"""

from __future__ import annotations

import sys

from aisquare.services.tray import main

if __name__ == "__main__":
    sys.exit(main())
