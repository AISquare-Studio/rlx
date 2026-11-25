"""The one-time copy of every project's personas to the user layer (0.10.2, handoff C).

Since 0.10.1 the app offers personas at the user level only (Settings ▸ Personas). A
project's own layer, ``<root>/.aisquare/personas``, is copied once into yours, so nothing a
project held is lost from the page. Nothing is deleted. A persona of the same name that is
already yours is kept, and the report says so.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

import pytest
from textual.app import App, ComposeResult
from textual.widgets import Static

from aisquare.cli.ui.views.personas_tab import PersonasTab
from aisquare.core import personas as core
from aisquare.core import state_file
from aisquare.services import personas as persona_service
from tests.projects import add_project
from tests.ui_workers import settle_page


def _user() -> Path:
    return dict(core.layer_dirs(None))["user"]


def _skill(directory: Path, body: str) -> Path:
    skill = directory / core.SKILL_FILE
    skill.parent.mkdir(parents=True, exist_ok=True)
    skill.write_text(
        f"---\nname: {directory.name}\ndescription: For this test.\n---\n{body}\n",
        encoding="utf-8",
    )
    return skill


def _project_persona(root: Path, name: str, body: str) -> Path:
    return _skill(root / ".aisquare" / "personas" / name, body)


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    root = tmp_path / "repo"
    (root / ".git").mkdir(parents=True)
    add_project(root)
    return root


def test_a_project_persona_ends_up_yours_and_the_project_file_stays(repo: Path) -> None:
    source = _project_persona(repo, "tempo", "Work at the repository's tempo.")

    report = persona_service.migrate_project_personas()  # every project added to rlx

    assert [(item.name, item.outcome) for item in report.personas] == [("tempo", "copied")]
    mine = core.load(_user() / "tempo", layer="user")
    assert mine.body == "Work at the repository's tempo."
    assert source.is_file(), "the project's file is left where it was"
    assert source.read_text(encoding="utf-8").endswith("Work at the repository's tempo.\n")
    summary = report.summary()
    assert summary is not None and "tempo" in summary and "left where they were" in summary


def test_a_persona_of_the_same_name_that_is_yours_is_kept_and_the_report_says_so(
    repo: Path,
) -> None:
    source = _project_persona(repo, "skeptic", "THE PROJECT'S skeptic.")
    mine = _skill(_user() / "skeptic", "YOUR skeptic.")

    report = persona_service.migrate_project_personas()

    assert [(item.name, item.outcome) for item in report.personas] == [("skeptic", "kept_yours")]
    assert mine.read_text(encoding="utf-8").endswith("YOUR skeptic.\n"), "yours is kept"
    assert source.is_file()
    summary = report.summary()
    assert summary is not None
    assert "Kept your own skeptic; the project's differs and was not copied" in summary
    assert str(source.parent) in summary, "it says where the project's copy is"


def test_an_identical_one_is_already_there_and_nothing_is_said(repo: Path) -> None:
    _project_persona(repo, "pair", "Pair on the work.")
    _skill(_user() / "pair", "Pair on the work.")

    report = persona_service.migrate_project_personas()

    assert [(item.name, item.outcome) for item in report.personas] == [("pair", "already_there")]
    assert report.summary() is None


def test_two_projects_with_one_name_copy_the_first_and_say_so_of_the_second(
    tmp_path: Path,
) -> None:
    first, second = tmp_path / "first", tmp_path / "second"
    _project_persona(first, "tempo", "FIRST tempo.")
    _project_persona(second, "tempo", "SECOND tempo.")

    report = persona_service.migrate_project_personas([first, second])

    assert [item.outcome for item in report.personas] == ["copied", "name_taken"]
    assert core.load(_user() / "tempo", layer="user").body == "FIRST tempo."
    assert "copied first" in (report.personas[1].detail or "")


def test_what_cannot_be_a_persona_is_skipped_with_its_reason(tmp_path: Path) -> None:
    root = tmp_path / "repo"
    layer = root / ".aisquare" / "personas"
    (layer / "notes").mkdir(parents=True)  # no SKILL.md
    _skill(layer / "Bad_Name", "A name a persona cannot have.")

    report = persona_service.migrate_project_personas([root])

    assert sorted((item.name, item.outcome) for item in report.personas) == [
        ("Bad_Name", "skipped"),
        ("notes", "skipped"),
    ]
    assert not (_user() / "notes").exists() and not (_user() / "Bad_Name").exists()


@pytest.mark.skipif(sys.platform == "win32", reason="symbolic links need privileges on Windows")
def test_a_link_is_never_followed_into_your_layer(tmp_path: Path) -> None:
    root = tmp_path / "repo"
    persona = root / ".aisquare" / "personas" / "linked"
    _skill(persona, "Work.")
    secret = tmp_path / "secret.txt"
    secret.write_text("not for you", encoding="utf-8")
    (persona / "env.txt").symlink_to(secret)
    elsewhere = root / ".aisquare" / "personas" / "fake"
    elsewhere.mkdir()
    (elsewhere / core.SKILL_FILE).symlink_to(persona / core.SKILL_FILE)

    report = persona_service.migrate_project_personas([root])

    by_name = {item.name: item.outcome for item in report.personas}
    assert by_name == {"fake": "skipped", "linked": "copied"}
    assert not (_user() / "linked" / "env.txt").exists()
    assert not (_user() / "fake").exists()


def test_it_runs_once_per_machine(repo: Path) -> None:
    _project_persona(repo, "tempo", "Tempo.")
    first = persona_service.migrate_once()
    _project_persona(repo, "later", "Added to the repository afterwards.")

    second = persona_service.migrate_once()

    assert first.ran and [item.name for item in first.personas] == ["tempo"]
    assert not second.ran and second.summary() is None
    assert not (_user() / "later").exists()
    kept = state_file.read_state()[persona_service.MIGRATION_KEY]
    assert isinstance(kept, dict) and kept["personas"][0]["outcome"] == "copied"


def test_the_personas_page_says_what_was_copied_once(repo: Path) -> None:
    _project_persona(repo, "tempo", "Tempo.")

    class Page(App[None]):
        def compose(self) -> ComposeResult:
            yield PersonasTab(id="personas")

    async def open_page() -> tuple[bool, str, list[str]]:
        app = Page()
        async with app.run_test(size=(120, 40)):
            await settle_page(app)
            line = app.query_one("#personas-migrated", Static)
            plain = getattr(line.visual, "plain", "")
            names = [entry.name for entry in app.query_one(PersonasTab).entries]
            return bool(line.display), str(plain), names

    shown, text, names = asyncio.run(open_page())
    again, _text, _names = asyncio.run(open_page())

    assert shown and "Copied your project personas to your personas: tempo." in text
    assert "tempo" in names, "and the page lists it as yours"
    assert not again, "said once: the second opening has nothing to say"


def _outside(tmp_path: Path) -> Path:
    """A directory outside the repository, standing for ``~/.ssh``: a persona with a key."""
    stolen = tmp_path / "outside" / "stolen"
    _skill(stolen, "Outside the repository.")
    (stolen / "id_rsa").write_text("PRIVATE KEY MATERIAL\n", encoding="utf-8")
    return stolen


def _nothing_stolen() -> None:
    assert not list(_user().rglob("id_rsa")), "a file from outside the repository is yours now"


@pytest.mark.skipif(sys.platform == "win32", reason="symbolic links need privileges on Windows")
def test_a_persona_directory_that_is_a_link_is_skipped_and_nothing_is_copied(
    tmp_path: Path,
) -> None:
    """The review of #561: ``.aisquare/personas/stolen -> <outside>`` copied ``id_rsa``."""
    root = tmp_path / "repo"
    (root / ".aisquare" / "personas").mkdir(parents=True)
    (root / ".aisquare" / "personas" / "stolen").symlink_to(
        _outside(tmp_path), target_is_directory=True
    )

    report = persona_service.migrate_project_personas([root])

    assert [(item.name, item.outcome) for item in report.personas] == [("stolen", "skipped")]
    assert "symbolic link" in (report.personas[0].detail or "")
    assert not (_user() / "stolen").exists()
    _nothing_stolen()


