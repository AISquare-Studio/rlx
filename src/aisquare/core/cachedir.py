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
from typing import Any

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


def _is_bare_repository(path: Path) -> bool:
    """A bare repository's own shape: ``HEAD``, ``objects`` and ``refs`` at its root, any of them
    a link too (a ``HEAD`` linked elsewhere is still a repository's, review of #58)."""
    return (
        os.path.lexists(path / "HEAD")
        and os.path.lexists(path / "objects")
        and os.path.lexists(path / "refs")
    )


def _device(path: Path | str) -> int | None:
    """The filesystem ``path`` itself is on (``os.lstat``, never following a link), or ``None``
    when it cannot be read. Not ``DirEntry.stat``, whose ``st_dev`` is 0 on Windows (review of
    #58, M1: every folder read as a mount, and reap kept every tree)."""
    try:
        return os.lstat(path).st_dev
    except OSError:
        return None


def same_filesystem(path: Path, root: Path) -> bool:
    """Whether ``path`` is on ``root``'s filesystem, both readable: no rule crosses a mount
    (review of #58, M3), whether the mount is ``path`` itself or a folder between them."""
    device = _device(path)
    return device is not None and device == _device(root)


BUNDLE_SIGNATURES = (b"# v2 git bundle\n", b"# v3 git bundle\n")
"""A git bundle's first line (git's bundle format, v2 and v3): a repository in one file."""
_BUNDLE_HEAD = len(BUNDLE_SIGNATURES[0])
_READ_ONLY = (
    os.O_RDONLY
    | getattr(os, "O_NOFOLLOW", 0)
    | getattr(os, "O_NONBLOCK", 0)
    | getattr(os, "O_BINARY", 0)
)


def is_bundle(path: Path | str) -> bool:
    """Whether ``path`` is a git bundle (manager #3697: a repository in one file): a regular
    file, no link followed, whose first line is a bundle's (:data:`BUNDLE_SIGNATURES`). Read
    without blocking (never a FIFO's reader), its first line alone."""
    return _bundle_or_unread(path) is True


def _bundle_or_unread(path: Path | str) -> bool | None:
    """``True`` for a bundle (:func:`is_bundle`), ``False`` for a file that is none or is gone,
    ``None`` for one that could not be read: what it holds is not known."""
    try:
        fd = os.open(path, _READ_ONLY)
    except FileNotFoundError:
        return False
    except OSError:
        return None
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            return False
        head = os.read(fd, _BUNDLE_HEAD)
    except OSError:
        return None
    finally:
        os.close(fd)
    return head in BUNDLE_SIGNATURES


def _is_git(name: str) -> bool:
    """``.git`` as git finds it, whatever its case on a case-insensitive disk (``.GIT``)."""
    return name.lower() == ".git"


def _is_file(path: Path) -> bool:
    try:
        return stat.S_ISREG(os.lstat(path).st_mode)
    except OSError:
        return False


def holds_repository(path: Path, *, below_only: bool = False) -> bool:
    """Whether a git repository of any kind is at or anywhere under ``path``: a ``.git`` (a
    clone's directory, a worktree's or a submodule's file, ``.GIT`` on a case-insensitive disk),
    a bare repository or a bundle (:func:`is_bundle`), ignored by git or not, links not
    followed (review of #58, C-a).
    ``below_only``: ``path``'s own ``.git`` (a checkout's) is neither counted nor walked into, a
    repository inside it counts. A folder or a file that cannot be read (#3804), or a folder
    on another filesystem (a mount: no rule crosses one, M3), is taken to hold one: kept."""
    device = _device(path)
    if device is None:
        return True
    if not below_only and _is_bare_repository(path):
        return True
    pending: list[tuple[Path, bool]] = [(path, True)]
    while pending:
        here, top = pending.pop()
        try:
            with os.scandir(here) as listing:
                entries = list(listing)
        except OSError:
            return True
        for entry in entries:
            if _is_git(entry.name):
                if top and below_only:
                    continue  # the checkout's own repository: never walked into, nor counted
                return True
            if not entry.is_dir(follow_symlinks=False):
                if (
                    entry.is_file(follow_symlinks=False)
                    and _bundle_or_unread(entry.path) is not False
                ):
                    # A repository in one file (manager #3697), or a file that could not be
                    # read, as an unreadable folder is: taken to hold one (#3804).
                    return True
                continue
            inner = Path(entry.path)
            if _device(inner) != device:
                return True  # a mount, or unreadable: not ours to walk, so taken to hold one
            if _is_bare_repository(inner):
                return True
            pending.append((inner, False))
    return False


def newest_mtime(path: Path, *, limit: int = 200_000) -> float | None:
    """The newest modification time of ``path`` and everything under it, links not followed;
    ``None`` past ``limit`` entries, at a mount (never crossed, M3), or where something cannot
    be read: a caller then takes it to be recent."""
    device = _device(path)
    try:
        newest = os.lstat(path).st_mtime
    except OSError:
        return None
    pending = [path]
    seen = 0
    while pending:
        here = pending.pop()
        try:
            with os.scandir(here) as listing:
                entries = list(listing)
        except OSError:
            return None
        for entry in entries:
            seen += 1
            if seen > limit:
                return None
            try:
                info = os.lstat(entry.path)
            except OSError:
                return None
            newest = max(newest, info.st_mtime)
            if stat.S_ISDIR(info.st_mode):
                if _device(entry.path) != device:
                    return None  # a mount: what is on it is not ours to date (M3)
                pending.append(Path(entry.path))
    return newest


