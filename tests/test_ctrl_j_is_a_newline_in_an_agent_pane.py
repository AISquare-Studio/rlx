"""Ctrl+J in an agent pane is a newline, never a submit: the check behind the doctor's advice
(rlx#558) that a terminal with unproven Shift+Enter use ctrl+j for a newline.

asq forwards a key pressed in an agent's pane to tmux (``core.keys.translate``): ctrl+j goes
as tmux's ``C-j``, which the program in the pane reads as one LF byte, and Enter as ``Enter``,
a CR, the byte that submits. Here a Textual pilot presses them in a real ``TerminalPane`` on a
sandbox tmux server (the private ``TMUX_TMPDIR`` every test gets, ``tests/conftest.py``), and
a stand-in agent in the pane records every byte it receives. No real agent runs: a real Claude
Code pane is the hub's check.
"""

from __future__ import annotations

import contextlib
import os
import shutil
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest

from aisquare.core.tmux import BUNDLED_CONF, TmuxError, TmuxServer
from tests.test_terminal_pane import Host, run, screen_text, wait_until

#: The stand-in agent: its tty raw, so the line discipline neither turns a CR into a LF nor
#: holds a line back, then ``ready`` on its screen, then every byte it reads appended,
#: unbuffered, to the file its argument names.
STAND_IN = (
    "import os, sys, tty\n"
    "tty.setraw(0)\n"
    "sink = open(sys.argv[1], 'ab', buffering=0)\n"
    "os.write(1, b'ready')\n"
    "while chunk := os.read(0, 64):\n"
    "    sink.write(chunk)\n"
)


@pytest.fixture
def real_server(tmp_path: Path) -> Iterator[TmuxServer]:
    conf = tmp_path / "tmux.conf"
    conf.write_text(BUNDLED_CONF, encoding="utf-8")
    server = TmuxServer(f"asq-test-{os.getpid()}-ctrl-j", conf=conf)
    try:
        yield server
    finally:
        with contextlib.suppress(TmuxError):
            server.run("kill-server")


@pytest.mark.skipif(sys.platform == "win32" or shutil.which("tmux") is None, reason="needs tmux")
def test_ctrl_j_reaches_the_agent_as_one_lf_and_enter_as_the_cr_that_submits(
    real_server: TmuxServer, tmp_path: Path
) -> None:
    """``a``, ctrl+j, ``b`` arrive as ``a``, LF, ``b``: a newline inside the message, and no CR
    with it, so nothing is submitted. Enter after them arrives as the CR, told apart."""
    received = tmp_path / "received"
    received.touch()
    stand_in = tmp_path / "stand_in.py"
    stand_in.write_text(STAND_IN, encoding="utf-8")
    window = real_server.spawn_window(
        "term",
        name="agent",
        cwd=tmp_path,
        command=[sys.executable, "-I", str(stand_in), str(received)],
        width=80,
        height=24,
    )

    async def drive() -> tuple[bytes, bytes]:
        host = Host(real_server, window.pane_id)
        async with host.run_test(size=(80, 24)) as pilot:
            pane = host.pane
            pane.focus()
            await wait_until(pilot, lambda: screen_text(pane)[:1] == ["ready"], timeout=5.0)
            await pilot.press("a", "ctrl+j", "b")
            await wait_until(pilot, lambda: received.read_bytes().endswith(b"b"))
            typed = received.read_bytes()
            await pilot.press("enter", "c")
            await wait_until(pilot, lambda: received.read_bytes().endswith(b"c"))
            return typed, received.read_bytes()

    typed, everything = run(drive())
    assert typed == b"a\nb", typed
    assert b"\r" not in typed, "ctrl+j sent a CR: the agent would submit"
    assert everything == b"a\nb\rc", everything