@pytest.mark.skipif(sys.platform == "win32", reason="symbolic links need privileges on Windows")
@pytest.mark.parametrize("linked", [".aisquare", "personas"])
def test_a_projects_layer_reached_through_a_link_is_skipped_whole(
    tmp_path: Path, linked: str
) -> None:
    """A repository that commits ``.aisquare -> …`` or ``.aisquare/personas -> …``: nothing
    under the link is read, and the report says why."""
    root = tmp_path / "repo"
    elsewhere = tmp_path / "elsewhere"
    stolen = _outside(tmp_path)
    if linked == ".aisquare":
        (elsewhere / "personas").mkdir(parents=True)
        (elsewhere / "personas" / "stolen").symlink_to(stolen, target_is_directory=True)
        root.mkdir()
        (root / ".aisquare").symlink_to(elsewhere, target_is_directory=True)
    else:
        (root / ".aisquare").mkdir(parents=True)
        (root / ".aisquare" / "personas").symlink_to(stolen.parent, target_is_directory=True)

    report = persona_service.migrate_project_personas([root])

    assert [(item.name, item.outcome) for item in report.personas] == [(linked, "skipped")]
    assert "symbolic link" in (report.personas[0].detail or "")
    assert not (_user() / "stolen").exists()
    _nothing_stolen()
