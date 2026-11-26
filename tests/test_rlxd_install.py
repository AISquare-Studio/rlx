"""``rlx daemon install`` against a stub releases server (handoff 0.10.3, 1.2).

The release is laid out as GitHub serves one, ``v<version>/<asset>``, by
tests/stub_releases.py, and its archives and ``rlxd-SHA256SUMS`` are made by
scripts/rlxd_archive.py, the packer CI runs, from stand-in binaries. ``uv tool dir --bin`` is
pointed at a temp folder with ``UV_TOOL_BIN_DIR``. Runs on macOS, Linux and Windows.
"""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import zipfile
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from pathlib import Path
from types import ModuleType

import pytest
from typer.testing import CliRunner

from aisquare.cli.app import app
from aisquare.core.version import __version__
from aisquare.services import rlxd_install
from tests.stub_releases import serving

REPO = Path(__file__).resolve().parents[1]
WINDOWS = sys.platform == "win32"
HOST = rlxd_install.host_target()
EXE = ".exe" if WINDOWS else ""


def _load_packer() -> ModuleType:
    spec = importlib.util.spec_from_file_location(
        "rlxd_archive", REPO / "scripts" / "rlxd_archive.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


packer = _load_packer()


def _stand_in(name: str, version: str) -> bytes:
    """A binary's stand-in: a script that answers ``--version`` as rlxd does, so on macOS
    and Linux the installed one runs; on Windows only its bytes are compared."""
    return f"#!/bin/sh\necho '{name.removesuffix('.exe')} {version}'\n".encode()


def _publish(root: Path, version: str, *, build: Path) -> Path:
    """Release ``v<version>`` in ``root``: every target's archive and the sums."""
    release = root / f"v{version}"
    for target in packer.TARGETS:
        bins = build / target
        bins.mkdir(parents=True, exist_ok=True)
        for name in packer.binary_names(target):
            (bins / name).write_bytes(_stand_in(name, version))
        packer.pack(target, bins, release, mtime=1_700_000_000)
    packer.write_sums(release)
    return release


@dataclass
class Stub:
    url: str
    root: Path
    release: Path
    log: Path
    bin_dir: Path

    def requested(self) -> list[str]:
        return self.log.read_text(encoding="utf-8").splitlines() if self.log.exists() else []

    def bin_entries(self) -> list[str]:
        return sorted(p.name for p in self.bin_dir.iterdir()) if self.bin_dir.exists() else []


@pytest.fixture
def stub(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Stub]:
    """This version released on a stub releases server, ``AISQUARE_RELEASES_URL`` naming it
    and ``UV_TOOL_BIN_DIR`` a fresh folder."""
    root = tmp_path / "releases"
    release = _publish(root, __version__, build=tmp_path / "build")
    log = tmp_path / "stub-requests.log"
    bin_dir = tmp_path / "uv-bin"
    for name in ("XDG_BIN_HOME", "XDG_DATA_HOME"):
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("UV_TOOL_BIN_DIR", str(bin_dir))
    monkeypatch.setenv("NO_PROXY", "127.0.0.1,localhost")
    monkeypatch.setenv("no_proxy", "127.0.0.1,localhost")
    with serving(root, log) as url:
        monkeypatch.setenv(rlxd_install.RELEASES_URL_ENV, url)
        yield Stub(url, root, release, log, bin_dir)


def _daemon_install(*args: str) -> tuple[int, str]:
    result = CliRunner().invoke(app, [*args, "daemon", "install"])
    return result.exit_code, result.output


def _json(output: str) -> dict[str, object]:
    data = json.loads(output.strip().splitlines()[-1])
    assert isinstance(data, dict)
    return data


def _executable(path: Path) -> bool:
    return path.is_file() and (WINDOWS or os.access(path, os.X_OK))


# --- the install -------------------------------------------------------------------------


def test_rm_rlxd_then_daemon_install_leaves_an_executable_rlxd(stub: Stub) -> None:
    """The handoff's line: ``rm "$(uv tool dir --bin)/rlxd" && rlx daemon install &&
    test -x "$(uv tool dir --bin)/rlxd"``."""
    code, output = _daemon_install()
    assert code == 0, output
    rlxd = stub.bin_dir / f"rlxd{EXE}"
    rlxd.unlink()

    code, output = _daemon_install()

    assert code == 0, output
    assert _executable(rlxd)
    assert _executable(stub.bin_dir / f"rlxd-hook{EXE}")
    assert stub.bin_entries() == sorted([f"rlxd{EXE}", f"rlxd-hook{EXE}"])  # no temp file left
    assert f"rlxd {__version__} installed in {stub.bin_dir}" in output
    archive = rlxd_install.archive_name(HOST)
    assert stub.requested()[-2:] == [
        f"/v{__version__}/rlxd-SHA256SUMS",
        f"/v{__version__}/{archive}",
    ]


@pytest.mark.skipif(WINDOWS, reason="the stand-in is a shell script; Windows compares bytes")
def test_the_installed_rlxd_answers_its_version(stub: Stub) -> None:
    """``test "$(rlxd --version)" = "rlxd $V"`` after an install, $V being rlx's version."""
    assert _daemon_install()[0] == 0
    for name in ("rlxd", "rlxd-hook"):
        answer = subprocess.run(
            [str(stub.bin_dir / name), "--version"], capture_output=True, text=True, check=True
        )
        assert answer.stdout.strip() == f"{name} {__version__}"


def test_the_installed_bytes_are_the_archives(stub: Stub) -> None:
    assert _daemon_install()[0] == 0
    for name in rlxd_install.binary_names(HOST):
        assert (stub.bin_dir / name).read_bytes() == _stand_in(name, __version__)


def test_json_names_what_was_installed_and_where(stub: Stub) -> None:
    code, output = _daemon_install("--json")
    assert code == 0, output
    data = _json(output)
    archive = rlxd_install.archive_name(HOST)
    sums = rlxd_install.parse_sums((stub.release / "rlxd-SHA256SUMS").read_text())
    assert data == {
        "installed": True,
        "version": __version__,
        "target": HOST,
        "archive": archive,
        "url": f"{stub.url}/v{__version__}/{archive}",
        "sha256": sums[archive],
        "bin_dir": str(stub.bin_dir),
        "binaries": [str(stub.bin_dir / n) for n in sorted(rlxd_install.binary_names(HOST))],
    }


def test_an_install_replaces_an_older_rlxd(stub: Stub) -> None:
    stub.bin_dir.mkdir()
    (stub.bin_dir / f"rlxd{EXE}").write_bytes(b"old")
    assert _daemon_install()[0] == 0
    assert (stub.bin_dir / f"rlxd{EXE}").read_bytes() == _stand_in(f"rlxd{EXE}", __version__)


def test_bin_dir_option_installs_there(stub: Stub, tmp_path: Path) -> None:
    there = tmp_path / "elsewhere"
    result = CliRunner().invoke(app, ["daemon", "install", "--bin-dir", str(there)])
    assert result.exit_code == 0, result.output
    assert _executable(there / f"rlxd{EXE}")
    assert not stub.bin_dir.exists()


def test_the_windows_archive_installs_exe_names(stub: Stub, tmp_path: Path) -> None:
    """The zip path, on every OS: ``rlxd.exe`` and ``rlxd-hook.exe``, no exec bit set."""
    target = "x86_64-pc-windows-msvc"
    done = rlxd_install.install(target=target, bin_dir=tmp_path / "win")
    assert [p.name for p in done.binaries] == ["rlxd-hook.exe", "rlxd.exe"]
    assert done.archive == "rlxd-x86_64-pc-windows-msvc.zip"
    for path in done.binaries:
        assert path.read_bytes() == _stand_in(path.name, __version__)
        if not WINDOWS:
            assert not os.access(path, os.X_OK)


# --- refusals: each named, each installing nothing ---------------------------------------


def _refused(stub: Stub, code: str) -> dict[str, object]:
    exit_code, output = _daemon_install("--json")
    assert exit_code == 1, output
    data = _json(output)
    assert data["error"] == code, data
    assert stub.bin_entries() == [], "a refusal installed something"
    return data


def test_a_tampered_archive_is_refused_with_checksum_mismatch(stub: Stub) -> None:
    archive = stub.release / rlxd_install.archive_name(HOST)
    archive.write_bytes(archive.read_bytes() + b"\0")
    data = _refused(stub, "checksum_mismatch")
    assert hashlib.sha256(archive.read_bytes()).hexdigest() in str(data["detail"])


def test_a_tampered_archive_leaves_the_old_rlxd_alone(stub: Stub) -> None:
    stub.bin_dir.mkdir()
    old = stub.bin_dir / f"rlxd{EXE}"
    old.write_bytes(b"old")
    archive = stub.release / rlxd_install.archive_name(HOST)
    archive.write_bytes(archive.read_bytes()[:-1] + b"\1")
    exit_code, output = _daemon_install()
    assert exit_code == 1
    assert "does not match rlxd-SHA256SUMS" in output
    assert "nothing installed" in output
    assert sorted(p.name for p in stub.bin_dir.iterdir()) == [f"rlxd{EXE}"]
    assert old.read_bytes() == b"old"


def test_a_version_with_no_release_is_refused_with_release_not_found(stub: Stub) -> None:
    shutil.move(stub.release, stub.root / "v0.0.1")
    data = _refused(stub, "release_not_found")
    assert stub.requested() == [f"/v{__version__}/rlxd-SHA256SUMS"]
    assert f"/v{__version__}/rlxd-SHA256SUMS" in str(data["detail"])


def test_a_version_with_no_release_says_so_to_a_person(stub: Stub) -> None:
    shutil.rmtree(stub.release)
    exit_code, output = _daemon_install()
    assert exit_code == 1
    assert f"there is no rlxd release v{__version__}" in output


def test_sums_that_do_not_list_this_archive_are_refused(stub: Stub) -> None:
    sums = stub.release / "rlxd-SHA256SUMS"
    archive = rlxd_install.archive_name(HOST)
    kept = [line for line in sums.read_text().splitlines() if not line.endswith(f" {archive}")]
    sums.write_text("\n".join(kept) + "\n")
    _refused(stub, "archive_not_listed")
    assert stub.requested() == [f"/v{__version__}/rlxd-SHA256SUMS"]  # never fetched


def test_a_listed_archive_that_is_missing_is_refused(stub: Stub) -> None:
    archive = rlxd_install.archive_name(HOST)
    (stub.release / archive).unlink()
    data = _refused(stub, "archive_not_found")
    assert str(data["detail"]).endswith(f"/v{__version__}/{archive}")


def _resum(stub: Stub, target: str, data: bytes) -> None:
    """Replace ``target``'s archive with ``data`` and sum it, so only its shape is wrong."""
    archive = rlxd_install.archive_name(target)
    (stub.release / archive).write_bytes(data)
    packer.write_sums(stub.release)


def _tar(entries: dict[str, bytes]) -> bytes:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w:gz") as tar:
        for name, data in entries.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = 0o755
            tar.addfile(info, io.BytesIO(data))
    return raw.getvalue()


def _zip(entries: dict[str, bytes]) -> bytes:
    raw = io.BytesIO()
    with zipfile.ZipFile(raw, "w") as archive:
        for name, data in entries.items():
            archive.writestr(name, data)
    return raw.getvalue()


def _archive(target: str, entries: dict[str, bytes]) -> bytes:
    return _zip(entries) if rlxd_install.is_windows_target(target) else _tar(entries)


@pytest.mark.parametrize(
    "shape",
    [
        pytest.param(lambda r, h: {r: b"x", h: b"x", "README": b"x"}, id="a third file"),
        pytest.param(lambda r, h: {r: b"x"}, id="no hook"),
        pytest.param(lambda r, h: {f"rlxd/{r}": b"x", f"rlxd/{h}": b"x"}, id="in a folder"),
        pytest.param(lambda r, h: {f"./{r}": b"x", f"./{h}": b"x"}, id="dot-slash names"),
    ],
)
def test_an_archive_that_is_not_exactly_the_two_binaries_is_refused(
    stub: Stub, shape: Callable[[str, str], dict[str, bytes]]
) -> None:
    rlxd, hook = rlxd_install.binary_names(HOST)
    _resum(stub, HOST, _archive(HOST, shape(rlxd, hook)))
    assert _refused(stub, "bad_archive")["error"] == "bad_archive"


def test_a_link_in_place_of_a_binary_is_refused() -> None:
    target = "x86_64-unknown-linux-gnu"
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w:gz") as tar:
        tar.addfile(tarfile.TarInfo("rlxd-hook"), io.BytesIO(b""))
        link = tarfile.TarInfo("rlxd")
        link.type = tarfile.SYMTYPE
        link.linkname = "/bin/sh"
        tar.addfile(link)
    with pytest.raises(rlxd_install.InstallError) as refused:
        rlxd_install.read_archive(target, raw.getvalue())
    assert refused.value.code == "bad_archive"


def test_garbage_that_matches_its_sum_is_refused_as_a_bad_archive(stub: Stub) -> None:
    _resum(stub, HOST, b"not an archive")
    assert _refused(stub, "bad_archive")["error"] == "bad_archive"
    assert stub.requested()[-1] == f"/v{__version__}/{rlxd_install.archive_name(HOST)}"


def test_an_unreachable_server_is_download_failed(
    stub: Stub, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv(rlxd_install.RELEASES_URL_ENV, "http://127.0.0.1:9")
    assert _refused(stub, "download_failed")["detail"]
    assert stub.requested() == []  # nothing reached the stub: the request went to port 9


def test_an_unsupported_machine_is_refused_by_name() -> None:
    with pytest.raises(rlxd_install.InstallError) as refused:
        rlxd_install.host_target("freebsd14", "amd64")
    assert refused.value.code == "unsupported_platform"
    with pytest.raises(rlxd_install.InstallError):
        rlxd_install.host_target("linux", "riscv64")


# --- the names, the target, the bin directory --------------------------------------------


@pytest.mark.parametrize(
    ("platform", "machine", "target"),
    [
        ("darwin", "arm64", "aarch64-apple-darwin"),
        ("darwin", "x86_64", "x86_64-apple-darwin"),
        ("linux", "x86_64", "x86_64-unknown-linux-gnu"),
        ("linux", "aarch64", "aarch64-unknown-linux-gnu"),
        ("win32", "AMD64", "x86_64-pc-windows-msvc"),
        ("win32", "ARM64", "x86_64-pc-windows-msvc"),
    ],
)
def test_each_machine_gets_its_target(platform: str, machine: str, target: str) -> None:
    assert rlxd_install.host_target(platform, machine) == target


def test_the_packer_and_the_installer_name_the_same_archives() -> None:
    assert packer.TARGETS == rlxd_install.TARGETS
    assert packer.SUMS == rlxd_install.SUMS
    for target in rlxd_install.TARGETS:
        assert packer.archive_name(target) == rlxd_install.archive_name(target)
        assert packer.binary_names(target) == rlxd_install.binary_names(target)


def test_the_five_archives_are_named_as_the_handoff_names_them() -> None:
    assert [rlxd_install.archive_name(t) for t in rlxd_install.TARGETS] == [
        "rlxd-aarch64-apple-darwin.tar.gz",
        "rlxd-x86_64-apple-darwin.tar.gz",
        "rlxd-x86_64-unknown-linux-gnu.tar.gz",
        "rlxd-aarch64-unknown-linux-gnu.tar.gz",
        "rlxd-x86_64-pc-windows-msvc.zip",
    ]


@pytest.mark.parametrize(
    ("env", "expected"),
    [
        ({"UV_TOOL_BIN_DIR": "{a}/u", "XDG_BIN_HOME": "{a}/x", "XDG_DATA_HOME": "{a}/d"}, "{a}/u"),
        ({"XDG_BIN_HOME": "{a}/x", "XDG_DATA_HOME": "{a}/d"}, "{a}/x"),
        ({"XDG_DATA_HOME": "{a}/d"}, "{a}/d/../bin"),
        ({}, "{home}/.local/bin"),
        ({"UV_TOOL_BIN_DIR": "relative", "XDG_BIN_HOME": "{a}/x"}, "{a}/x"),
        (
            {"UV_TOOL_BIN_DIR": "", "XDG_BIN_HOME": "rel", "XDG_DATA_HOME": "rel"},
            "{home}/.local/bin",
        ),
    ],
)
def test_the_bin_dir_is_decided_as_uv_decides_it(
    env: dict[str, str], expected: str, tmp_path: Path
) -> None:
    a, home = str(tmp_path / "abs"), tmp_path / "home"
    resolved = {k: v.format(a=a) for k, v in env.items()}
    assert rlxd_install.uv_tool_bin_dir(resolved, home=home) == Path(
        expected.format(a=a, home=home)
    )


@pytest.mark.skipif(shutil.which("uv") is None, reason="needs uv on PATH")
@pytest.mark.parametrize("variable", ["UV_TOOL_BIN_DIR", "XDG_BIN_HOME", "XDG_DATA_HOME"])
def test_the_bin_dir_is_what_uv_tool_dir_bin_prints(variable: str, tmp_path: Path) -> None:
    """Checked against uv itself: ``uv tool dir --bin`` under the same environment."""
    unset = ("UV_TOOL_BIN_DIR", "XDG_BIN_HOME", "XDG_DATA_HOME")
    env = {k: v for k, v in os.environ.items() if k not in unset}
    env[variable] = str(tmp_path / "chosen")
    uv = shutil.which("uv")
    assert uv is not None
    printed = subprocess.run(
        [uv, "tool", "dir", "--bin"], env=env, capture_output=True, text=True, check=True
    ).stdout.strip()
    assert Path(printed) == rlxd_install.uv_tool_bin_dir(env)


def test_sums_lines_parse_in_both_shasum_modes() -> None:
    a, b = "a" * 64, "B" * 64
    text = f"{a}  rlxd-x.tar.gz\n\n{b} *rlxd-y.zip\nnot a line\n"
    assert rlxd_install.parse_sums(text) == {"rlxd-x.tar.gz": a, "rlxd-y.zip": b.lower()}


# --- the packer -----------------------------------------------------------------------------


def test_each_archive_is_flat_and_holds_exactly_the_two_binaries(tmp_path: Path) -> None:
    release = _publish(tmp_path / "r", "1.2.3", build=tmp_path / "b")
    for target in packer.TARGETS:
        path = release / packer.archive_name(target)
        if rlxd_install.is_windows_target(target):
            with zipfile.ZipFile(path) as archive:
                names = archive.namelist()
                modes = [info.external_attr >> 16 & 0o777 for info in archive.infolist()]
        else:
            with tarfile.open(path, "r:gz") as tar:
                names = tar.getnames()
                modes = [member.mode for member in tar.getmembers()]
        assert sorted(names) == sorted(packer.binary_names(target)), target
        assert modes == [0o755, 0o755], target


def test_the_sums_list_every_archive_in_the_shasum_format(tmp_path: Path) -> None:
    release = _publish(tmp_path / "r", "1.2.3", build=tmp_path / "b")
    lines = (release / "rlxd-SHA256SUMS").read_bytes().decode("ascii").splitlines()
    names = sorted(packer.archive_name(t) for t in packer.TARGETS)
    assert [line.split("  ", 1)[1] for line in lines] == names
    for line in lines:
        digest, name = line.split("  ", 1)
        assert digest == hashlib.sha256((release / name).read_bytes()).hexdigest()
    assert b"\r" not in (release / "rlxd-SHA256SUMS").read_bytes()


def test_the_archives_are_reproducible(tmp_path: Path) -> None:
    one = _publish(tmp_path / "one", "1.2.3", build=tmp_path / "b1")
    two = _publish(tmp_path / "two", "1.2.3", build=tmp_path / "b2")
    assert (one / "rlxd-SHA256SUMS").read_bytes() == (two / "rlxd-SHA256SUMS").read_bytes()


def test_sums_are_refused_while_a_target_is_missing(tmp_path: Path) -> None:
    release = _publish(tmp_path / "r", "1.2.3", build=tmp_path / "b")
    (release / "rlxd-aarch64-unknown-linux-gnu.tar.gz").unlink()
    with pytest.raises(SystemExit, match=r"missing rlxd-aarch64-unknown-linux-gnu\.tar\.gz"):
        packer.write_sums(release)
