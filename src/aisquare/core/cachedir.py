"""Directories the Cache Directory Tagging spec marks regenerable, and what they hold on disk.

A directory holding a ``CACHEDIR.TAG`` file that starts with the spec's signature is a cache
by its own word (https://bford.info/cachedir/): its contents can be made again, so deleting it
loses no work. Cargo's ``target/``, uv's caches and virtualenvs, pytest's ``.pytest_cache``,
ruff's and Go's caches write one. ``rlx tidy`` (``services.tidy``) and ``fleet reap``'s
judgement of an ended agent's tree (``services.fleet._why_kept``) read it here, by the tag and
never by a directory's name, so no language's build output is special.

Every walk here is ``lstat``-based and follows no symlink, so a link out of a tree is never
read through, and a tagged directory is not descended into: its contents are the cache's.

**A tag never authorises a root, nor anything holding a git checkout** (review of #58, C2): a
``CACHEDIR.TAG`` at the root of what is judged (a tree, a checkout, a scratchpad, a session)
counts for nothing, since ``CARGO_TARGET_DIR``, ``UV_CACHE_DIR`` or ``pytest -o cache_dir=.``
can put one there over real work; and a tagged directory with a ``.git`` anywhere under it is
no cache. Git judges those, never the tag.
"""

from __future__ import annotations

import os
import shutil
import stat
import sys
from collections.abc import Callable, Iterator
from pathlib import Path

TAG = "CACHEDIR.TAG"
SIGNATURE = b"Signature: 8a477f597d28d172789f06886806bc55"
"""What a ``CACHEDIR.TAG`` starts with, by the spec; a file without it tags nothing."""
ENVIRONMENT_MARKER = "pyvenv.cfg"
"""PEP 405's file at a virtual environment's root. uv tags the environments it makes, but a
live agent runs its commands from one: the one standard-marker exception ``rlx tidy`` makes,
for a live coder's tree only (``services.tidy``)."""


def _is_dir(path: Path) -> bool:
    try:
        return stat.S_ISDIR(os.lstat(path).st_mode)
    except OSError:
        return False


def is_cache_dir(path: Path) -> bool:
    """Whether ``path`` is a directory (not a link to one) tagged by the spec."""
    if not _is_dir(path):
        return False
    tag = path / TAG
    try:
        if not stat.S_ISREG(os.lstat(tag).st_mode):
            return False
        with tag.open("rb") as handle:
            return handle.read(len(SIGNATURE)) == SIGNATURE
    except OSError:
        return False


def is_environment(path: Path) -> bool:
    """Whether ``path`` is a virtual environment: PEP 405's ``pyvenv.cfg`` at its root."""
    try:
        return stat.S_ISREG(os.lstat(path / ENVIRONMENT_MARKER).st_mode)
    except OSError:
        return False


def has_git(path: Path) -> bool:
    """Whether a ``.git`` (a repository's directory or a worktree's file) is at or anywhere under
    ``path``, links not followed. One that cannot be read is taken to hold one: kept."""
    pending = [path]
    while pending:
        here = pending.pop()
        try:
            with os.scandir(here) as listing:
                entries = list(listing)
        except OSError:
            return True
        for entry in entries:
            if entry.name == ".git":
                return True
            if entry.is_dir(follow_symlinks=False):
                pending.append(Path(entry.path))
    return False


def _entries(path: Path) -> Iterator[os.DirEntry[str]]:
    try:
        with os.scandir(path) as listing:
            yield from listing
    except OSError:
        return


def is_cache(path: Path) -> bool:
    """Whether ``path`` is a removable cache: tagged by the spec, holding no ``.git``."""
    return is_cache_dir(path) and not has_git(path)


