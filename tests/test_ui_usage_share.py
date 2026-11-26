"""Usage sharing in the TUI: the switch on the Usage & stats page, the one-time question, and
what is shown of what would be sent.

The widgets reach the service through ``Sharing``, here a fake that keeps its own state, so
nothing is written and nothing sent; the shell's question and its shipper tick are driven through
the whole fleet UI. The service itself is ``tests/test_usage_share.py``.
"""

from __future__ import annotations

import asyncio
import json
import os
from collections.abc import Awaitable, Callable, Sequence
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, TypeVar

import pytest
from textual.app import App, ComposeResult
from textual.pilot import Pilot
from textual.widgets import Button, Static

from aisquare.cli.ui import app as app_mod
from aisquare.cli.ui.app import FleetApp
from aisquare.cli.ui.sidebar import SettingsRow
from aisquare.cli.ui.views.usage_sharing import (
    ConsentDialog,
    PreviewDialog,
    Sharing,
    SharingBox,
    status_text,
)
from aisquare.cli.ui.views.usage_stats import UsageStatsView
from aisquare.core import sharing as sharing_core
from aisquare.core import tmux as tmux_core
from aisquare.core.tmux import Completed, TmuxServer
from aisquare.models import (
    FleetAgent,
    FleetAgentStatus,
    ProjectInfo,
    SharingStatus,
    ShipReport,
)
from aisquare.services import first_run, usage_share
from aisquare.services import fleet as fleet_service
from aisquare.services.active_ping import PingStatus
from tests import guard_paint
from tests.fleet_fakes import list_agents_as
from tests.projects import add_project
from tests.test_ui_settings_hub import GOOD
from tests.ui_workers import released, settle_page, settle_until

T = TypeVar("T")
SENT = datetime(2026, 10, 26, 6, 30, tzinfo=UTC)
USAGE_LINE = (
    "Your usage here, the last 30 days: 1.2M tokens in 48 sessions across 6 repositories, "
    "on 14 days. It stays on this machine unless you share it."
)


class FakeSharing:
    """The sharing service, in memory: it keeps the state the buttons change."""

    def __init__(
        self,
        *,
        enabled: bool = False,
        forced_off: str | None = None,
        signed_in: bool = False,
        due: bool = True,
        ship_report: ShipReport | None = None,
        ping: bool = True,
        usage_line: str = USAGE_LINE,
    ) -> None:
        self.ping_now = PingStatus(switch=ping, forced_off=forced_off, last_sent=None)
        self.ping_calls: list[bool] = []
        """Each flip of the daily ping's switch (kept apart from ``calls``: it is not sharing)."""
        self.usage_line = usage_line
        self.status_now = SharingStatus(
            enabled=enabled and forced_off is None,
            switch=enabled,
            forced_off=forced_off,
            signed_in=signed_in,
            last_sent_at=SENT if enabled else None,
            linked=signed_in if enabled else None,
        )
        self.due = due
        self.sample = not enabled
        self.ship_report = ship_report or ShipReport(sent=True, days=3, linked=signed_in)
        self.calls: list[str] = []

    def status(self) -> SharingStatus:
        return self.status_now

    def enable(self) -> SharingStatus:
        self.calls.append("enable")
        self.status_now = self.status_now.model_copy(
            update={
                "enabled": True,
                "switch": True,
                "last_sent_at": SENT,
                "linked": self.status_now.signed_in,
            }
        )
        return self.status_now

    def disable(self) -> ShipReport:
        self.calls.append("disable")
        self.status_now = self.status_now.model_copy(
            update={"enabled": False, "switch": False, "last_sent_at": None}
        )
        return ShipReport(sent=True, opt_out=True)

    def ship(self) -> ShipReport:
        self.calls.append("ship")
        return self.ship_report

    def ship_due(self) -> ShipReport:
        self.calls.append("ship_due")
        return ShipReport(skipped="nothing to send")

    def preview(self) -> str:
        self.calls.append("preview")
        return json.dumps({"schema_version": 1, "days": []}, indent=2)

    def is_sample(self) -> bool:
        return self.sample

    def consent_due(self) -> bool:
        self.calls.append("consent_due")
        return self.due

    def answer(self, yes: bool) -> ShipReport | None:
        self.calls.append("share" if yes else "declined")
        self.due = False
        if yes:
            self.status_now = self.status_now.model_copy(update={"enabled": True, "switch": True})
            return self.ship_report
        return None

    def ping_status(self) -> PingStatus:
        return self.ping_now

    def set_ping(self, on: bool) -> PingStatus:
        self.ping_calls.append(on)
        self.ping_now = PingStatus(switch=on, forced_off=self.ping_now.forced_off, last_sent=None)
        return self.ping_now

    def usage(self) -> str:
        return self.usage_line

    def seam(self) -> Sharing:
        return Sharing(
            status=self.status,
            enable=self.enable,
            disable=self.disable,
            ship=self.ship,
            ship_due=self.ship_due,
            preview=self.preview,
            is_sample=self.is_sample,
            consent_due=self.consent_due,
            answer=self.answer,
            ping_status=self.ping_status,
            set_ping=self.set_ping,
            usage=self.usage,
        )


