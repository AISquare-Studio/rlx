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


@dataclass(frozen=True)
class Change:
    """One kept change: what it was, said as the toast says it, and how to take it back — put
    the old value on disk again, and on screen."""

    what: str
    undo: Callable[[], object]


class CommitInput(Input):
    """A text field that keeps its value on Enter or on leaving it, once per change.

    :attr:`kept` is the value on disk; a commit with the same value posts nothing, so Enter and
    then leaving the field keep it once. The page answers a :class:`Committed` by saving the
    value (and calling :meth:`keep`), or by refusing it (:meth:`SavesAsYouGo.refuse`), which
    leaves :attr:`kept` as it was."""

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
        self.kept = value
        """The value on disk: what Undo, or a refusal, comes back to."""

    def keep(self, value: str) -> None:
        """``value`` is on disk now (saved, or put back by Undo): shown, and not committed
        again."""
        self.kept = value
        if self.value != value:
            with self.prevent(Input.Changed):
                self.value = value

    def commit(self) -> None:
        if self.value != self.kept:
            self.post_message(self.Committed(self, self.value, self.kept))

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

    def kept(self, what: str, undo: Callable[[], object], *, said: str | None = None) -> None:
        """A change is on disk: say so (``Saved <what>``, or the page's own ``said``), with the
        key that takes it back, and remember how."""
        self.changes.append(Change(what, undo))
        self._app.notify(
            f"{said or f'Saved {what}'} · Undo: {UNDO_SAID}", timeout=SAVED_SECONDS, markup=False
        )

    def action_undo_change(self) -> None:
        """:data:`UNDO_KEY`: the page's last change taken back, on disk and on screen."""
        if not self.changes:
            self._app.notify("Nothing to undo here", timeout=3, markup=False)
            return
        change = self.changes.pop()
        change.undo()
        self._app.notify(f"Undone: {change.what}", timeout=3, markup=False)

    def refuse(self, field: Widget, why: str) -> None:
        """``field``'s value is refused: said in a line under it, and nothing is written. The
        field keeps what the person typed, to correct."""
        problem = self._problem_of(field)
        if problem is None:
            problem = Static("", classes=PROBLEM_CLASS, id=f"{field.id}-problem")
            if isinstance(field.parent, Widget):
                field.parent.mount(problem, after=field)
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
        if field.parent is None or field.id is None:
            return None
        found = field.parent.query_children(f"#{field.id}-problem")
        return found.first(Static) if found else None