def cache_dirs(root: Path, *, skip: Callable[[Path], bool] | None = None) -> list[Path]:
    """Every cache STRICTLY under ``root`` (:func:`is_cache`; never ``root`` itself, whose tag
    authorises nothing), outermost only, links not followed, never inside a nested checkout (a
    directory holding a ``.git``) nor inside a directory ``skip`` says to pass over."""
    found: list[Path] = []
    pending = [entry for entry in _subdirectories(root)]
    while pending:
        here = pending.pop()
        if skip is not None and skip(here):
            continue
        if is_cache_dir(here):
            if not has_git(here):
                found.append(here)
            continue
        if _there(here / ".git"):
            continue  # a nested checkout: git judges it, never a tag
        pending.extend(_subdirectories(here))
    return sorted(found)


def _subdirectories(path: Path) -> list[Path]:
    return [Path(entry.path) for entry in _entries(path) if entry.is_dir(follow_symlinks=False)]


def _there(path: Path) -> bool:
    try:
        os.lstat(path)
    except OSError:
        return False
    return True


def only_caches(root: Path) -> bool:
    """Whether everything under ``root`` (an untracked entry of a tree, never the tree itself)
    lies inside caches (:func:`is_cache`): no file, link, ``.git`` or anything else outside one.
    An empty directory holds nothing; one that cannot be read is taken to hold something, the
    side that keeps it."""
    if is_cache_dir(root):
        return not has_git(root)
    if not _is_dir(root):
        return False
    try:
        with os.scandir(root) as listing:
            entries = list(listing)
    except OSError:
        return False
    return all(
        entry.is_dir(follow_symlinks=False) and only_caches(Path(entry.path)) for entry in entries
    )


def remove_dir(path: Path) -> None:
    """Delete the directory ``path`` and all it holds, following no link (``shutil.rmtree``
    unlinks a link it meets, and refuses one given as ``path``); a read-only entry, as git and
    some builds leave, is made writable and tried again. Raises what the delete raised."""

    def writable_then_again(
        function: Callable[..., object], name: str, _exc: BaseException | object
    ) -> None:
        if os.path.islink(name):
            os.unlink(name)  # a link is unlinked, never chmod'ed through
            return
        os.chmod(name, stat.S_IWRITE | stat.S_IREAD | stat.S_IEXEC)
        function(name)

    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=writable_then_again)
    else:  # pragma: no cover - 3.11 names the hook onerror
        shutil.rmtree(path, onerror=writable_then_again)


def remove_within(path: Path, root: Path) -> None:
    """Delete the cache ``path``, refusing (``ValueError``) a link, ``root`` itself, a path that
    holds a ``.git``, and one whose real place is not strictly inside ``root``'s: a tree reached
    through a link deletes nothing outside it (review of #58)."""
    info = os.lstat(path)
    if stat.S_ISLNK(info.st_mode):
        raise ValueError(f"{path} is a link")
    parent, top = os.path.realpath(path.parent), os.path.realpath(root)
    here = os.path.join(parent, path.name)
    if here == top or os.path.commonpath([here, top]) != top:
        raise ValueError(f"{path} is not inside {root}")
    if has_git(path):
        raise ValueError(f"{path} holds a git checkout")
    remove_dir(path)


def allocated_bytes(root: Path) -> int:
    """What ``root`` takes on disk: each file, link and directory under it once, by inode, as
    the blocks it holds (its size where the system says no blocks, as Windows does). Links are
    counted as links, never followed; what cannot be read counts nothing."""
    seen: set[tuple[int, int]] = set()

    def counted(info: os.stat_result) -> int:
        key = (info.st_dev, info.st_ino)
        if key in seen and info.st_ino:
            return 0
        seen.add(key)
        blocks = getattr(info, "st_blocks", None)
        return blocks * 512 if blocks is not None else info.st_size

    try:
        top = os.lstat(root)
    except OSError:
        return 0
    total = counted(top)
    if not stat.S_ISDIR(top.st_mode):
        return total
    pending = [root]
    while pending:
        here = pending.pop()
        for entry in _entries(here):
            try:
                info = entry.stat(follow_symlinks=False)
            except OSError:
                continue
            total += counted(info)
            if stat.S_ISDIR(info.st_mode):
                pending.append(Path(entry.path))
    return total
