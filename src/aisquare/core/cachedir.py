"""Directories the Cache Directory Tagging spec marks regenerable, and what they hold on disk.

A directory holding a ``CACHEDIR.TAG`` file that starts with the spec's signature is a cache
by its own word (https://bford.info/cachedir/): its contents can be made again, so deleting it
loses no work. Cargo's ``target/``, uv's caches and virtualenvs, pytest's ``.pytest_cache``,
ruff's and Go's caches write one. ``rlx tidy`` (``services.tidy``) and ``fleet reap``'s
judgement of an ended agent's tree (``services.fleet._why_kept``) read it here, by the tag and
never by a directory's name, so no language's build output is special.

Every walk here is ``lstat``-based and follows no symlink, so a link out of a tree is never
read through, and a tagged directory is not descended into: its contents are the cache's.
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


def _entries(path: Path) -> Iterator[os.DirEntry[str]]:
    try:
        with os.scandir(path) as listing:
            yield from listing
    except OSError:
        return


def cache_dirs(root: Path) -> list[Path]:
    """Every tagged directory at or under ``root``, outermost only, links not followed."""
    if is_cache_dir(root):
        return [root]
    found: list[Path] = []
    for entry in _entries(root):
        if entry.is_dir(follow_symlinks=False):
            found.extend(cache_dirs(Path(entry.path)))
    return found


def only_caches(root: Path) -> bool:
    """Whether everything under ``root`` lies inside tagged directories: no file, link or
    anything else outside one. An empty directory holds nothing; one that cannot be read is
    taken to hold something, the side that keeps it."""
    if is_cache_dir(root):
        return True
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
        os.chmod(name, stat.S_IWRITE | stat.S_IREAD | stat.S_IEXEC)
        function(name)

    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=writable_then_again)
    else:  # pragma: no cover - 3.11 names the hook onerror
        shutil.rmtree(path, onerror=writable_then_again)


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
