"""``rlx tidy`` and reap's repository scan, on every platform Windows included (review of #58).

``tests/test_tidy.py`` needs POSIX file semantics (``os.utime`` on links, symlinks, unlinking a
held lock, sandbox paths past Windows' MAX_PATH) and skips on native Windows, where tidy deletes
nothing in 0.10.2 (manager #3647). What must hold there too is here: the scan reap keeps a tree
by (M1: ``DirEntry.stat``'s ``st_dev`` is 0 on Windows, and every tree read as holding a
repository), the mount boundary (M3, through ``cachedir._device``), and tidy reporting only.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core import cachedir
from aisquare.services import fleet as fleet_service
from aisquare.services import tidy

TAG = cachedir.SIGNATURE + b"\n"


def _git(*args: str, cwd: Path) -> str:
    done = subprocess.run(
        ["git", "-c", "user.email=t@t", "-c", "user.name=t", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
    )
    return done.stdout.strip()


def _tree(root: Path) -> Path:
    """A plain tree, folders within folders, no repository in it."""
    for rel in ("src/pkg", "docs/img", "build/out"):
        (root / rel).mkdir(parents=True)
        (root / rel / "file.txt").write_text("x\n", encoding="utf-8")
    return root


def test_a_plain_tree_holds_no_repository(tmp_path: Path) -> None:
    """M1: read through ``DirEntry.stat``, every folder was a mount on Windows (``st_dev`` 0)."""
    assert not cachedir.holds_repository(_tree(tmp_path / "tree"))


def _repo(repo: Path) -> Path:
    """A main checkout with one commit on ``main``, and nothing else in it."""
    repo.mkdir()
    _git("init", "-q", "-b", "main", cwd=repo)
    (repo / "src").mkdir()
    (repo / "src" / "a.txt").write_text("a\n", encoding="utf-8")
    _git("add", ".", cwd=repo)
    _git("commit", "-q", "-m", "a", cwd=repo)
    return repo


def test_a_clean_merged_checkout_is_judged_removable_by_reap(tmp_path: Path) -> None:
    """M1, reap's regression on Windows: a clean checkout read as holding another repository."""
    repo = _repo(tmp_path / "repo")

    assert not cachedir.holds_repository(repo, below_only=True)
    assert fleet_service._why_kept(repo, repo, "main") is None


@pytest.mark.parametrize("name", [".git", ".GIT"])
def test_a_repository_is_found_by_its_git_whatever_its_case(tmp_path: Path, name: str) -> None:
    tree = _tree(tmp_path / "tree")
    (tree / "vendor" / "lib").mkdir(parents=True)
    (tree / "vendor" / "lib" / name).write_text("gitdir: elsewhere\n", encoding="utf-8")

    assert cachedir.holds_repository(tree, below_only=True)


@pytest.mark.skipif(sys.platform == "win32", reason="a symlink needs a privilege on Windows")
def test_a_bare_repository_with_a_linked_head_is_a_repository(tmp_path: Path) -> None:
    bare = tmp_path / "tree" / "mirror"
    (bare / "objects").mkdir(parents=True)
    (bare / "refs").mkdir()
    (tmp_path / "HEAD.real").write_text("ref: refs/heads/main\n", encoding="utf-8")
    (bare / "HEAD").symlink_to(tmp_path / "HEAD.real")

    assert cachedir.holds_repository(tmp_path / "tree")