class Host(App[None]):
    def __init__(self, fake: FakeSharing) -> None:
        super().__init__()
        self.fake = fake

    def compose(self) -> ComposeResult:
        yield UsageStatsView(
            loader=lambda range_, sweep, progress: _empty(range_),
            sharing=self.fake.seam(),
            id="usage",
        )


def _empty(range_: Any) -> Any:
    from aisquare.models import UsageStats

    return UsageStats(range=range_, until="2026-10-26")


def _drive_page(fake: FakeSharing, fn: Callable[[Pilot[None], SharingBox], Awaitable[T]]) -> T:
    async def run() -> T:
        app = Host(fake)
        async with app.run_test(size=(120, 50)) as pilot:
            await settle_page(app)
            return await fn(pilot, app.query_one(SharingBox))

    return asyncio.run(run())


def _text(widget: Static) -> str:
    plain = getattr(widget.visual, "plain", None)
    assert isinstance(plain, str)
    return plain


def _line(box: SharingBox) -> str:
    return _text(box.query_one("#usage-share-status", Static))


def _label(box: SharingBox) -> str:
    return str(box.query_one("#usage-share-toggle", Button).label)


# --- the switch ----------


def test_the_switch_starts_off_and_says_nothing_leaves_the_machine() -> None:
    async def go(pilot: Pilot[None], box: SharingBox) -> tuple[str, str, bool, str]:
        toggle = box.query_one("#usage-share-toggle", Button)
        return (
            _label(box),
            _line(box),
            toggle.disabled,
            _text(box.query_one("#usage-share-about", Static)),
        )

    label, line, disabled, about = _drive_page(FakeSharing(), go)

    assert (label, line, disabled) == (
        "Turn sharing on",
        "Off — no usage leaves this machine.",
        False,
    )
    assert (
        "Never names, paths, remotes, prompts or arguments" in about and "Off by default" in about
    )
    # re-review of #444 (m2): whoever opts in here, not only in the dialog, is told
    assert "every repo you run Claude Code in, including ones you haven't added" in about


def test_one_click_turns_it_on_and_sends_the_first_shipment_and_one_click_turns_it_off() -> None:
    fake = FakeSharing(signed_in=True)

    async def go(pilot: Pilot[None], box: SharingBox) -> list[tuple[str, str]]:
        seen: list[tuple[str, str]] = []
        await pilot.click("#usage-share-toggle")
        await settle_page(box.app)
        seen.append((_label(box), _line(box)))
        await released(pilot, box.query_one("#usage-share-toggle", Button))
        await pilot.click("#usage-share-toggle")
        await settle_page(box.app)
        seen.append((_label(box), _line(box)))
        return seen

    seen = _drive_page(fake, go)

    assert fake.calls == ["enable", "ship", "disable"]  # one click each way; nothing else
    assert seen[0][0] == "Turn sharing off" and seen[0][1].startswith("On — last sent")
    assert "linked to your AISquare account" in seen[0][1]
    assert seen[1] == ("Turn sharing on", "Off — no usage leaves this machine.")


