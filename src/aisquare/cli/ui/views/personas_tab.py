"""The Personas page — search, catalogue, preview, and every action.

docs/plans/spawn-personas.md §4.2 (this page) and §4.3 to §4.5 (the dialogs it opens,
in ``cli/ui/persona_dialogs.py``). asq shows it on Settings ▸ Personas with no project
(R27, 0.10.1): the user and bundled layers only, the same for every project, and every
dialog it opens writes the user layer. Built with a project (``PersonasTab(project)``, the
Project view's tab until R27, which nothing in asq builds now), its project supplies the
project layer (``core.project_root``), the one ``fleet spawn`` checks and ``aisquare
persona`` reads from the project's directory. The CLI and the services still READ that
layer, where an agent starts; since 0.10.2 nothing writes it (the owner's decision on #21),
and every dialog writes yours.

**The tab holds no state the disk does not.** Every action ends by re-reading
``core.personas.catalogue``, and the selection is kept by row key (``layer:name``),
falling back to the name, so an imported or copied persona is the row selected
next. Reads are ``core.personas`` (pure: directories in, models out); every write
goes through ``services.personas`` — the seam the CLI uses — looked up on the
module at call time, so a test replaces each with a recorder.

**Attach to existing / Attach to new** — with a project only, as the target picker lists
one project's agents and a spawn goes to one project: the app-level page has neither — post
:class:`AttachRequested`, which the tab answers itself with the target picker
(``cli/ui/attach.py``): an agent is attached to once its row was chosen twice, the first
asking in place (``fleet_service.attach_persona`` in a thread
worker; the toast says whether the briefing was typed, and when it was not, why and how
the persona reaches the agent), a bind or an account opens the Spawn dialog preset with the
persona, and *+ New account* hands over to the Accounts page. The message is not
stopped, so the shell can see it.

Bundled rows disable Edit and Remove ("bundled — open it and Save as… to make it yours"); the
row itself still opens — ``Enter`` on any row opens the SKILL.md, and a bundled
one opens read-only with *Save as…* (§4.4). Keys, live while focus is inside the
tab (the search box keeps its letters): ``a`` attach to existing · ``n`` attach to
new (with a project) · ``e`` edit · ``x`` export · ``Del`` remove · ``v`` validate ·
``i`` import.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import ClassVar, Literal

from rich.text import Text
from textual import on
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.message import Message
from textual.widgets import Button, Collapsible, DataTable, Input, Static
from textual.worker import Worker, WorkerState

from aisquare.cli.ui import persona_dialogs as dialogs
from aisquare.cli.ui.attach import (
    AttachTargetScreen,
    NewAccountRequested,
    ReturnTo,
    Target,
)
from aisquare.cli.ui.save_model import UNDO_BINDING, SavesAsYouGo
from aisquare.cli.ui.spawn import SpawnCompleted, SpawnDialog
from aisquare.core import personas as core
from aisquare.core.agent_adapters import get_adapter
from aisquare.core.personas import Layer, Persona, PersonaError
from aisquare.models import ProjectInfo
from aisquare.services import claude_accounts as accounts_service
from aisquare.services import fleet as fleet_service
from aisquare.services import personas as personas_service

AttachIntent = Literal["existing", "new"]

LAYERS: tuple[Layer, ...] = ("project", "user", "bundled")
"""The layers in precedence order: how rows of one name are sorted."""

ATTACH_ACTIONS = frozenset({"attach_existing", "attach_new"})
"""The actions that need a project: the picker's agents are one project's, a spawn goes to one."""

ATTACH_WORKER = "persona-attach"
"""The worker that attaches a persona to a running agent."""

BUNDLED_REASON = "bundled — open it and Save as… to make it yours"


class AttachRequested(Message):
    """Attach ``persona`` to a running agent (``existing``) or to a new spawn (``new``)."""

    def __init__(self, persona: str, intent: AttachIntent) -> None:
        self.persona = persona
        self.intent = intent
        super().__init__()


@dataclass(frozen=True)
class Entry:
    """One row: a loadable persona, or a directory in a layer that did not load."""

    name: str
    layer: Layer
    path: Path
    persona: Persona | None = None
    reason: str | None = None
    """Why the directory did not load; ``None`` for a persona."""
    shadows: tuple[Layer, ...] = ()

    @property
    def key(self) -> str:
        return f"{self.layer}:{self.name}"


def read_entries(root: Path | None) -> list[Entry]:
    """The catalogue as rows: every winner with what it shadows, then every broken directory."""
    personas, invalid = core.catalogue(root)
    rows = [
        Entry(
            name=persona.name,
            layer=persona.layer,
            path=persona.path,
            persona=persona,
            shadows=tuple(personas_service.shadows(persona, root)),
        )
        for persona in personas
    ]
    bases = core.layer_dirs(root)
    for path, reason in invalid:
        layer = next((layer for layer, base in bases if path.parent == base), None)
        if layer is not None:
            rows.append(Entry(name=path.name, layer=layer, path=path, reason=reason))
    return sorted(rows, key=lambda row: (row.name, LAYERS.index(row.layer)))


def matches(entry: Entry, query: str) -> bool:
    """The search: a substring of the name, description or a tag."""
    needle = query.strip().lower()
    if not needle:
        return True
    haystack = [entry.name]
    if entry.persona is not None:
        haystack += [entry.persona.description, *entry.persona.tags]
    return any(needle in item.lower() for item in haystack)


def marks(entry: Entry) -> Text:
    if entry.persona is None:
        return Text("✗ invalid", style="red")
    if entry.shadows:
        return Text(f"⇧ shadows {', '.join(entry.shadows)}", style="cyan")
    return Text("")


def details_text(entry: Entry) -> Text:
    """Everything under the preview: where it is, what travels with it, where it came from."""
    text = Text()
    text.append("path      ", style="dim")
    text.append(f"{entry.path}\n")
    text.append("layer     ", style="dim")
    text.append(entry.layer)
    if entry.shadows:
        text.append(f"  (shadows {', '.join(entry.shadows)})", style="cyan")
    text.append("\n")
    persona = entry.persona
    if persona is None:
        return text
    text.append("files     ", style="dim")
    text.append(", ".join(persona.files) if persona.files else "(SKILL.md only)")
    text.append("\n")
    if persona.roles:
        text.append("roles     ", style="dim")
        text.append(", ".join(persona.roles) + "\n")
    provenance = persona.provenance
    text.append("source    ", style="dim")
    if provenance is None:
        text.append("(no .persona.json — bundled or written by hand)\n")
    else:
        model = f" · {provenance.model}" if provenance.model else ""
        condensed = " · condensed" if provenance.condensed else ""
        text.append(
            f"{provenance.source} · {provenance.engine}{model}{condensed} · "
            f"{provenance.imported_at:%Y-%m-%d %H:%M}\n"
        )
    for warning in core.warnings(persona):
        text.append(f"⚠ {warning}\n", style="yellow")
    return text


class PersonasTab(SavesAsYouGo, Vertical):
    """The personas: pick one, preview exactly what is injected, act on it. With no
    ``project`` (Settings ▸ Personas), yours and the bundled ones, and no attach."""

    DEFAULT_CSS = """
    PersonasTab { height: 1fr; padding: 0 1; }
    PersonasTab #personas-title { height: 1; text-style: bold; margin-bottom: 1; }
    PersonasTab #personas-migrated { height: auto; margin-bottom: 1; }
    PersonasTab #personas-migrated-scroll { height: auto; max-height: 6; }
    PersonasTab #personas-migrated-detail { height: auto; color: $text-muted; }
    PersonasTab #personas-top { height: auto; }
    PersonasTab #persona-search { width: 1fr; }
    PersonasTab #personas-top Button { margin-left: 1; }
    PersonasTab #personas-body { height: 1fr; }
    PersonasTab #persona-table { width: 3fr; height: 1fr; }
    PersonasTab #persona-preview { width: 2fr; height: 1fr; border-left: tall $panel;
                                   padding: 0 1; }
    PersonasTab #persona-details { margin-top: 1; height: auto; }
    PersonasTab #persona-actions { height: auto; }
    PersonasTab #persona-actions Button { margin-right: 1; }
    PersonasTab #persona-note { height: auto; color: $text-muted; }
    """
    BINDINGS: ClassVar = [
        Binding("a", "attach_existing", "attach to existing"),
        Binding("n", "attach_new", "attach to new"),
        Binding("e", "edit", "edit"),
        Binding("x", "export", "export"),
        Binding("delete", "remove", "remove"),
        Binding("v", "validate", "validate"),
        Binding("i", "import_persona", "import"),
        UNDO_BINDING,
    ]

    def __init__(self, project: ProjectInfo | None = None, *, id: str | None = None) -> None:
        super().__init__(id=id)
        self.project = project
        self.root: Path | None = core.project_root(project) if project is not None else None
        """The project layer's root: the project's own, as spawn checks it (review of #201,
        R1-F6). ``None`` with no project, which is how asq builds this page (Settings ▸
        Personas): your personas and the bundled ones, never a project's layer (0.10.2, the
        owner's decision on #21). Nothing writes a project's layer any more
        (``services.personas._project_refusal``); its personas were copied to yours once."""
        self.entries: list[Entry] = []
        self.selected_key: str | None = None
        self.unavailable: str | None = None
        self.migrated: str | None = None
        """What the one-time copy of the project personas to yours did, when it ran here and
        had something to say (``services.personas.migrate_once``)."""

    def compose(self) -> ComposeResult:
        yield Static(Text("Personas"), id="personas-title")
        # Folded, its title the counts alone: a report of several personas, each with its
        # whole path, took the page at 80x24 (the tester of #21, round 3).
        with (
            Collapsible(title="", collapsed=True, id="personas-migrated"),
            VerticalScroll(id="personas-migrated-scroll"),
        ):
            yield Static(id="personas-migrated-detail")
        # Two rows: one would need ~86 columns, and beside the sidebar an 80-column
        # terminal gives this tab 49, which left + Import… and + New off its edge.
        with Horizontal(id="personas-top"):
            yield Input(placeholder="search name, description or tag", id="persona-search")
            yield Button("+ Import…", id="persona-import")
            yield Button("+ New", id="persona-new")
        with Horizontal(id="personas-body"):
            yield DataTable(id="persona-table", cursor_type="row", zebra_stripes=True)
            with VerticalScroll(id="persona-preview"):
                yield Static(id="persona-briefing")
                yield Static(id="persona-details")
        with Horizontal(id="persona-actions"):
            if self.project is not None:
                yield Button("Attach to existing", id="persona-attach-existing", variant="primary")
                yield Button("Attach to new", id="persona-attach-new", variant="primary")
            yield Button("Edit", id="persona-edit")
            yield Button("Export…", id="persona-export")
            yield Button("Remove", id="persona-remove", variant="error")
            yield Button("Validate", id="persona-validate")
        yield Static(id="persona-note")

    def on_mount(self) -> None:
        self.query_one(DataTable).add_columns("persona", "layer", "description", "roles", "marks")
        report = personas_service.migrate_once()
        self.migrated = report.summary()
        fold = self.query_one("#personas-migrated", Collapsible)
        fold.title = report.headline() or ""
        fold.display = self.migrated is not None
        detail = self.query_one("#personas-migrated-detail", Static)
        detail.update(Text(self.migrated) if self.migrated else "")
        self.reload()

    # --- reading and painting ---------------------------------------------------------

    def reload(self, *, select: str | None = None) -> None:
        """Re-read the catalogue and repaint; ``select`` is a row key or a persona name."""
        try:
            self.entries = read_entries(self.root)
            self.unavailable = None
        except Exception as exc:  # a layer we cannot read costs the list, never the view
            self.entries = []
            self.unavailable = f"personas unavailable — {type(exc).__name__}: {exc}"
        if select is not None:
            self.selected_key = select
        self._paint()

    def visible_entries(self) -> list[Entry]:
        query = self.query_one("#persona-search", Input).value
        return [entry for entry in self.entries if matches(entry, query)]

    def _paint(self) -> None:
        table = self.query_one(DataTable)
        rows = self.visible_entries()
        table.clear()
        for entry in rows:
            description = (
                entry.persona.description if entry.persona is not None else entry.reason or ""
            )
            style = "" if entry.persona is not None else "dim"
            table.add_row(
                Text(entry.name, style=style),
                Text(entry.layer, style=style),
                Text(description, style=style),
                Text(", ".join(entry.persona.roles) if entry.persona else "", style=style),
                marks(entry),
                key=entry.key,
            )
        index = self._index_for(rows, self.selected_key)
        self.selected_key = rows[index].key if rows else None
        if rows:
            table.move_cursor(row=index)
        self._show_selected()

    @staticmethod
    def _index_for(rows: list[Entry], wanted: str | None) -> int:
        """The row a key names — else the first row whose name it is (a layer changed) — else 0."""
        if wanted is None:
            return 0
        for index, entry in enumerate(rows):
            if entry.key == wanted:
                return index
        name = wanted.split(":", 1)[-1]
        winners = [i for i, entry in enumerate(rows) if entry.name == name]
        valid = [i for i in winners if rows[i].persona is not None]
        return (valid or winners or [0])[0]

    def selected(self) -> Entry | None:
        return next((entry for entry in self.entries if entry.key == self.selected_key), None)

    @on(DataTable.RowHighlighted, "#persona-table")
    def _highlighted(self, event: DataTable.RowHighlighted) -> None:
        if event.row_key.value is not None:
            self.selected_key = event.row_key.value
            self._show_selected()

    @on(Input.Changed, "#persona-search")
    def _filter_changed(self) -> None:
        self._paint()

    def _show_selected(self) -> None:
        entry = self.selected()
        briefing = self.query_one("#persona-briefing", Static)
        details = self.query_one("#persona-details", Static)
        if entry is None:
            briefing.update(Text("no persona matches" if self.entries else "", style="dim"))
            details.update("")
        elif entry.persona is None:
            briefing.update(Text(f"✗ {entry.path}: {entry.reason}", style="red"))
            details.update(details_text(entry))
        else:
            # Byte-equal to what a session is briefed with (§4.2): the lines, joined.
            briefing.update(Text("\n".join(core.briefing(entry.persona))))
            details.update(details_text(entry))
        for action, button in (
            ("attach", "#persona-attach-existing"),
            ("attach", "#persona-attach-new"),
            ("edit", "#persona-edit"),
            ("export", "#persona-export"),
            ("remove", "#persona-remove"),
            ("validate", "#persona-validate"),
        ):
            for shown in self.query(button).results(Button):  # no Attach with no project
                shown.disabled = not self._allowed(action, entry)
        if self.unavailable:
            note: str | None = self.unavailable
        elif entry is not None and entry.layer == "bundled":
            note = BUNDLED_REASON
        elif entry is not None and entry.persona is None:
            note = "✗ invalid — Edit fixes it, Remove deletes it; it cannot be attached or exported"
        else:
            note = None
        self.query_one("#persona-note", Static).update(Text(note) if note else "")

    @staticmethod
    def _allowed(action: str, entry: Entry | None) -> bool:
        if entry is None:
            return False
        if action in ("attach", "export"):
            return entry.persona is not None
        if action in ("edit", "remove"):
            return entry.layer != "bundled"
        return True  # validate: a broken directory is exactly what it explains

    # --- attach (the picker's seam) ---------------------------------------------------

    def check_action(self, action: str, parameters: tuple[object, ...]) -> bool | None:
        """``a`` and ``n`` neither run nor show in the footer with no project to attach in."""
        return self.project is not None or action not in ATTACH_ACTIONS

    def _request_attach(self, intent: AttachIntent) -> None:
        entry = self.selected()
        if self.project is not None and entry is not None and self._allowed("attach", entry):
            self.post_message(AttachRequested(entry.name, intent))

    @on(Button.Pressed, "#persona-attach-existing")
    def action_attach_existing(self) -> None:
        self._request_attach("existing")

    @on(Button.Pressed, "#persona-attach-new")
    def action_attach_new(self) -> None:
        self._request_attach("new")

    def on_attach_requested(self, event: AttachRequested) -> None:
        """The second step — who runs it: the target picker, ordered by the intent."""
        self.open_picker(event.persona, event.intent)

    def open_picker(
        self,
        persona: str,
        intent: AttachIntent,
        *,
        select: str | None = None,
        filter_text: str = "",
    ) -> None:
        """The target picker for ``persona``. The shell opens it again, through here, once a
        sign-in its *+ New account* started has landed, with that account highlighted
        (``select``) and the filter as it was (#215): what it is chosen for stays this tab's.
        Nothing with no project: the picker lists one project's agents."""
        project = self.project
        if project is None:
            return
        self.app.push_screen(
            AttachTargetScreen(
                project,
                persona=persona,
                intent=intent,
                accounts=accounts_service.overview,
                select=select,
                filter_text=filter_text,
                attaching=True,  # an agent's row asks in place before it answers
            ),
            callback=lambda target: self._target_chosen(persona, intent, target),
        )

    def _target_chosen(self, persona: str, intent: AttachIntent, target: Target | None) -> None:
        project = self.project
        if target is None or project is None:
            return
        if target.kind == "agent":  # chosen twice in the picker, which asked in place
            self._attach_persona(persona, target.key)
        elif target.kind == "new-account":
            form = {"persona": persona, "intent": intent, "filter": target.key}
            self.post_message(NewAccountRequested(ReturnTo("picker", project.id, form)))
        else:
            self.app.push_screen(
                SpawnDialog(
                    project,
                    persona=persona,
                    role=target.role,
                    binary=target.binary,
                    account=target.account,
                    accounts=accounts_service.overview,
                ),
                callback=self._spawned,
            )

    def _spawned(self, receipt: fleet_service.SpawnReceipt | None) -> None:
        if receipt is not None:
            self.post_message(SpawnCompleted(receipt))  # the shell's own receipt path

    def _attach_persona(self, persona: str, label: str) -> None:
        project = self.project
        if project is None:
            return
        self.run_worker(
            lambda: fleet_service.attach_persona(project, label, persona),
            name=ATTACH_WORKER,
            group=ATTACH_WORKER,
            thread=True,
            exit_on_error=False,  # a refusal is an answer to show, not a crash
        )

    def on_worker_state_changed(self, event: Worker.StateChanged) -> None:
        if event.worker.name != ATTACH_WORKER:
            return
        if event.state is WorkerState.SUCCESS:
            receipt = event.worker.result
            if isinstance(receipt, fleet_service.AttachReceipt):
                label = receipt.agent.label
                said = f"✓ attached {receipt.persona} to {label} ({receipt.delivered})"
                typed = receipt.delivered == "typed"
                if not typed:
                    # Noted, unsent or maybe: the briefing may not have reached the agent, so
                    # say why, and how the persona gets to it, as the CLI's second line does
                    # (review of #240, finding 7; for all three, #201's R1-F9).
                    said = f"{said} — {receipt.how}"
                self.notify(said, timeout=6 if typed else 10, markup=False)
            self.reload()
        elif event.state is WorkerState.ERROR:
            error = event.worker.error
            known = isinstance(error, (fleet_service.FleetError, PersonaError))
            reason = str(error) if known else f"{type(error).__name__}: {error}"
            self.notify(f"✗ could not attach: {reason}", severity="error", timeout=10, markup=False)

    # --- the dialogs ------------------------------------------------------------------

    @on(Button.Pressed, "#persona-import")
    def action_import_persona(self) -> None:
        self.app.push_screen(dialogs.ImportPersonaScreen(self.root), callback=self._imported)

    def _imported(self, result: personas_service.ImportResult | None) -> None:
        if result is None:
            self.reload()
            return
        persona = result.persona
        self.kept(
            f"the import of {persona.name}",
            lambda: self._persona_take_back(persona.name, persona.layer),
            said=f"✓ imported {persona.name} ({result.engine})",
        )
        for warning in result.warnings:
            self.notify(warning, severity="warning", timeout=8, markup=False)
        self.reload(select=f"{persona.layer}:{persona.name}")

    # --- ctrl+z (M5): an import, a new persona or an edit taken back -------------------------

    def _persona_take_back(self, name: str, layer: Layer) -> bool:
        """Undo of an import or a *New*: the persona it made, removed again; whether it was."""
        try:
            personas_service.remove(name, layer=layer, root=self.root)
        except (PersonaError, OSError) as exc:
            self.notify(dialogs.failure(exc), severity="error", timeout=8, markup=False)
            return False
        finally:
            self.reload()
        return True

    @staticmethod
    def _skill_text(entry: Entry) -> str | None:
        """``entry``'s SKILL.md as it is now, for ctrl+z to put back; ``None`` when unreadable
        (an edit of it is then not undoable) or bundled (never edited in place)."""
        if entry.layer == "bundled":
            return None
        try:
            return (entry.path / core.SKILL_FILE).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            return None

    def _kept_edit(self, entry: Entry, before: str | None) -> None:
        """An editor closed having saved ``entry``: kept, with ctrl+z writing ``before`` back
        through the one writer (``services.personas.save``), when the text did change."""
        if before is None or self._skill_text(entry) in (None, before):
            return
        self.kept(f"{entry.name}'s SKILL.md", lambda: self._persona_put_text(entry, before))

    def _persona_put_text(self, entry: Entry, text: str) -> bool:
        """Undo of an edit: ``text`` written back; whether it was."""
        try:
            personas_service.save(entry.name, text, root=self.root, layer=entry.layer)
        except (PersonaError, OSError) as exc:
            self.notify(dialogs.failure(exc), severity="error", timeout=8, markup=False)
            return False
        finally:
            self.reload(select=entry.key)
        return True

    @on(Button.Pressed, "#persona-new")
    def _new(self) -> None:
        self.app.push_screen(dialogs.NewPersonaScreen(self.root), callback=self._persona_created)

    def _persona_created(self, created: dialogs.NewPersona | None) -> None:
        if created is None:
            return
        self.kept(
            f"the new persona {created.name}",
            lambda: self._persona_take_back(created.name, created.layer),
        )
        self.reload(select=f"{created.layer}:{created.name}")
        made = Entry(created.name, created.layer, created.path)
        before = self._skill_text(made)
        self.app.push_screen(
            dialogs.EditPersonaScreen(
                created.name, created.layer, created.path, self.root, text=created.text
            ),
            callback=lambda saved: self._edited_text(made, before, bool(saved)),
        )

    @on(Button.Pressed, "#persona-edit")
    def action_edit(self) -> None:
        entry = self.selected()
        if entry is not None and self._allowed("edit", entry):
            self._open(entry)

    @on(DataTable.RowSelected, "#persona-table")
    def _row_selected(self) -> None:
        entry = self.selected()
        if entry is not None:
            self._open(entry)

    def _open(self, entry: Entry) -> None:
        """The SKILL.md in ``$EDITOR``, in a pane (#212). The text box editor for a bundled
        persona (read-only, with *Save as…*) and wherever no pane can open. What it held
        before is kept for ctrl+z (M5)."""
        before = self._skill_text(entry)
        if entry.layer != "bundled" and dialogs.editor_unavailable() is None:
            self.app.push_screen(
                dialogs.EditorPaneScreen(entry.name, entry.layer, entry.path, self.root),
                callback=lambda result: self._edited(entry, result, before),
            )
            return
        self._open_text(entry, before)

    def _open_text(self, entry: Entry, before: str | None) -> None:
        self.app.push_screen(
            dialogs.EditPersonaScreen(entry.name, entry.layer, entry.path, self.root),
            callback=lambda saved: self._edited_text(entry, before, bool(saved)),
        )

    def _edited_text(self, entry: Entry, before: str | None, saved: bool) -> None:
        self.reload(select=entry.name if saved else entry.key)
        if saved:
            self._kept_edit(entry, before)

    def _edited(self, entry: Entry, result: bool | str | None, before: str | None) -> None:
        """The editor pane closed: re-read the catalogue. A pane that could not open hands
        over to the text box editor, saying why."""
        if isinstance(result, str):
            self.notify(
                f"{result} — editing in the text box instead", severity="warning", markup=False
            )
            self._open_text(entry, before)
            return
        self.reload(select=entry.name if result else entry.key)
        if result:
            self._kept_edit(entry, before)

    @on(Button.Pressed, "#persona-export")
    def action_export(self) -> None:
        entry = self.selected()
        if entry is None or not self._allowed("export", entry):
            return
        self.app.push_screen(
            dialogs.ExportPersonaScreen(entry.name, self.root), callback=self._exported
        )

    def _exported(self, done: dialogs.ExportDone | None) -> None:
        if done is None:
            return
        where = f"✓ exported {done.name} to {done.path}"
        if done.skill is not None and done.agent is not None:
            adapter = get_adapter(done.agent)
            command = adapter.capabilities.skill_command
            where += (
                f" — it is {command.format(name=done.name)} in {adapter.label} now"
                if command is not None
                else f" — a skill {adapter.label} reads now"
            )
        self.notify(where, timeout=8, markup=False)
        self.reload()

    @on(Button.Pressed, "#persona-remove")
    def action_remove(self) -> None:
        entry = self.selected()
        if entry is None or not self._allowed("remove", entry):
            return

        def confirmed(yes: bool | None) -> None:
            if not yes:
                return
            try:
                removed = personas_service.remove(entry.name, layer=entry.layer, root=self.root)
            except (PersonaError, OSError) as exc:
                self.notify(dialogs.failure(exc), severity="error", timeout=8, markup=False)
            else:
                self.notify(f"✓ removed {removed}", timeout=6, markup=False)
            self.reload()

        self.app.push_screen(
            dialogs.ConfirmRemoveScreen(entry.name, entry.layer, entry.path), callback=confirmed
        )

    @on(Button.Pressed, "#persona-validate")
    def action_validate(self) -> None:
        entry = self.selected()
        if entry is None:
            return
        try:
            persona, warnings = personas_service.validate(entry.path)
        except PersonaError as exc:
            self.notify(f"✗ {exc}", severity="error", timeout=10, markup=False)
        else:
            self.notify(f"✓ {persona.name} is a valid persona", timeout=5, markup=False)
            for warning in warnings:
                self.notify(warning, severity="warning", timeout=8, markup=False)
        self.reload()