def test_no_walk_and_no_delete_crosses_a_mount(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """M3: a folder on another filesystem (``cachedir._device``, the seam every walk asks) is not
    walked, not freed, not dated, and holds what no rule judges: reap keeps a tree that has
    one, and its own delete refuses one."""
    tree = _tree(tmp_path / "tree")
    mounted = tree / "mnt"
    (mounted / "deep").mkdir(parents=True)
    (mounted / cachedir.TAG).write_bytes(TAG)
    (mounted / "deep" / cachedir.TAG).write_bytes(TAG)
    (mounted / "deep" / "big.bin").write_bytes(b"x" * 2_000_000)
    inside = tree / "build" / "cache"
    inside.mkdir()
    (inside / cachedir.TAG).write_bytes(TAG)
    real = cachedir._device

    def device(path: Path | str) -> int | None:
        found = real(path)
        on_it = Path(path) == mounted or Path(path).is_relative_to(mounted)
        return None if found is None else found + (7 if on_it else 0)

    monkeypatch.setattr(cachedir, "_device", device)

    assert cachedir.cache_dirs(tree) == [inside]
    assert cachedir.holds_repository(tree) and cachedir.holds_repository(tree, below_only=True)
    assert cachedir.newest_mtime(tree) is None
    assert cachedir.newest_mtime(tree / "build") is not None
    assert not cachedir.same_filesystem(mounted, tree)
    assert not cachedir.only_caches(tree)
    assert cachedir.allocated_bytes(tree) < 1_000_000, "a mount's content is not this disk's"
    with pytest.raises(ValueError):
        cachedir.remove_within(mounted, tree)
    with pytest.raises(ValueError):
        cachedir.remove_within(mounted / "deep", tree)
    assert (mounted / "deep" / cachedir.TAG).is_file()
    cachedir.remove_within(inside, tree)
    assert not inside.exists()


def test_a_folder_that_cannot_be_read_is_taken_to_hold_a_repository(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    tree = _tree(tmp_path / "tree")
    monkeypatch.setattr(cachedir, "_device", lambda path: None)

    assert cachedir.holds_repository(tree) and not cachedir.same_filesystem(tree, tree)


def test_a_caller_s_roots_bound_reaps_delete(tmp_path: Path) -> None:
    cache = tmp_path / "tree" / "target"
    cache.mkdir(parents=True)
    (cache / cachedir.TAG).write_bytes(TAG)
    with pytest.raises(ValueError):
        cachedir.remove_within(cache, tmp_path / "tree", allows=lambda path: False)
    assert cache.is_dir()


def test_tidy_deletes_except_on_native_windows() -> None:
    """Pinned per platform (manager #3647): on native Windows tidy reports and lists alone."""
    assert tidy.DELETES is (sys.platform != "win32")


def test_a_report_only_tidy_frees_nothing_and_says_why(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    lock = tmp_path / "system-temp" / f"rlx-home-migration-{'c' * 16}.lock"
    lock.parent.mkdir()
    lock.write_bytes(b"")
    old = 1_000_000_000.0
    os.utime(lock, (old, old))
    monkeypatch.setattr(tidy, "DELETES", False)
    assert tidy._system_temp_dirs() == [tmp_path / "system-temp"]  # the suite's sandbox

    made = tidy.plan()
    report = tidy.apply(made)
    shown = CliRunner().invoke(app, ["tidy"])
    real = json.loads(CliRunner().invoke(app, ["--json", "tidy"]).stdout)

    assert [item.path for item in made.items] == [lock]
    assert report.freed == [] and report.not_freed == [(made.items[0], tidy.REPORTS_ONLY)]
    assert tidy.REPORTS_ONLY in shown.output and lock.exists()
    assert real["freed"] == [] and real["not_freed"][0]["why"] == tidy.REPORTS_ONLY


# --- round 5 (review #3696, manager #3697) --------------------------------------------------


@pytest.mark.parametrize("version", ["2", "3"])
def test_a_git_bundle_is_a_repository(tmp_path: Path, version: str) -> None:
    """A repository in one file, whatever its name: git's own bundle, by its first line."""
    repo = _repo(tmp_path / "repo")
    tree = _tree(tmp_path / "tree")
    bundle = tree / "docs" / "img" / "snapshot"
    _git("bundle", "create", f"--version={version}", str(bundle), "--all", cwd=repo)

    assert bundle.read_bytes().startswith(f"# v{version} git bundle\n".encode())
    assert cachedir.is_bundle(bundle)
    assert cachedir.holds_repository(tree) and cachedir.holds_repository(tree, below_only=True)


def test_a_file_that_only_looks_like_a_bundle_is_none(tmp_path: Path) -> None:
    tree = _tree(tmp_path / "tree")
    lookalikes = {
        "longer": b"# v2 git bundles are files\n",
        "unknown": b"# v4 git bundle\n",
        "no-newline": b"# v2 git bundle",
        "empty": b"",
        "short": b"# v",
    }
    for name, head in lookalikes.items():
        (tree / "docs" / name).write_bytes(head)

    assert not any(cachedir.is_bundle(tree / "docs" / name) for name in lookalikes)
    assert not cachedir.holds_repository(tree)


@pytest.mark.skipif(sys.platform == "win32", reason="a symlink needs a privilege on Windows")
def test_a_link_to_a_bundle_elsewhere_is_no_repository_here(tmp_path: Path) -> None:
    repo = _repo(tmp_path / "repo")
    bundle = tmp_path / "elsewhere.bundle"
    _git("bundle", "create", str(bundle), "--all", cwd=repo)
    tree = _tree(tmp_path / "tree")
    (tree / "docs" / "linked.bundle").symlink_to(bundle)

    assert not cachedir.is_bundle(tree / "docs" / "linked.bundle")
    assert not cachedir.holds_repository(tree)


def test_reap_frees_caches_except_on_native_windows() -> None:
    """Pinned per platform (manager #3697): on native Windows reap judges a tree as 0.10.1."""
    assert fleet_service.REAP_FREES_CACHES is (sys.platform != "win32")


def test_with_its_cache_step_off_reap_judges_a_tree_as_0_10_1_did(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Off (native Windows): an untracked cache is uncommitted content that keeps the tree, and
    reap frees nothing in it. On: the cache is no reason to keep it."""
    repo = _repo(tmp_path / "repo")
    target = repo / "target"
    target.mkdir()
    (target / cachedir.TAG).write_bytes(TAG)
    (target / "out.bin").write_bytes(b"x" * 64)

    monkeypatch.setattr(fleet_service, "REAP_FREES_CACHES", False)
    assert fleet_service._untracked_caches(repo) == (None, [], True)
    assert fleet_service._why_kept(repo, repo, "main") == "uncommitted changes"
    assert fleet_service._remove_untracked_caches(repo) == []
    assert (target / "out.bin").is_file()
    # Its delete holds the switch too, whatever the listing says.
    with monkeypatch.context() as listed:
        listed.setattr(fleet_service, "_untracked_caches", lambda tree: (None, [target], False))
        assert fleet_service._remove_untracked_caches(repo) == []
    assert (target / "out.bin").is_file()
    monkeypatch.setattr(fleet_service, "REAP_FREES_CACHES", True)
    assert fleet_service._why_kept(repo, repo, "main") is None