def test_a_click_that_lands_as_the_page_appears_is_not_how_someone_agrees_to_share(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    guard_paint.install(monkeypatch)  # the page's paint is held back: it has just appeared
    fake = FakeSharing()

    async def go(pilot: Pilot[None], box: SharingBox) -> str:
        await pilot.click("#usage-share-toggle")
        await settle_page(box.app)
        return _label(box)

    assert _drive_page(fake, go) == "Turn sharing on" and fake.calls == []


def test_the_keyboard_is_a_deliberate_gesture_and_flips_it() -> None:
    fake = FakeSharing()

    async def go(pilot: Pilot[None], box: SharingBox) -> str:
        box.query_one("#usage-share-toggle", Button).focus()
        await pilot.press("enter")
        await settle_page(box.app)
        return _label(box)

    assert _drive_page(fake, go) == "Turn sharing off" and fake.calls[:1] == ["enable"]


@pytest.mark.parametrize(
    "why", ["DO_NOT_TRACK is set", "AISQUARE_NO_TELEMETRY is set", "CI is set"]
)
def test_a_forced_off_switch_cannot_be_pressed_and_says_why(why: str) -> None:
    fake = FakeSharing(enabled=True, forced_off=why)

    async def go(pilot: Pilot[None], box: SharingBox) -> tuple[bool, str, str]:
        await pilot.click("#usage-share-toggle")
        await settle_page(box.app)
        toggle = box.query_one("#usage-share-toggle", Button)
        return toggle.disabled, _label(box), _line(box)

    disabled, label, line = _drive_page(fake, go)

    assert disabled is True and fake.calls == []
    assert (
        label == "Turn sharing on" and line == f"Off — forced: {why}. Nothing leaves this machine."
    )


# --- the daily active ping ----------


def _ping_line(box: SharingBox) -> str:
    return _text(box.query_one("#active-ping-status", Static))


def _ping_label(box: SharingBox) -> str:
    return str(box.query_one("#active-ping-toggle", Button).label)


def test_the_daily_ping_has_its_own_switch_which_says_it_is_separate_from_sharing() -> None:
    async def go(pilot: Pilot[None], box: SharingBox) -> tuple[str, str, str, str, bool]:
        return (
            _text(box.query_one("#active-ping-title", Static)),
            _text(box.query_one("#active-ping-about", Static)),
            _ping_label(box),
            _ping_line(box),
            box.query_one("#active-ping-toggle", Button).disabled,
        )

    title, about, label, line, disabled = _drive_page(FakeSharing(), go)

    assert title == "Daily active ping (separate from usage sharing)"
    assert "whether usage sharing is on or off" in about
    for sent in ("the date", "rlx's version", "your OS", "CPU type", "how rlx was installed"):
        assert sent in about, sent
    assert "No id, no account, nothing about your work" in about
    assert (label, line, disabled) == (
        "Turn the daily ping off",
        "On — sent once on each day you use rlx.",
        False,
    )


def test_one_click_turns_the_daily_ping_off_and_one_turns_it_on_and_sharing_is_untouched() -> None:
    fake = FakeSharing()

    async def go(pilot: Pilot[None], box: SharingBox) -> list[tuple[str, str]]:
        seen: list[tuple[str, str]] = []
        for _ in range(2):
            toggle = box.query_one("#active-ping-toggle", Button)
            await released(pilot, toggle)
            await pilot.click("#active-ping-toggle")
            await settle_page(box.app)
            seen.append((_ping_label(box), _ping_line(box)))
        return seen

    seen = _drive_page(fake, go)

    assert fake.ping_calls == [False, True]
    assert seen == [
        ("Turn the daily ping on", "Off — no ping is sent."),
        ("Turn the daily ping off", "On — sent once on each day you use rlx."),
    ]
    assert fake.calls == []  # sharing was neither turned on nor off


@pytest.mark.parametrize(
    "why", ["DO_NOT_TRACK is set", "AISQUARE_NO_TELEMETRY is set", "CI is set"]
)
def test_a_forced_off_ping_cannot_be_pressed_and_says_why(why: str) -> None:
    fake = FakeSharing(forced_off=why)

    async def go(pilot: Pilot[None], box: SharingBox) -> tuple[bool, str]:
        await pilot.click("#active-ping-toggle")
        await settle_page(box.app)
        return box.query_one("#active-ping-toggle", Button).disabled, _ping_line(box)

    disabled, line = _drive_page(fake, go)

    assert disabled is True and fake.ping_calls == []
    assert line == f"Off — forced: {why}. No ping is sent."


def test_the_line_says_whether_the_server_linked_the_shipments_to_the_account() -> None:
    base = SharingStatus(enabled=True, switch=True, last_sent_at=SENT)
    anonymous = status_text(base)
    linked = status_text(base.model_copy(update={"signed_in": True, "linked": True}))
    pending = status_text(base.model_copy(update={"signed_in": True, "linked": False}))
    never = status_text(base.model_copy(update={"last_sent_at": None}))
    failed = status_text(base.model_copy(update={"last_error": "HTTP 503", "failures": 2}))

    assert "anonymous (sign in to AISquare to link it" in anonymous
    assert "last sent 2026-10-26" in anonymous or "last sent 2026-10-25" in anonymous
    assert "· linked to your AISquare account" in linked
    assert "the next shipment is linked to your AISquare account" in pending
    assert never.startswith("On — nothing sent yet")
    assert "fail" not in failed.lower() and "503" not in failed and "retry" not in failed.lower()


def test_a_first_shipment_that_fails_is_not_said_and_the_switch_stays_on() -> None:
    """Sharing fails silently, whatever the server's state (the owner's decision): the page
    shows the consent state and the last shipment that WENT, never "failed" or "retrying"."""
    fake = FakeSharing(ship_report=ShipReport(error="HTTP 404 — Could not reach the server"))

    async def go(pilot: Pilot[None], box: SharingBox) -> tuple[str, str, list[str]]:
        await pilot.click("#usage-share-toggle")
        await settle_page(box.app)
        toasts = [n.message for n in box.app._notifications]
        return _label(box), _line(box), toasts

    label, line, toasts = _drive_page(fake, go)

    assert label == "Turn sharing off"
    assert line.startswith("On — ")
    for word in ("fail", "404", "retry", "error", "reach", "wait"):
        assert word not in line.lower(), word
    assert toasts == []


def test_the_state_is_read_again_when_the_page_is_shown_again() -> None:
    fake = FakeSharing()

    async def go(pilot: Pilot[None], box: SharingBox) -> tuple[str, str]:
        before = _label(box)
        fake.status_now = fake.status_now.model_copy(update={"enabled": True, "switch": True})
        box.refresh_status()  # as the page's `on_show` does after the shell's question
        await settle_page(box.app)
        return before, _label(box)

    assert _drive_page(fake, go) == ("Turn sharing on", "Turn sharing off")


# --- what is shared ----------


def test_whats_shared_shows_the_exact_body_and_sends_nothing() -> None:
    fake = FakeSharing()

    async def go(pilot: Pilot[None], box: SharingBox) -> tuple[str | None, bool, int]:
        await pilot.click("#usage-share-preview")
        await settle_page(box.app)
        screen = box.app.screen
        assert isinstance(screen, PreviewDialog)
        shown = _text(screen.query_one("#preview-body", Static))
        body = screen.body
        await pilot.press("escape")
        await settle_page(box.app)
        return body, shown == body, len(box.app.screen_stack)

    body, shown, depth = _drive_page(fake, go)

    assert body is not None and json.loads(body) == {"schema_version": 1, "days": []} and shown
    assert depth == 1  # Escape closed it
    assert fake.calls == ["preview"]  # looking sends nothing and turns nothing on


def test_the_preview_is_captioned_a_sample_while_sharing_is_off_and_exact_when_on() -> None:
    async def about(pilot: Pilot[None], box: SharingBox) -> str:
        await pilot.click("#usage-share-preview")
        await settle_page(box.app)
        screen = box.app.screen
        assert isinstance(screen, PreviewDialog)
        return _text(screen.query_one("#preview-about", Static))

    off = _drive_page(FakeSharing(), about)
    on = _drive_page(FakeSharing(enabled=True), about)

    assert "SAMPLE" in off and "placeholder ids" in off and "only days from then on" in off
    assert "SAMPLE" not in on and on.startswith("Exactly what the next shipment would send")


def test_the_consent_text_says_only_days_from_yes_on_are_sent() -> None:
    from aisquare.cli.ui.views.usage_sharing import consent_text

    assert "Only days from the moment you say yes are sent" in consent_text()
    assert "every repo you run Claude Code in, including ones you haven't added" in consent_text()
    assert "exactly what's sent" not in consent_text().lower()


def test_the_consent_text_names_the_daily_ping_and_that_it_is_separate() -> None:
    from aisquare.cli.ui.views.usage_sharing import consent_text

    text = consent_text()

    assert "Separate from this, whatever you answer: a daily active ping" in text
    assert "no id" in text and "It has its own switch there." in text
    assert "rlx config" not in text and "metrics" not in text  # the app names no command


def test_the_question_shows_your_own_usage_before_it_asks() -> None:
    fake = FakeSharing()

    async def run() -> tuple[str, int, int, int]:
        class Page(App[None]):
            def compose(self) -> ComposeResult:
                yield Static("page")

        app = Page()
        async with app.run_test(size=(100, 40)):
            await app.push_screen(ConsentDialog(fake.seam()))
            await settle_page(app)
            screen = app.screen
            usage = screen.query_one("#consent-usage", Static)
            return (
                _text(usage),
                usage.region.y,
                screen.query_one("#consent-text", Static).region.y,
                screen.query_one("#consent-share", Button).region.y,
            )

    usage, usage_y, question_y, share_y = asyncio.run(run())

    assert usage == USAGE_LINE
    assert usage_y < question_y < share_y  # above the question, which is above its answer


@pytest.mark.parametrize("size", [(80, 24), (130, 50)])
def test_the_question_keeps_its_answers_on_screen_and_its_text_scrolls(
    size: tuple[int, int],
) -> None:
    """At 80x24 the question is taller than the screen: its text scrolls, and the three
    answers stay inside the box (before 0.10.2 they were cut off below it)."""
    fake = FakeSharing()

    async def run() -> tuple[bool, bool]:
        class Page(App[None]):
            def compose(self) -> ComposeResult:
                yield Static("page")

        app = Page()
        async with app.run_test(size=size):
            await app.push_screen(ConsentDialog(fake.seam()))
            await settle_page(app)
            screen = app.screen
            box = screen.query_one("#consent-box").region
            inside = all(
                box.contains_region(screen.query_one(f"#{name}", Button).region)
                for name in ("consent-see", "consent-share", "consent-no")
            )
            scroll = screen.query_one("#consent-scroll")
            return inside, scroll.virtual_size.height > scroll.region.height

    inside, scrolls = asyncio.run(run())

    assert inside
    assert scrolls is (size == (80, 24))


def test_your_usage_is_the_last_30_days_in_one_line(monkeypatch: pytest.MonkeyPatch) -> None:
    from aisquare.cli.ui.views.usage_sharing import your_usage
    from aisquare.models import UsageRepos, UsageStats, UsageTokens
    from aisquare.services import usage_stats

    asked: list[str] = []

    def stats(range_: str) -> UsageStats:
        asked.append(range_)
        return UsageStats(
            range="30d",
            until="2026-11-25",
            turns=900,
            sessions=48,
            active_days=14,
            tokens=UsageTokens(input=200_000, output=1_000_000),
            repos=UsageRepos(active=1),
        )

    monkeypatch.setattr(usage_stats, "stats", stats)
    line = your_usage()
    monkeypatch.setattr(
        usage_stats, "stats", lambda range_: UsageStats(range="30d", until="2026-11-25")
    )
    empty = your_usage()

    assert asked == ["30d"]
    assert line == (
        "Your usage here, the last 30 days: 1.2M tokens in 48 sessions across 1 repository, "
        "on 14 days. It stays on this machine unless you share it."
    )
    assert empty == "Your usage here, the last 30 days: nothing recorded yet."


def test_the_preview_is_built_off_the_ui_thread_and_a_failure_is_said() -> None:
    fake = FakeSharing()

    def broken() -> str:
        raise RuntimeError("the store is locked")

    seam = fake.seam()
    seam.preview = broken

    async def run() -> str:
        class Page(App[None]):
            def compose(self) -> ComposeResult:
                yield Static("page")

        app = Page()
        async with app.run_test(size=(100, 40)):
            await app.push_screen(PreviewDialog(seam))
            await settle_page(app)
            return _text(app.screen.query_one("#preview-body", Static))

    assert asyncio.run(run()) == "could not build it: the store is locked"


# --- the question ----------


def _fleet(fake: FakeSharing) -> FleetApp:
    return FleetApp(
        refresh_seconds=3600,
        doctor=lambda: [],
        accounts=None,
        headroom=lambda accounts: GOOD,
        headroom_seconds=3600,
        sharing=fake.seam(),
    )


async def _open_settings(app: FleetApp, pilot: Pilot[None]) -> None:
    await pilot.click(app.query_one(SettingsRow))  # leaves the welcome page: onboarded, in use
    await settle_page(app)
    await settle_until(
        app, lambda: isinstance(app.screen, ConsentDialog) or len(app.screen_stack) == 1
    )


@pytest.fixture
def a_fleet_is_up(monkeypatch: pytest.MonkeyPatch) -> None:
    """A listed project's fleet is up (``first_run.fleet_is_up``), the moment the question
    waits for (Q6): for the tests of the question itself. Its wait has tests of its own, with
    the listing's own rows (the end of this file)."""
    monkeypatch.setattr(first_run, "fleet_is_up", lambda rows: True)


def test_the_question_is_asked_once_after_onboarding_and_no_thanks_is_remembered(
    tmp_path: Path, sharing_allowed: None, a_fleet_is_up: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    add_project(tmp_path / "work")
    (tmp_path / "work").mkdir(exist_ok=True)
    fake = FakeSharing()

    async def go() -> tuple[list[str], str, int, list[str]]:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            # Start opens home since L1, the project's own view: the first view after
            # onboarding, where the question is asked, once.
            asked_at_start = type(app.screen).__name__
            default_focus = getattr(app.screen.focused, "id", None)
            await pilot.click("#consent-no")
            await settle_page(app)
            after = len(app.screen_stack)
            await _open_settings(app, pilot)  # another page: not asked again
            await pilot.click(app.query_one(SettingsRow))
            await settle_page(app)
            asked_again = [type(app.screen).__name__]
            return (
                [asked_at_start, str(default_focus)],
                str(after),
                len(app.screen_stack),
                asked_again,
            )

    first, after, depth, again = asyncio.run(go())

    assert first[0] == "ConsentDialog", "asked at start, on home: there is a project to work in"
    assert first[1] == "consent-no", "No thanks has the focus: Enter alone does not opt in"
    assert after == "1" and depth == 1 and again[0] != "ConsentDialog"
    assert fake.calls.count("consent_due") == 1 and fake.calls.count("declined") == 1
    assert "share" not in fake.calls and "enable" not in fake.calls


def test_share_anonymous_usage_turns_it_on_and_sends_the_first_shipment(
    tmp_path: Path, sharing_allowed: None, a_fleet_is_up: None
) -> None:
    add_project(tmp_path / "work")
    fake = FakeSharing(signed_in=True)

    async def go() -> list[str]:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            await _open_settings(app, pilot)
            await pilot.click("#consent-share")
            await settle_page(app)
            await settle_until(app, lambda: "share" in fake.calls)
            return [type(app.screen).__name__]

    assert asyncio.run(go()) != ["ConsentDialog"]
    assert fake.calls == ["consent_due", "share"]
    assert fake.status_now.enabled is True


def test_escape_is_no_thanks(
    tmp_path: Path, sharing_allowed: None, a_fleet_is_up: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    add_project(tmp_path / "work")
    fake = FakeSharing()

    async def go() -> int:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            await _open_settings(app, pilot)
            await pilot.press("escape")
            await settle_page(app)
            await settle_until(app, lambda: "declined" in fake.calls)
            return len(app.screen_stack)

    assert asyncio.run(go()) == 1 and fake.calls == ["consent_due", "declined"]


def test_see_exactly_whats_sent_opens_the_preview_over_the_question_and_returns_to_it(
    tmp_path: Path, sharing_allowed: None, a_fleet_is_up: None
) -> None:
    add_project(tmp_path / "work")
    fake = FakeSharing()

    async def go() -> tuple[list[str], list[str]]:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            await _open_settings(app, pilot)
            await pilot.click("#consent-see")
            await settle_page(app)
            over = [type(screen).__name__ for screen in app.screen_stack[1:]]
            await pilot.press("escape")  # closes the preview, not the question
            await settle_page(app)
            back = [type(screen).__name__ for screen in app.screen_stack[1:]]
            return over, back

    over, back = asyncio.run(go())

    assert over == ["ConsentDialog", "PreviewDialog"] and back == ["ConsentDialog"]
    assert fake.calls.count("preview") == 1 and "declined" not in fake.calls


def test_the_question_is_never_asked_where_something_forces_sharing_off(
    tmp_path: Path, a_fleet_is_up: None
) -> None:
    """The suite forces it off (AISQUARE_NO_TELEMETRY), as a CI run or DO_NOT_TRACK does."""
    add_project(tmp_path / "work")
    fake = FakeSharing()

    async def go() -> int:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            await pilot.click(app.query_one(SettingsRow))
            await settle_page(app)
            return len(app.screen_stack)

    assert asyncio.run(go()) == 1 and "consent_due" not in fake.calls


def test_the_question_is_not_asked_when_it_was_answered_before(
    tmp_path: Path, sharing_allowed: None, a_fleet_is_up: None
) -> None:
    add_project(tmp_path / "work")
    fake = FakeSharing(due=False)

    async def go() -> int:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            await pilot.click(app.query_one(SettingsRow))
            await settle_page(app)
            await settle_until(app, lambda: "consent_due" in fake.calls)
            return len(app.screen_stack)

    assert asyncio.run(go()) == 1 and fake.calls == ["consent_due"]


def test_with_no_project_yet_there_is_no_question(
    tmp_path: Path, sharing_allowed: None, a_fleet_is_up: None
) -> None:
    fake = FakeSharing()

    async def go() -> int:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            await pilot.click(app.query_one(SettingsRow))
            await settle_page(app)
            return len(app.screen_stack)

    assert asyncio.run(go()) == 1 and "consent_due" not in fake.calls


# --- the shell's shipper ----------


def test_the_housekeeping_tick_tries_a_shipment_only_where_sharing_is_on(
    sharing_allowed: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = FakeSharing()
    state = {"on": False}
    monkeypatch.setattr(sharing_core, "enabled", lambda: state["on"])

    async def go() -> tuple[list[str], list[str], list[str]]:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)):
            await settle_page(app)
            app._ship_at = 0.0
            app._housekeeping()
            await settle_page(app)
            off = list(fake.calls)
            state["on"] = True
            app._ship_at = 0.0
            app._housekeeping()
            await settle_page(app)
            on = list(fake.calls)
            app._housekeeping()  # the next look is five minutes away
            await settle_page(app)
            return off, on, list(fake.calls)

    off, on, again = asyncio.run(go())

    assert off == [] and on == ["ship_due"] and again == ["ship_due"]


def test_the_shell_does_not_look_at_the_shipper_before_a_minute_has_passed(
    sharing_allowed: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = FakeSharing()
    monkeypatch.setattr(sharing_core, "enabled", lambda: True)

    async def go() -> list[str]:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)):
            await settle_page(app)
            app._housekeeping()
            await settle_page(app)
            return list(fake.calls)

    assert asyncio.run(go()) == []
    assert usage_share.TIMEOUT_S == 5.0


# --- Q6: after "Your fleet is up", never mid-setup ----------


Rows = dict[str, list[FleetAgentStatus]]
Q6_SOCKET = f"asq-test-{os.getpid()}-sharing-ask"


@pytest.fixture
def listed(monkeypatch: pytest.MonkeyPatch) -> Rows:
    """What the listing says of each project's agents (``fleet.list_agents``), by project id;
    and no tmux server but a private one, which never answers."""

    def no_server(argv: Sequence[str], stdin: bytes | None) -> Completed:
        assert "-L" not in argv or Q6_SOCKET in argv, argv
        return Completed(1, "", "no server running (a UI test addresses no real fleet)\n")

    monkeypatch.setattr(tmux_core, "_tmux", no_server)
    monkeypatch.setattr(fleet_service, "server", lambda config=None: TmuxServer(Q6_SOCKET))
    rows: Rows = {}
    list_agents_as(monkeypatch, lambda project, live_only=True: list(rows.get(project.id, [])))
    return rows


def _running(project: ProjectInfo, label: str, role: str) -> FleetAgentStatus:
    agent = FleetAgent(
        id=f"agt_{project.id}_{label}",
        project_id=project.id,
        label=label,
        role=role,
        pane_id="%1",
        tmux_socket=Q6_SOCKET,
        cwd=project.root,
        created_at=datetime(2026, 11, 25, 9, 0, tzinfo=UTC),
    )
    return FleetAgentStatus(agent=agent, state="working")


def _fleet_of(project: ProjectInfo, coders: int) -> list[FleetAgentStatus]:
    manager = _running(project, "manager", "manager")
    return [manager, *(_running(project, f"coder-{n}", "coder") for n in range(1, coders + 1))]


async def _listed_again(app: FleetApp) -> None:
    app.refresh_data()
    await settle_page(app, group=app_mod.FLEET_WORKER)


async def _page_change(app: FleetApp, pilot: Pilot[None]) -> str:
    """Settings, then what is on screen: a page change is where the question is asked."""
    await _open_settings(app, pilot)
    return type(app.screen).__name__


def test_the_question_waits_for_a_fleet_that_is_up_whichever_page_rlx_opens_on(
    tmp_path: Path, sharing_allowed: None, listed: Rows
) -> None:
    """Q6 (the round-2 tester of #570): since #580 rlx opens on the last project's Manager tab
    whenever one is listed, so with a project and no fleet the question came at start,
    mid-setup, and Welcome being shown held nothing. It waits for a listed project's manager
    and coders to be running (``first_run.fleet_is_up``), whatever page is open; the next page
    change then asks, once."""
    project = add_project(tmp_path / "work")
    (tmp_path / "work").mkdir(exist_ok=True)
    fake = FakeSharing()

    async def go() -> dict[str, Any]:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            seen: dict[str, Any] = {"start": (app.content.current, type(app.screen).__name__)}
            seen["settings"] = await _page_change(app, pilot)
            listed[project.id] = _fleet_of(project, first_run.CODERS - 1)  # one coder short
            await _listed_again(app)
            seen["short"] = await _page_change(app, pilot)
            listed[project.id] = _fleet_of(project, first_run.CODERS)
            await _listed_again(app)
            seen["listed"] = type(app.screen).__name__  # no page change yet
            seen["up"] = await _page_change(app, pilot)
            return seen

    seen = asyncio.run(go())
    page, screen = seen["start"]
    assert page == f"project-{project.id}" and screen != "ConsentDialog", seen
    assert "ConsentDialog" not in (seen["settings"], seen["short"], seen["listed"]), seen
    assert seen["up"] == "ConsentDialog", seen
    assert fake.calls.count("consent_due") == 1


def test_the_fleet_is_up_with_a_manager_and_the_coders_running_and_not_before() -> None:
    project = ProjectInfo(id="prj_q6", root=Path("work"))
    up = _fleet_of(project, first_run.CODERS)
    assert first_run.fleet_is_up(up)
    assert not first_run.fleet_is_up(up[1:]), "no manager"
    assert not first_run.fleet_is_up(up[:-1]), "a coder short"
    lost = [*up[:-1], up[-1].model_copy(update={"state": "lost"})]
    assert not first_run.fleet_is_up(lost), "a coder whose window is gone"
    ended = up[-1].agent.model_copy(update={"ended_at": datetime(2026, 11, 25, tzinfo=UTC)})
    assert not first_run.fleet_is_up([*up[:-1], up[-1].model_copy(update={"agent": ended})])


def test_welcome_holds_the_question_while_it_sets_a_fleet_up_or_waits_on_its_trust_answer(
    tmp_path: Path, sharing_allowed: None, listed: Rows
) -> None:
    """Q6 with a fleet up already: Welcome setting another project's up holds the question (its
    manager started, its coders not yet), and so does its wait on the manager's trust answer,
    the coders up or not (started by hand before it). Then the next page change asks."""
    from aisquare.cli.ui.views.welcome import WelcomeView
    from aisquare.services.first_run import FleetStep

    up = add_project(tmp_path / "up")
    new = add_project(tmp_path / "new")
    for root in (up.root, new.root):
        root.mkdir(exist_ok=True)
    fake = FakeSharing()

    async def go() -> dict[str, Any]:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            await app.action_welcome()  # `w`: no fleet up yet, so nothing is asked
            await settle_page(app)
            page = app.query_one(WelcomeView)
            page.project = new
            manager = _running(new, "manager", "manager").agent
            page.steps = {"manager": FleetStep("manager", "manager", "started", agent=manager)}
            listed[up.id] = _fleet_of(up, first_run.CODERS)
            await _listed_again(app)
            seen: dict[str, Any] = {"setting_up": page.setting_up}
            seen["setting up"] = await _page_change(app, pilot)
            for n in range(1, first_run.CODERS + 1):
                coder = _running(new, f"coder-{n}", "coder").agent
                page.steps[coder.label] = FleetStep(coder.label, "coder", "started", agent=coder)
            page.busy.add("trust")  # its wait on the trust answer still runs
            seen["fleet_up"] = page.fleet_up
            seen["trust"] = await _page_change(app, pilot)
            page.busy.discard("trust")
            seen["answered"] = await _page_change(app, pilot)
            return seen

    seen = asyncio.run(go())
    assert seen["setting_up"] and seen["fleet_up"], seen
    assert seen["setting up"] != "ConsentDialog", "not while Welcome sets a fleet up"
    assert seen["trust"] != "ConsentDialog", "not while the manager's trust answer is awaited"
    assert seen["answered"] == "ConsentDialog", seen
    assert fake.calls.count("consent_due") == 1


def test_a_fresh_home_is_asked_once_welcome_has_its_fleet_up_and_never_before(
    tmp_path: Path, sharing_allowed: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Q6 from a fresh home (the reviewer of #570): no project, so rlx opens on Welcome. Step 1
    sets a folder up, *Start fleet* opens the manager's pane in place for its trust question,
    and the coders follow the answer. Nothing is asked on the way, the pages it opens
    included; once the fleet is up, the next page change asks, once."""
    import threading

    from aisquare.cli.ui.views import welcome
    from aisquare.cli.ui.views.welcome import WelcomeView
    from aisquare.models import SetupReport
    from aisquare.services.first_run import Candidates
    from aisquare.services.onboarding import OnboardOutcome
    from tests import test_ui_welcome as screens

    folder = tmp_path / "shop"
    folder.mkdir()
    machine = screens.Machine(
        claude=[screens.READY],
        found=Candidates(items=(screens.here(folder),)),
        trust=threading.Event(),
    )

    def onboarded(path: Path) -> OnboardOutcome:
        project = add_project(path)  # in the store, so the shell lists it
        machine.stored[project.id] = project
        report = SetupReport(home=path / ".home", already_initialized=False, project=project)
        return OnboardOutcome(path=path, project_id=project.id, report=report)

    machine.onboard_answer = onboarded
    monkeypatch.setattr(welcome, "DEFAULT_SEAMS", machine.seams())
    list_agents_as(monkeypatch, lambda project, live_only=True: machine.statuses(project))
    sockets = (Q6_SOCKET, screens.PRIVATE_SOCKET)

    def no_server(argv: Sequence[str], stdin: bytes | None) -> Completed:
        assert "-L" not in argv or any(socket in argv for socket in sockets), argv
        return Completed(1, "", "no server running (a UI test addresses no real fleet)\n")

    monkeypatch.setattr(tmux_core, "_tmux", no_server)
    monkeypatch.setattr(fleet_service, "server", lambda config=None: TmuxServer(Q6_SOCKET))
    fake = FakeSharing()
    group = welcome.WORKER_GROUP  # the wait on the trust answer is held on purpose

    async def go() -> dict[str, Any]:
        app = _fleet(fake)
        async with app.run_test(size=(130, 50)) as pilot:
            await settle_page(app)
            page = app.query_one(WelcomeView)
            seen: dict[str, Any] = {"start": app.content.current, "screens": []}

            def look() -> None:
                seen["screens"].append((app.content.current, type(app.screen).__name__))

            screens.first_candidate(page).press()
            await settle_until(
                app,
                lambda: (
                    app.snapshot is not None
                    and bool(app.snapshot.projects)
                    and screens.visible(page, "fleet-manager")
                ),
                group=group,
            )
            look()
            page.query_one("#fleet-manager", Button).press()
            await settle_until(
                app, lambda: machine.waited and app.content.current != "welcome", group=group
            )
            await settle_page(app, group=group)
            look()  # the manager's pane, its trust question unanswered
            machine.trust.set()  # type: ignore[union-attr]  # answered there
            await settle_until(app, lambda: len(machine.starts) > 1)
            await _listed_again(app)
            look()  # the fleet is up: no page change yet
            seen["asked_before"] = fake.calls.count("consent_due")
            app.sidebar.focus()
            await pilot.press("w")  # Welcome again: a page change
            await settle_until(app, lambda: isinstance(app.screen, ConsentDialog))
            seen["after"] = (app.content.current, type(app.screen).__name__)
            return seen

    seen = asyncio.run(go())
    assert seen["start"] == "welcome", seen
    [(set_up, on_it), (manager, over_it), (_up, then)] = seen["screens"]
    assert set_up == "welcome" and manager.startswith("agent-"), seen
    assert "ConsentDialog" not in (on_it, over_it, then), seen
    assert seen["asked_before"] == 0, "not asked while it was set up"
    assert seen["after"] == ("welcome", "ConsentDialog"), seen
    assert fake.calls.count("consent_due") == 1
