"""rlxd and rlxd-hook from the release that matches this rlx (handoff 0.10.3, 1.2).

rlxd's version is rlx's, and it lives in uv's tool bin directory beside ``rlx``. The
installers put it there and ``rlx upgrade`` replaces it; someone who installed with plain
``uv tool install`` runs ``rlx daemon install``, which is :func:`install`:

1. It names this machine's target (:func:`host_target`) and its archive,
   ``rlxd-<target>.tar.gz`` or ``rlxd-x86_64-pc-windows-msvc.zip``.
2. It fetches ``rlxd-SHA256SUMS`` of release ``v<version>``, then the archive, from
   ``<releases>/v<version>/<name>``. ``<releases>`` is GitHub's download root for the repo,
   or ``AISQUARE_RELEASES_URL`` when set: the stub releases server of the handoff's
   pre-release checks. Whatever names it, the root must be ``https://``, or ``http://`` to
   this machine (127.0.0.1, ::1, localhost) for a test server (:func:`check_releases_url`),
   and a redirect is followed only while it stays so: never from https to anything else
   (:func:`check_redirect`). The sums come from the same root as the archive, so the root
   is what the sha256 check vouches for.
3. It checks the archive's sha256 against its line in the sums BEFORE anything is
   written, and the archive's members: exactly ``rlxd`` and ``rlxd-hook`` (``.exe`` on
   Windows), regular files, no directory.
4. It writes each binary to a temporary file in the bin directory, sets the exec bit
   (not on Windows), and renames it over the old one, so a binary is never half written.
   Both or neither: each old binary is first set aside, and a failure puts both back, so
   ``rlxd`` and ``rlxd-hook`` are never from different versions (:func:`_place`). On
   Windows the old one is renamed aside, which Windows allows for a running ``rlxd.exe``
   where it refuses to replace it; an aside still in use is removed by the next install.

Every refusal is an :class:`InstallError` with a code: ``unsafe_releases_url``,
``unsafe_redirect``, ``release_not_found`` (no sums at that version), ``archive_not_listed``,
``archive_not_found``, ``checksum_mismatch``, ``bad_archive``, ``download_failed``,
``unsupported_platform`` and ``install_failed``. None of them changes what is installed:
``install_failed`` (the bin directory cannot be written) puts the old binaries back.

The bin directory is decided as ``uv tool dir --bin`` decides it (:func:`uv_tool_bin_dir`),
without running uv. ``RLX_HOME`` and ``AISQUARE_HOME`` play no part.

The network is reached through :func:`open_release_url` alone, so a test can replace it; the
standard-library modules are imported inside, off the path every command pays to start.
scripts/rlxd_archive.py makes the archives and the sums, by the same names as here.
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import os
import platform as platform_module
import re
import shutil
import sys
import tempfile
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from aisquare.core.version import DISTRIBUTION, __version__

RELEASES_URL = "https://github.com/AISquare-Studio/rlx/releases/download"
"""Where release ``vX.Y.Z``'s assets are: ``<this>/vX.Y.Z/<name>``."""
RELEASES_URL_ENV = "AISQUARE_RELEASES_URL"
"""Names another root with the same layout: the stub releases server in tests and CI."""
LOOPBACK_HOSTS = frozenset({"127.0.0.1", "::1", "localhost"})
"""The hosts a plain ``http://`` root may name: this machine, for a test server."""

TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
)
BINARIES = ("rlxd", "rlxd-hook")
SUMS = "rlxd-SHA256SUMS"

TIMEOUT_SECONDS = 60.0
MAX_ARCHIVE_BYTES = 256 * 1024 * 1024
MAX_SUMS_BYTES = 64 * 1024

_SUM_LINE = re.compile(r"^([0-9a-fA-F]{64}) [ *](\S+)$")


