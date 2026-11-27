"""One save model for asq's settings pages (UX audit M5): what is changed is kept, and can be
taken back.

Before it, the pages differed: App settings' first two tabs kept nothing until *Save*, Display
and the Remote switches kept each change at once, and a project's nickname waited for Enter or
its *Save*. Now every settings control keeps its change as it is made:

- a switch, a box or a pick, at once;
- a text field (:class:`CommitInput`), on Enter or on leaving it — once, and only when its value
  is not what was last kept;
- a value the page refuses is said in a line under its field (:meth:`SavesAsYouGo.refuse`), and
  nothing is written;
- each kept change says so, ``Saved <what> · Undo: ctrl+z``, and :data:`UNDO_KEY` on the page
  takes the page's last change back: the old value on disk again, and on screen.

``u`` is the sidebar's layout undo, so the key here is ``ctrl+z``: no settings control takes it
(Textual's ``Input``, ``Select``, ``Switch`` and ``Checkbox`` bind nothing to it), and settings
pages hold no terminal pane, where every key is the agent's. It is bound on each page, not the
app (the pages are the settings; the app's keys are the shell's): a page's ``BINDINGS`` lists
:data:`UNDO_BINDING`. :class:`SavesAsYouGo` is a plain mixin, not a widget: as a ``Widget`` base
ahead of the page's own container its stylesheet took the page's (a ``VerticalScroll``'s
``1fr`` height became two rows), and only a widget's own class may bind keys.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from rich.text import Text
from textual.binding import Binding
from textual.containers import Horizontal
from textual.message import Message
from textual.widget import Widget
from textual.widgets import Input, Static

UNDO_KEY = "ctrl+z"
UNDO_BINDING = Binding(UNDO_KEY, "undo_change", "undo", show=False)
"""What a settings page lists in its ``BINDINGS``: :data:`UNDO_KEY` takes its last change back."""
UNDO_SAID = "ctrl+z"
"""How the toast names :data:`UNDO_KEY`."""
SAVED_SECONDS = 5.0
"""How long ``Saved … · Undo`` shows: long enough to reach for the key."""
PROBLEM_CLASS = "field-problem"
"""The line under a field that says why its value was refused."""


class Stale:
    """What an undo returns when what its change left is not there any more: changed since,
    by another way in (another dialog, a shell). It acts on nothing; the page drops it, says
    so, and the next ctrl+z reaches the older ones (the reviewer of #24, #1813)."""

    def __repr__(self) -> str:
        return "STALE"


STALE = Stale()
""":class:`Stale`'s one value."""


@dataclass(frozen=True)
class Change:
    """One kept change: what it was, said as the toast says it, and how to take it back — put
    the old value on disk again, and on screen."""

    what: str
    undo: Callable[[], object]
    """Puts the old value back; returns ``False`` when it could not (having said why), and
    :data:`STALE` when what the change left has changed since."""
    key: object = None
    """What it changed, for :meth:`SavesAsYouGo.forget` (a persona's layer and name): ``None``
    when nothing else can make it stale."""


class CommitInput(Input):
    """A text field that keeps its value on Enter or on leaving it, once per change.

    :attr:`kept` is the value on disk; a commit with the same value posts nothing, so Enter and
    then leaving the field keep it once. Nor does one with the value already posted and not
    answered yet (:attr:`in_flight`): a page that saves off the UI thread hears its save after
    the person may have left the field, and that second commit saved the value again, with a
    second, no-op Undo (#53). The page answers a :class:`Committed` by saving the value (and
    calling :meth:`keep`), by refusing it (:meth:`SavesAsYouGo.refuse`), which leaves
    :attr:`kept` as it was, or, when the save failed and the typed value stays to try again,
    by :meth:`release`; each lets the next commit of that value go again."""

    class Committed(Message):
        """The field's value, on Enter or on leaving it, and the one it replaces on disk."""

        def __init__(self, field: CommitInput, value: str, previous: str) -> None:
            self.field = field
            self.value = value
            self.previous = previous
            super().__init__()

        @property
        def control(self) -> CommitInput:
            return self.field

    def __init__(self, value: str = "", **kwargs: object) -> None:
        super().__init__(value, **kwargs)  # type: ignore[arg-type]
        self.in_flight: str | None = None
        """The value a :class:`Committed` went to the page with, not answered yet: committed
        again, it posts nothing. Set by :meth:`commit`, cleared by every answer (:attr:`kept`
        set, :meth:`keep`, :meth:`SavesAsYouGo.refuse`, :meth:`release`)."""
        self.kept = value
        self.leaving: Callable[[CommitInput], object] | None = None
        """The owner's writer for a value typed and not left when the page goes (rlx quitting,
        its screen closed): called on the field's own last message, synchronously, to write
        the value showing nothing (there is nothing left to show it on); a value it would
        refuse it does not write. The owner sets it as it mounts, never where the field is
        composed: a name as common as ``compose`` is one tests/test_config_writes_stay_in_the_cli
        sees the hooks reach, and a writer handed on there reads as a hook's config write."""

    @property
    def kept(self) -> str:
        """The value on disk: what Undo, or a refusal, comes back to. Set, it answers the value
        in flight too (:attr:`in_flight`): a page's re-read says what is on disk now."""
        return self._kept

    @kept.setter
    def kept(self, value: str) -> None:
        self._kept = value
        self.in_flight = None

    def release(self) -> None:
        """The page's save of the value in flight ended without keeping it, and the typed value
        stays to try again (the store or the file said no): its next commit goes again."""
        self.in_flight = None

    def keep(self, value: str) -> None:
        """``value`` is on disk now (saved, or put back by Undo): kept, and not committed again;
        shown when the field still shows what was kept or what the page is answering
        (:attr:`in_flight`). Text typed since is the person's, and stays: an earlier save's
        report replaced it (#56), as a frame does not (``SettingsView.show_project``)."""
        ours = self.value in (self.kept, self.in_flight)
        self.kept = value
        if ours and self.value != value:
            with self.prevent(Input.Changed):
                self.value = value

    def commit(self) -> None:
        """Enter, or leaving the field: a changed value goes to the page to keep or refuse. One
        put back to what is on disk needs neither, and takes the refusal line under it away:
        it said why a value no longer shown was not saved (tester of #24, F1).

        Left because its page is going (rlx quitting, its screen closed), the field hands its
        value to its owner's :attr:`leaving` at once, which writes it showing nothing: a
        :class:`Committed` would bubble into ancestors whose message loops have ended, and the
        typed value went with it (reviewer of #24)."""
        if self._pruning:
            if self.value != self.kept and self.leaving is not None:
                self.leaving(self)
            return
        if self.value != self.kept:
            if self.value != self.in_flight:
                self.in_flight = self.value
                self.post_message(self.Committed(self, self.value, self.kept))
            return
        page = page_of(self)
        if page is not None:
            page.accept(self)

    def on_input_submitted(self, event: Input.Submitted) -> None:
        if event.input is self:
            self.commit()

    def on_input_blurred(self, event: Input.Blurred) -> None:
        if event.input is self:
            self.commit()


def page_of(widget: Widget) -> SavesAsYouGo | None:
    """The settings page ``widget`` is on (the nearest :class:`SavesAsYouGo` above it), whose
    :meth:`SavesAsYouGo.kept` a part of the page reports its own change to."""
    return next((node for node in widget.ancestors if isinstance(node, SavesAsYouGo)), None)


class SavesAsYouGo:
    """What a settings page (a widget) mixes in: its kept changes, the action :data:`UNDO_KEY`
    runs (the page lists :data:`UNDO_BINDING`), and the line under a field that refuses a value.

    The page saves a control's change in its handler, then calls :meth:`kept` with how to undo
    it; a refused value goes to :meth:`refuse` instead, and nothing is written."""

    @property
    def changes(self) -> list[Change]:
        """The page's kept changes, the last one last: what :data:`UNDO_KEY` takes back."""
        kept: list[Change] | None = self.__dict__.get("_save_model_changes")
        if kept is None:
            kept = self.__dict__["_save_model_changes"] = []
        return kept

    @property
    def _app(self) -> Any:
        assert isinstance(self, Widget), "SavesAsYouGo is mixed into a widget"
        return self.app

    def kept(
        self, what: str, undo: Callable[[], object], *, said: str | None = None, key: object = None
    ) -> None:
        """A change is on disk: say so (``Saved <what>``, or the page's own ``said``), with the
        key that takes it back, and remember how (by ``key``, what it changed, when a later
        action can make the undo wrong: :meth:`forget`)."""
        self.changes.append(Change(what, undo, key))
        self._app.notify(
            f"{said or f'Saved {what}'} · Undo: {UNDO_SAID}", timeout=SAVED_SECONDS, markup=False
        )

    def forget(self, key: object) -> None:
        """Every kept change to ``key`` dropped: an action that cannot be undone replaced what
        they would put back (an import over a persona of that name, a persona removed), and
        undoing them would act on what is there now: remove it, or write an older text over
        it (the tester of #24, #1778). ``None`` (a change no later action can make stale)
        forgets nothing."""
        if key is None:
            return
        self.changes[:] = [change for change in self.changes if change.key != key]

    def action_undo_change(self) -> None:
        """:data:`UNDO_KEY`: the page's last change taken back, on disk and on screen. An undo
        that says it failed (returns ``False``; it has said why) keeps the change, to try again,
        and nothing says "Undone" (reviewer of #24)."""
        if not self.changes:
            self._app.notify("Nothing to undo here", timeout=3, markup=False)
            return
        change = self.changes[-1]
        outcome = change.undo()
        if outcome is STALE:
            self.changes.remove(change)
            self._app.notify(
                f"Not undone: {change.what} has changed since, elsewhere",
                severity="warning",
                timeout=SAVED_SECONDS,
                markup=False,
            )
            return
        if outcome is False:
            self._app.notify(
                f"Not undone: {change.what} · {UNDO_SAID} tries again",
                severity="warning",
                timeout=SAVED_SECONDS,
                markup=False,
            )
            return
        self.changes.remove(change)
        self._app.notify(f"Undone: {change.what}", timeout=3, markup=False)

    def refuse(self, field: Widget, why: str) -> None:
        """``field``'s value is refused: said in a line under it (under its row, when it sits
        in one beside its label), and nothing is written. The field keeps what the person
        typed, to correct, and to commit again (:meth:`CommitInput.release`)."""
        if isinstance(field, CommitInput):
            field.release()
        problem = self._problem_of(field)
        if problem is None:
            problem = Static("", classes=PROBLEM_CLASS, id=f"{field.id}-problem")
            problem.styles.height = "auto"
            home, after = _under_its_row(field)
            if home is not None:
                home.mount(problem, after=after)
        problem.update(Text(why, style="bold red"))
        problem.display = True

    def accept(self, field: Widget) -> None:
        """``field``'s value is good: no refusal line under it."""
        problem = self._problem_of(field)
        if problem is not None:
            problem.display = False

    def problem(self, field: Widget) -> str:
        """What the line under ``field`` says (empty when it says nothing): what a test reads."""
        problem = self._problem_of(field)
        if problem is None or not problem.display:
            return ""
        return str(getattr(problem.visual, "plain", ""))

    def _problem_of(self, field: Widget) -> Static | None:
        home, _after = _under_its_row(field)
        if home is None or field.id is None:
            return None
        found = home.query_children(f"#{field.id}-problem")
        return found.first(Static) if found else None


def _under_its_row(field: Widget) -> tuple[Widget | None, Widget]:
    """Where a field's refusal line goes, and what it follows: under the field's row when the
    field sits in one beside its label (a ``Horizontal``), else under the field. Inside the
    row it took the row's width from the label, drawn a letter a line at 80x24, and was cut
    to two words itself (tester of #24, m1)."""
    parent = field.parent
    if isinstance(parent, Horizontal) and isinstance(parent.parent, Widget):
        return parent.parent, parent
    return (parent if isinstance(parent, Widget) else None), field
