"""What the App settings tests do where they pressed *Save* or *Reload* (M5: there is
neither): leave every text field, which keeps each one changed (a switch or a pick is kept at
its own change); leave the page and open it again, which re-reads the file."""

from __future__ import annotations

from typing import Any

from textual.dom import DOMNode
from textual.pilot import Pilot
from textual.widget import Widget

from aisquare.cli.ui.save_model import CommitInput
from tests.ui_workers import settle_page


def leave_fields(node: DOMNode) -> None:
    """Every text field under ``node`` left, as a person leaves it: each one changed is kept."""
    for field in node.query(CommitInput):
        field.commit()


async def reopen(pilot: Pilot[Any], page: Widget) -> None:
    """``page`` left and opened again, as a person switches away and back to it."""
    page.display = False
    await settle_page(pilot.app)
    page.display = True
    await settle_page(pilot.app)