class InstallError(Exception):
    """A refusal, with the code ``--json`` reports and a message a person acts on."""

    def __init__(self, code: str, message: str, *, detail: str | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.detail = detail


@dataclass(frozen=True)
class Installed:
    """What :func:`install` put where."""

    version: str
    target: str
    archive: str
    url: str
    sha256: str
    bin_dir: Path
    binaries: tuple[Path, ...]
    releases_url: str

    def to_json(self) -> dict[str, Any]:
        return {
            "installed": True,
            "version": self.version,
            "target": self.target,
            "archive": self.archive,
            "releases_url": self.releases_url,
            "url": self.url,
            "sha256": self.sha256,
            "bin_dir": str(self.bin_dir),
            "binaries": [str(path) for path in self.binaries],
        }


# --- names ------------------------------------------------------------------------------


def is_windows_target(target: str) -> bool:
    return target.endswith("-windows-msvc")


def binary_names(target: str) -> tuple[str, ...]:
    """``rlxd`` and ``rlxd-hook``, with ``.exe`` for Windows."""
    suffix = ".exe" if is_windows_target(target) else ""
    return tuple(name + suffix for name in BINARIES)


def archive_name(target: str) -> str:
    return f"rlxd-{target}.zip" if is_windows_target(target) else f"rlxd-{target}.tar.gz"


def host_target(platform: str | None = None, machine: str | None = None) -> str:
    """The target whose binaries run here.

    Windows on Arm gets the x86_64 build, which Windows 11 runs under its emulation; there
    is no aarch64 Windows build. Linux means glibc, as the Linux builds are."""
    platform = sys.platform if platform is None else platform
    arch = (platform_module.machine() if machine is None else machine).lower()
    arm = arch in ("arm64", "aarch64")
    x86 = arch in ("x86_64", "amd64", "x64")
    if platform == "darwin" and (arm or x86):
        return "aarch64-apple-darwin" if arm else "x86_64-apple-darwin"
    if platform.startswith("linux") and (arm or x86):
        return "aarch64-unknown-linux-gnu" if arm else "x86_64-unknown-linux-gnu"
    if platform == "win32" and (arm or x86):
        return "x86_64-pc-windows-msvc"
    raise InstallError(
        "unsupported_platform",
        f"rlxd has no build for {platform} on {arch or 'an unknown machine'}; "
        f"it ships for {', '.join(TARGETS)}",
    )


def uv_tool_bin_dir(env: Mapping[str, str] | None = None, home: Path | None = None) -> Path:
    """Where uv puts a tool's executables, decided as ``uv tool dir --bin`` decides it.

    ``UV_TOOL_BIN_DIR`` when set, as uv takes it: a relative one is the working
    directory's (``uv tool dir --bin`` prints it as given, and installs there). Else the
    first of ``XDG_BIN_HOME`` and ``XDG_DATA_HOME/../bin`` that is absolute: uv ignores a
    relative XDG value, as the XDG spec says to. Else ``~/.local/bin``."""
    env = os.environ if env is None else env
    tool_bin = env.get("UV_TOOL_BIN_DIR", "")
    if tool_bin:
        return Path(tool_bin) if os.path.isabs(tool_bin) else Path.cwd() / tool_bin
    xdg_bin = env.get("XDG_BIN_HOME", "")
    if xdg_bin and os.path.isabs(xdg_bin):
        return Path(xdg_bin)
    data = env.get("XDG_DATA_HOME", "")
    if data and os.path.isabs(data):
        return Path(data) / "../bin"
    return (Path.home() if home is None else home) / ".local" / "bin"


def releases_url(env: Mapping[str, str] | None = None) -> str:
    env = os.environ if env is None else env
    return (env.get(RELEASES_URL_ENV) or RELEASES_URL).rstrip("/")


def _allowed_url(url: str) -> bool:
    """``https://`` to any host, or ``http://`` to this machine; nothing else (``file:``,
    ``ftp:``, a plain-http host elsewhere)."""
    from urllib.parse import urlsplit

    try:
        parts = urlsplit(url)
        host = (parts.hostname or "").lower()
    except ValueError:
        return False
    scheme = parts.scheme.lower()
    if scheme == "https":
        return bool(host)
    return scheme == "http" and host in LOOPBACK_HOSTS


def check_releases_url(base: str) -> str:
    """``base``, if a release may come from it (:func:`_allowed_url`); refused by name if not.

    The sums come from the same root as the archive, so a root the user did not mean would
    pass the sha256 check: only GitHub's, an https mirror, or a test server here."""
    if not _allowed_url(base):
        raise InstallError(
            "unsafe_releases_url",
            f"refusing to install rlxd from {base}: the releases URL ({RELEASES_URL_ENV}) "
            "must be https://, or http:// to this machine (127.0.0.1, ::1, localhost) "
            "for a test server; nothing installed",
            detail=base,
        )
    return base


def check_redirect(from_url: str, to_url: str) -> None:
    """Refuse (``unsafe_redirect``) a redirect that leaves https, or goes where a root could
    not be (:func:`_allowed_url`). GitHub's download URL redirects to its https CDN, which
    passes; urllib alone would follow one to plain http."""
    from urllib.parse import urlsplit

    downgrade = (
        urlsplit(from_url).scheme.lower() == "https" and urlsplit(to_url).scheme.lower() != "https"
    )
    if downgrade or not _allowed_url(to_url):
        raise InstallError(
            "unsafe_redirect",
            f"refusing to install rlxd: {from_url} redirected to {to_url}, which is not "
            "https; nothing installed",
            detail=f"{from_url} -> {to_url}",
        )


def asset_url(base: str, version: str, name: str) -> str:
    return f"{base}/v{version}/{name}"


# --- the network --------------------------------------------------------------------------


def open_release_url(request: Any, *, timeout: float) -> Any:
    """Open ``request`` over ``core.tls``'s verified HTTPS: the one network call here. A
    redirect is followed only past :func:`check_redirect` (:func:`https_redirects_only`)."""
    import urllib.request

    from aisquare.core import tls

    opener = urllib.request.build_opener(https_redirects_only(), tls.https_handler())
    return opener.open(request, timeout=timeout)


def https_redirects_only() -> Any:
    """A ``urllib.request.HTTPRedirectHandler`` that raises :class:`InstallError`
    (``unsafe_redirect``) for a redirect :func:`check_redirect` refuses, and follows the
    rest as urllib does."""
    import urllib.request

    class _HttpsRedirectsOnly(urllib.request.HTTPRedirectHandler):
        def redirect_request(
            self, req: Any, fp: Any, code: Any, msg: Any, headers: Any, newurl: Any
        ) -> Any:
            try:
                check_redirect(req.full_url, newurl)
            except InstallError:
                fp.close()
                raise
            return super().redirect_request(req, fp, code, msg, headers, newurl)

    return _HttpsRedirectsOnly()


def _fetch(url: str, *, limit: int, missing: InstallError) -> bytes:
    """The body at ``url``; ``missing`` when it answers 404, ``download_failed`` otherwise."""
    from http.client import HTTPException
    from urllib.error import HTTPError, URLError
    from urllib.request import Request

    request = Request(url, headers={"User-Agent": f"{DISTRIBUTION}/{__version__}"})
    try:
        with open_release_url(request, timeout=TIMEOUT_SECONDS) as response:
            body: bytes = response.read(limit + 1)
    except HTTPError as exc:
        if exc.code == 404:
            raise missing from exc
        raise InstallError(
            "download_failed", f"could not download {url}: HTTP {exc.code}", detail=str(exc)
        ) from exc
    except (URLError, HTTPException, OSError, ValueError) as exc:
        raise InstallError(
            "download_failed", f"could not download {url}: {exc}", detail=repr(exc)
        ) from exc
    if len(body) > limit:
        raise InstallError("download_failed", f"{url} is larger than {limit} bytes")
    return body


# --- the sums and the archive ------------------------------------------------------------


def parse_sums(text: str) -> dict[str, str]:
    """``{name: sha256}`` from ``<sha256>  <name>`` lines (``*<name>`` too, shasum's binary
    mode); blank lines are skipped, and so is any other line, which names nothing."""
    sums: dict[str, str] = {}
    for line in text.splitlines():
        match = _SUM_LINE.match(line.strip())
        if match:
            sums[match.group(2)] = match.group(1).lower()
    return sums


def read_archive(target: str, data: bytes) -> dict[str, bytes]:
    """The two binaries in ``target``'s archive, by name.

    Refused (``bad_archive``) unless it holds exactly those two names, each a regular file,
    flat: no directory, no link, nothing else."""
    import tarfile
    import zipfile

    expected = set(binary_names(target))
    name = archive_name(target)
    files: dict[str, bytes] = {}
    try:
        if is_windows_target(target):
            with zipfile.ZipFile(io.BytesIO(data)) as archive:
                infos = archive.infolist()
                names = [info.filename for info in infos]
                _check_members(name, names, expected)
                for info in infos:
                    if info.is_dir():
                        raise InstallError("bad_archive", f"{name}: {info.filename} is a folder")
                    files[info.filename] = archive.read(info)
        else:
            with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
                members = archive.getmembers()
                _check_members(name, [member.name for member in members], expected)
                for member in members:
                    handle = archive.extractfile(member) if member.isfile() else None
                    if handle is None:
                        raise InstallError(
                            "bad_archive", f"{name}: {member.name} is not a regular file"
                        )
                    files[member.name] = handle.read()
    except (tarfile.TarError, zipfile.BadZipFile, EOFError, OSError) as exc:
        raise InstallError("bad_archive", f"{name} is not a readable archive: {exc}") from exc
    return files


def _check_members(archive: str, names: list[str], expected: set[str]) -> None:
    if sorted(names) != sorted(expected):
        raise InstallError(
            "bad_archive",
            f"{archive} must hold exactly {' and '.join(sorted(expected))}; "
            f"it holds {', '.join(names) or 'nothing'}",
        )


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# --- the install --------------------------------------------------------------------------


def install(
    version: str = __version__,
    *,
    target: str | None = None,
    bin_dir: Path | None = None,
    base: str | None = None,
) -> Installed:
    """Download, verify and install ``version``'s rlxd and rlxd-hook (the module docstring)."""
    target = host_target() if target is None else target
    bin_dir = uv_tool_bin_dir() if bin_dir is None else bin_dir
    base = check_releases_url(releases_url() if base is None else base.rstrip("/"))
    name = archive_name(target)

    sums_url = asset_url(base, version, SUMS)
    sums = parse_sums(
        _fetch(
            sums_url,
            limit=MAX_SUMS_BYTES,
            missing=InstallError(
                "release_not_found",
                f"there is no rlxd release v{version}: {sums_url} was not found",
                detail=f"HTTP 404 from {sums_url}",
            ),
        ).decode("utf-8", errors="replace")
    )
    expected = sums.get(name)
    if expected is None:
        raise InstallError(
            "archive_not_listed",
            f"{SUMS} of v{version} lists no {name}, so it cannot be checked; nothing installed",
            detail=sums_url,
        )

    url = asset_url(base, version, name)
    data = _fetch(
        url,
        limit=MAX_ARCHIVE_BYTES,
        missing=InstallError(
            "archive_not_found",
            f"{SUMS} of v{version} lists {name}, but {url} was not found; nothing installed",
            detail=f"HTTP 404 from {url}",
        ),
    )
    actual = sha256_hex(data)
    if actual != expected:
        raise InstallError(
            "checksum_mismatch",
            f"{name} does not match {SUMS}: its sha256 is {actual}, the sums say {expected}; "
            "nothing installed",
            detail=f"expected {expected}, got {actual}, from {url}",
        )
    files = read_archive(target, data)
    paths = _place(files, bin_dir, windows=is_windows_target(target))
    return Installed(version, target, name, url, actual, bin_dir, paths, base)


def _place(files: Mapping[str, bytes], bin_dir: Path, *, windows: bool) -> tuple[Path, ...]:
    """Install ``files`` in ``bin_dir``, both or neither.

    Each is written to a temporary file there first (executable, but on Windows). Then, one
    at a time, rlxd-hook before rlxd, the old binary is set aside (:func:`_set_aside`) and the
    new one renamed into its place. When any step fails, every binary set aside is put
    back and every new one with no old one is removed, so the bin directory holds the old
    pair again (``install_failed``). An aside is removed once both are in place; one Windows
    still runs is left, and removed by the next install (:func:`_sweep_set_aside`)."""
    order = sorted(files, key=lambda n: n.startswith("rlxd-hook"), reverse=True)
    staged: dict[str, Path] = {}
    aside: dict[str, Path] = {}
    placed: list[str] = []
    try:
        bin_dir.mkdir(parents=True, exist_ok=True)
        _sweep_set_aside(bin_dir)
        for name in order:
            fd, temp_name = tempfile.mkstemp(prefix=f".{name}.", suffix=".tmp", dir=bin_dir)
            staged[name] = Path(temp_name)
            with os.fdopen(fd, "wb") as handle:
                handle.write(files[name])
                handle.flush()
                os.fsync(handle.fileno())
            if not windows:
                os.chmod(temp_name, 0o755)
        for name in order:
            final = bin_dir / name
            if os.path.lexists(final):
                aside[name] = _set_aside(final, windows=windows)
            os.replace(staged[name], final)
            del staged[name]
            placed.append(name)
    except OSError as exc:
        unrestored = _put_back(bin_dir, placed, aside)
        raise InstallError(
            "install_failed",
            f"could not install into {bin_dir}: {exc}; "
            + (
                "the rlxd and rlxd-hook there are as they were"
                if not unrestored
                else f"and could not put back {', '.join(unrestored)}: run this again"
            ),
            detail=repr(exc),
        ) from exc
    finally:
        for temp in staged.values():
            temp.unlink(missing_ok=True)
    for old in aside.values():
        _remove_set_aside(old)
    return tuple(bin_dir / name for name in sorted(files))


def _set_aside(final: Path, *, windows: bool) -> Path:
    """Keep the binary at ``final`` beside it, as ``.<name>.<random>.old``, so it can be put
    back. On Windows it is renamed: a running ``rlxd.exe`` can be renamed but not replaced.
    Elsewhere it is hard-linked (copied where links are not allowed), so the rename of the
    new one over ``final`` stays atomic."""
    old = final.with_name(f".{final.name}.{os.urandom(4).hex()}.old")
    if windows:
        os.replace(final, old)
        return old
    try:
        os.link(final, old)
    except OSError:
        shutil.copy2(final, old)
    return old


def _put_back(bin_dir: Path, placed: list[str], aside: Mapping[str, Path]) -> list[str]:
    """Undo a failed :func:`_place`: each binary set aside goes back to its name, and each
    new one that had no old one is removed. Returns the names it could not restore."""
    unrestored: list[str] = []
    for name, old in aside.items():
        final = bin_dir / name
        try:
            if os.path.lexists(final) and os.path.samefile(old, final):
                # Never replaced: ``old`` is a link to it, and a rename between two links
                # to one file does nothing (POSIX), so the link is removed instead.
                os.unlink(old)
            else:
                os.replace(old, final)
        except OSError:
            unrestored.append(name)
    for name in placed:
        if name not in aside:
            try:
                (bin_dir / name).unlink(missing_ok=True)
            except OSError:
                unrestored.append(name)
    return unrestored


def _remove_set_aside(old: Path) -> None:
    """Remove an aside; one still running (Windows) is left for the next install. (``unlink``,
    not ``remove``: tests/test_config_writes_stay_in_the_cli.py links calls by name, and a
    ``remove`` of the package's writes config.)"""
    with contextlib.suppress(OSError):
        os.unlink(old)


def _sweep_set_aside(bin_dir: Path) -> None:
    """Remove what earlier installs set aside and could not remove then."""
    for old in bin_dir.glob(".rlxd*.old"):
        _remove_set_aside(old)


__all__ = [
    "BINARIES",
    "RELEASES_URL",
    "RELEASES_URL_ENV",
    "SUMS",
    "TARGETS",
    "InstallError",
    "Installed",
    "archive_name",
    "asset_url",
    "binary_names",
    "check_redirect",
    "check_releases_url",
    "host_target",
    "https_redirects_only",
    "install",
    "parse_sums",
    "read_archive",
    "releases_url",
    "uv_tool_bin_dir",
]
