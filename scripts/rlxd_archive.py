"""Package rlxd's release archives and write rlxd-SHA256SUMS (handoff 0.10.3, 1.2).

    python scripts/rlxd_archive.py pack --target T --bin-dir DIR --out OUT
    python scripts/rlxd_archive.py sums --dir OUT

``pack`` writes ``rlxd-<target>.tar.gz`` (``.zip`` for the Windows target) into ``OUT``,
holding exactly ``rlxd`` and ``rlxd-hook`` (``rlxd.exe`` and ``rlxd-hook.exe``) taken from
``DIR``, flat with no directory. ``sums`` writes ``OUT/rlxd-SHA256SUMS``: one
``<sha256>  <name>`` line per archive in ``OUT``, the format ``shasum -a 256 -c`` reads.

The archives are reproducible: every entry is mode 0755, owned by 0:0, and stamped
``SOURCE_DATE_EPOCH`` (the commit's time in CI; 0 when unset), and the gzip header carries
no name and no time. So two builds of one commit with the same binaries sum the same.

Standard library only: .github/workflows/rlxd-release.yml runs it on every target's runner
before anything is installed, and tests/test_rlxd_install.py builds its fixture archives
with it, so the archives the tests install are the ones CI makes. The names here are
``services/rlxd_install.py``'s; tests/test_rlxd_install.py holds the two to each other.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import os
import sys
import tarfile
import time
import zipfile
from pathlib import Path

TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
)
"""The five targets rlxd ships for, in the order the handoff lists them."""

BINARIES = ("rlxd", "rlxd-hook")
SUMS = "rlxd-SHA256SUMS"
MODE = 0o755
ZIP_EPOCH = 315532800
"""1980-01-01 00:00:00 UTC: the earliest time a zip entry can carry."""


def windows(target: str) -> bool:
    """Whether ``target`` is a Windows one: ``.exe`` names in a ``.zip``."""
    return target.endswith("-windows-msvc")


def binary_names(target: str) -> tuple[str, ...]:
    """The two file names the archive for ``target`` holds."""
    suffix = ".exe" if windows(target) else ""
    return tuple(name + suffix for name in BINARIES)


def archive_name(target: str) -> str:
    """``rlxd-<target>.tar.gz``, or ``.zip`` for Windows."""
    return f"rlxd-{target}.zip" if windows(target) else f"rlxd-{target}.tar.gz"


def source_date_epoch() -> int:
    """``SOURCE_DATE_EPOCH`` when set (reproducible-builds.org's variable), else 0."""
    value = os.environ.get("SOURCE_DATE_EPOCH", "").strip()
    return int(value) if value else 0


def pack(target: str, bin_dir: Path, out: Path, *, mtime: int | None = None) -> Path:
    """Write the archive for ``target`` from the two binaries in ``bin_dir`` into ``out``."""
    if target not in TARGETS:
        raise SystemExit(f"unknown target {target!r}; one of: {', '.join(TARGETS)}")
    stamp = source_date_epoch() if mtime is None else mtime
    members = [(name, (bin_dir / name).read_bytes()) for name in binary_names(target)]
    out.mkdir(parents=True, exist_ok=True)
    path = out / archive_name(target)
    path.write_bytes(_zip(members, stamp) if windows(target) else _tar_gz(members, stamp))
    return path


def _tar_gz(members: list[tuple[str, bytes]], mtime: int) -> bytes:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = MODE
            info.mtime = mtime
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            tar.addfile(info, io.BytesIO(data))
    packed = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=packed, mtime=0) as gz:
        gz.write(raw.getvalue())
    return packed.getvalue()


def _zip(members: list[tuple[str, bytes]], mtime: int) -> bytes:
    stamp = time.gmtime(max(mtime, ZIP_EPOCH))[:6]
    packed = io.BytesIO()
    with zipfile.ZipFile(packed, "w") as archive:
        for name, data in members:
            info = zipfile.ZipInfo(name, date_time=stamp)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3  # Unix, so the mode below is read as one
            info.external_attr = (0o100000 | MODE) << 16
            archive.writestr(info, data)
    return packed.getvalue()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def write_sums(directory: Path) -> Path:
    """Write ``rlxd-SHA256SUMS`` in ``directory``, listing every archive there, sorted.

    Refuses a directory holding no archive, or one missing any of the five targets', so a
    build that lost a target cannot publish sums that pass without it."""
    names = sorted(p.name for p in directory.iterdir() if p.name in _ARCHIVES)
    missing = sorted(_ARCHIVES - set(names))
    if missing:
        raise SystemExit(f"{directory}: missing {', '.join(missing)}")
    lines = "".join(f"{sha256(directory / name)}  {name}\n" for name in names)
    path = directory / SUMS
    path.write_bytes(lines.encode("ascii"))
    return path


_ARCHIVES = frozenset(archive_name(t) for t in TARGETS)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    pack_cmd = commands.add_parser("pack", help="write one target's archive")
    pack_cmd.add_argument("--target", required=True, choices=TARGETS)
    pack_cmd.add_argument("--bin-dir", required=True, type=Path)
    pack_cmd.add_argument("--out", required=True, type=Path)
    sums_cmd = commands.add_parser("sums", help=f"write {SUMS} for a directory of archives")
    sums_cmd.add_argument("--dir", required=True, type=Path)
    args = parser.parse_args(argv)
    if args.command == "pack":
        path = pack(args.target, args.bin_dir, args.out)
    else:
        path = write_sums(args.dir)
    print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