def holds_environment(path: Path) -> bool:
    """Whether a virtual environment (PEP 405's ``pyvenv.cfg``) is at or anywhere under
    ``path``, links not followed."""
    pending = [path]
    while pending:
        here = pending.pop()
        if is_environment(here):
            return True
        pending.extend(_subdirectories(here))
    return False


def _entries(path: Path) -> Iterator[os.DirEntry[str]]:
    try:
        with os.scandir(path) as listing:
            yield from listing
    except OSError:
        return


def is_cache(path: Path) -> bool:
    """Whether ``path`` is a removable cache: tagged by the spec, holding no repository of any
    kind and crossing no mount (:func:`holds_repository`)."""
    return is_cache_dir(path) and not holds_repository(path)


def cache_dirs(root: Path, *, skip: Callable[[Path], bool] | None = None) -> list[Path]:
    """Every cache STRICTLY under ``root`` (:func:`is_cache`; never ``root`` itself, whose tag
    authorises nothing), outermost only, links not followed, never inside a nested checkout (a
    directory holding a ``.git``) nor inside a directory ``skip`` says to pass over."""
    device = _device(root)
    found: list[Path] = []
    pending = [entry for entry in _subdirectories(root)]
    while pending:
        here = pending.pop()
        if device is None or _device(here) != device:
            continue  # a mount: no rule crosses one (review of #58, M3)
        if skip is not None and skip(here):
            continue
        if is_cache_dir(here):
            if not holds_repository(here):
                found.append(here)
            continue
        if _there(here / ".git") or _is_bare_repository(here):
            continue  # a repository: git judges it, never a tag
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
        return not holds_repository(root)
    if not _is_dir(root):
        return False
    try:
        with os.scandir(root) as listing:
            entries = list(listing)
    except OSError:
        return False
    device = _device(root)
    return all(
        entry.is_dir(follow_symlinks=False)
        and _device(entry.path) == device  # a mount holds what tidy will not judge (M3)
        and only_caches(Path(entry.path))
        for entry in entries
    )


def remove_dir(path: Path | str, *, dir_fd: int | None = None) -> None:
    """Delete the directory ``path`` and all it holds, following no link (``shutil.rmtree``
    unlinks a link it meets, and refuses one given as ``path``). With ``dir_fd``, ``path`` is a
    name in that folder, opened and checked by the caller (``rlx tidy``'s pinned delete). What
    refuses (a read-only folder) is made writable, inside ``path`` alone and never through a
    link, and a second pass removes it; what still refuses is raised then."""
    top = None if dir_fd is not None else os.path.realpath(path)

    def loosen(_function: Callable[..., object], name: str, _exc: BaseException | object) -> None:
        """Never makes the failed call again: rmtree's ``os.open`` takes flags this hook does
        not have (review of #58, a TypeError that aborted tidy and reap)."""
        for target in (name, os.path.dirname(name)):
            if not target:
                continue
            try:
                if dir_fd is not None:
                    info = os.lstat(target, dir_fd=dir_fd)
                    if stat.S_ISLNK(info.st_mode):
                        continue
                    os.chmod(target, _writable(info), dir_fd=dir_fd)
                    continue
                real = os.path.realpath(target)
                if os.path.islink(target) or os.path.commonpath([real, str(top)]) != top:
                    continue
                os.chmod(target, _writable(os.lstat(target)))
            except (OSError, NotImplementedError):
                continue

    def gone() -> bool:
        try:
            os.lstat(path, dir_fd=dir_fd) if dir_fd is not None else os.lstat(path)
        except OSError:
            return True
        return False

    kwargs: dict[str, Any] = {} if dir_fd is None else {"dir_fd": dir_fd}
    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=loosen, **kwargs)
    else:  # pragma: no cover - 3.11 names the hook onerror
        shutil.rmtree(path, onerror=loosen, **kwargs)
    if not gone():
        shutil.rmtree(path, **kwargs)  # what still refuses is raised now


def _writable(info: os.stat_result) -> int:
    return stat.S_IRWXU if stat.S_ISDIR(info.st_mode) else stat.S_IRUSR | stat.S_IWUSR


def remove_within(path: Path, root: Path, *, allows: Callable[[Path], bool] | None = None) -> None:
    """Delete the cache ``path``, refusing (``ValueError``) a link, ``root`` itself, a path that
    holds a repository or crosses a mount, one whose real place is not strictly inside
    ``root``'s (a tree reached through a link deletes nothing outside it), and one ``allows``
    (a caller's own roots: ``rlx tidy``'s) refuses (review of #58)."""
    info = os.lstat(path)
    if stat.S_ISLNK(info.st_mode):
        raise ValueError(f"{path} is a link")
    if allows is not None and not allows(path):
        raise ValueError(f"{path} is outside the folders the caller may delete in")
    parent, top = os.path.realpath(path.parent), os.path.realpath(root)
    here = os.path.join(parent, path.name)
    if here == top or os.path.commonpath([here, top]) != top:
        raise ValueError(f"{path} is not inside {root}")
    if not same_filesystem(path, root):
        raise ValueError(f"{path} is on another filesystem than {root}")
    if holds_repository(path):
        raise ValueError(f"{path} holds a repository, or crosses a mount")
    remove_dir(path)


def allocated_bytes(root: Path) -> int:
    """What ``root`` takes on disk: each file, link and directory under it once, by inode, as
    the blocks it holds (its size where the system says no blocks, as Windows does). Links are
    counted as links, never followed; a mount is not walked (it is not this disk's, M3); what
    cannot be read counts nothing."""
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
            if stat.S_ISDIR(info.st_mode) and _device(entry.path) == top.st_dev:
                pending.append(Path(entry.path))
    return total
