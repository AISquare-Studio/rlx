"""rlxd and rlxd-hook from the release that matches this rlx (handoff 0.10.3, 1.2).

rlxd's version is rlx's, and it lives in uv's tool bin directory beside ``rlx``. The
installers put it there and ``rlx upgrade`` replaces it; someone who installed with plain
``uv tool install`` runs ``rlx daemon install``, which is :func:`install`:

1. It names this machine's target (:func:`host_target`) and its archive,
   ``rlxd-<target>.tar.gz`` or ``rlxd-x86_64-pc-windows-msvc.zip``.
2. It fetches ``rlxd-SHA256SUMS`` of release ``v<version>``, then the archive, from
   ``<releases>/v<version>/<name>``. ``<releases>`` is GitHub's download root for the repo,
   or ``AISQUARE_RELEASES_URL`` when set: the stub releases server of the handoff's
   pre-release checks.
3. It checks the archive's sha256 against its line in the sums BEFORE anything is
   written, and the archive's members: exactly ``rlxd`` and ``rlxd-hook`` (``.exe`` on
   Windows), regular files, no directory.
4. It writes each binary to a temporary file in the bin directory, sets the exec bit
   (not on Windows), and renames it over the old one, so a binary is never half written.
   rlxd-hook is renamed first, so an ``rlxd`` there always has its hook beside it.

Every refusal is an :class:`InstallError` with a code: ``release_not_found`` (no sums at
that version), ``archive_not_listed``, ``archive_not_found``, ``checksum_mismatch``,
``bad_archive``, ``download_failed``, ``unsupported_platform`` and ``install_failed``.
All but the last install nothing. ``install_failed`` (the bin directory cannot be written,
or on Windows a running ``rlxd.exe`` cannot be replaced) leaves the old rlxd in place, beside
either hook.

The bin directory is decided as ``uv tool dir --bin`` decides it (:func:`uv_tool_bin_dir`),
without running uv. ``RLX_HOME`` and ``AISQUARE_HOME`` play no part.

The network is reached through :func:`open_release_url` alone, so a test can replace it; the
standard-library modules are imported inside, off the path every command pays to start.
scripts/rlxd_archive.py makes the archives and the sums, by the same names as here.
"""

from __future__ import annotations

import hashlib
import io
import os
import platform as platform_module
import re
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

    def to_json(self) -> dict[str, Any]:
        return {
            "installed": True,
            "version": self.version,
            "target": self.target,
            "archive": self.archive,
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

    The first that is set to an absolute path: ``UV_TOOL_BIN_DIR``, ``XDG_BIN_HOME``,
    ``XDG_DATA_HOME/../bin``; else ``~/.local/bin``. uv ignores a relative value, so this
    does too. install.sh's ``uv_bin_dir`` follows the same order."""
    env = os.environ if env is None else env
    for name in ("UV_TOOL_BIN_DIR", "XDG_BIN_HOME"):
        value = env.get(name, "")
        if value and os.path.isabs(value):
            return Path(value)
    data = env.get("XDG_DATA_HOME", "")
    if data and os.path.isabs(data):
        return Path(data) / "../bin"
    return (Path.home() if home is None else home) / ".local" / "bin"


def releases_url(env: Mapping[str, str] | None = None) -> str:
    env = os.environ if env is None else env
    return (env.get(RELEASES_URL_ENV) or RELEASES_URL).rstrip("/")


def asset_url(base: str, version: str, name: str) -> str:
    return f"{base}/v{version}/{name}"


# --- the network --------------------------------------------------------------------------


def open_release_url(request: Any, *, timeout: float) -> Any:
    """``urllib.request.urlopen`` through ``core.tls``: the one network call here."""
    from aisquare.core.tls import urlopen

    return urlopen(request, timeout=timeout)


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
    base = releases_url() if base is None else base.rstrip("/")
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
    paths = _place(files, bin_dir, executable=not is_windows_target(target))
    return Installed(version, target, name, url, actual, bin_dir, paths)


def _place(files: Mapping[str, bytes], bin_dir: Path, *, executable: bool) -> tuple[Path, ...]:
    """Write every file to a temporary file in ``bin_dir``, then rename each over its name:
    rlxd-hook first, rlxd last. A temporary file left by a failure is removed."""
    order = sorted(files, key=lambda n: n.startswith("rlxd-hook"), reverse=True)
    staged: list[tuple[Path, Path]] = []
    try:
        bin_dir.mkdir(parents=True, exist_ok=True)
        for name in order:
            fd, temp_name = tempfile.mkstemp(prefix=f".{name}.", suffix=".tmp", dir=bin_dir)
            staged.append((Path(temp_name), bin_dir / name))
            with os.fdopen(fd, "wb") as handle:
                handle.write(files[name])
                handle.flush()
                os.fsync(handle.fileno())
            if executable:
                os.chmod(temp_name, 0o755)
        for staged_file, final in staged:
            os.replace(staged_file, final)
    except OSError as exc:
        raise InstallError(
            "install_failed",
            f"could not install into {bin_dir}: {exc}"
            + ("; if rlxd is running, stop it and run this again" if not executable else ""),
            detail=repr(exc),
        ) from exc
    finally:
        for temp, _final in staged:
            temp.unlink(missing_ok=True)
    return tuple(bin_dir / name for name in sorted(files))


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
    "host_target",
    "install",
    "parse_sums",
    "read_archive",
    "releases_url",
    "uv_tool_bin_dir",
]
