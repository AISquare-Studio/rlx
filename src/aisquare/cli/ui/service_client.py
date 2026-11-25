"""A TUI as the rlx service's client (S1, #32), and what it does when the service dies.

While a service of this release runs on the home (``services.service_host``), it hosts Remote,
the usage reads and the retry of held-back manager nudges. The TUI then leaves them to it:
Remote's controller is the service's client (``RemoteController.service``: the switch is
saved and the service follows it; nothing starts or stops here), and the frame retries no
nudge (``cli.ui.app._read_frame``). Its own usage read goes on through the cache, which the
service keeps fresh: no fetch.

If the service dies while a TUI is open, the TUI takes them back on its next housekeeping
tick and says so once (the manager's ruling on #32, Q4): Remote, if its switch is on, starts
here again; the nudges are the frame's again from the frame that finds no service. Without
it, Remote and the reads would stop with nobody told.
"""

from __future__ import annotations

from collections.abc import Callable

from aisquare.cli.ui.remote_control import RemoteController
from aisquare.services.service_host import SERVICE, Hosting

TAKEN_BACK = (
    f"the {SERVICE} stopped: this rlx keeps Remote and the fleet's nudges going while it is open"
)
"""What the TUI says, once, when it takes back what the service hosted."""


class ServiceClient:
    """What a TUI knows of the rlx service, from each frame's registry read, and its take-back."""

    def __init__(self, remote: RemoteController, say: Callable[[str], object]) -> None:
        self._remote = remote
        self._say = say
        self.hosting: Hosting | None = None
        """The live service of this release, as the last frame found it."""
        self._client = False
        """Whether this TUI was the service's client: what a take-back takes over from."""

    def note_hosting(self, hosting: Hosting | None) -> None:
        """A frame's read: the service live (its Remote as it says), or none."""
        self.hosting = hosting
        self._remote.service = None if hosting is None else hosting.remote
        if hosting is not None:
            self._client = True

    def take_back(self) -> bool:
        """On the housekeeping tick: if the service this TUI was the client of is gone, start
        Remote here again (when its switch is on) and say so, once; whether it did."""
        if not self._client or self.hosting is not None:
            return False
        self._client = False
        self._remote.restore(wait=False)
        self._say(TAKEN_BACK)
        return True
