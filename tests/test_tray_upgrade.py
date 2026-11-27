"""An upgrade brings the tray (#34, the ruling on M2): ``rlx upgrade`` and the app's Update of a
PyPI uv-tool install add the ``tray`` extra on macOS and on Linux outside WSL, to every extra its
receipt had, so an install made before the tray (0.10.1) gets it as install.sh's would. Windows,
WSL and an install from a source keep the extras they had.

``install_route._uv_spec`` reuses the receipt's extras, and a 0.10.1 receipt has no ``tray``:
without this the upgraded install showed nothing, and ``rlx service status`` said the extra was
missing.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

from aisquare.services import install_route
from tests.test_lifecycle_upgrade import _facts, _prefix, _receipt, _uv_route

REAL = install_route.tray_extra_here

SERVE = '{ name = "aisquare-cli", extras = ["serve"] }'
PLAIN = '{ name = "aisquare-cli" }'


@pytest.mark.parametrize(
    ("platform", "env", "here"),
    [
        ("darwin", {}, True),
        ("linux", {}, True),
        ("linux", {"WSL_DISTRO_NAME": "Ubuntu"}, False),
        ("linux", {"WSL_INTEROP": "/run/WSL/1_interop"}, False),
        ("win32", {}, False),
    ],
    ids=["macos", "linux", "wsl", "wsl-interop", "windows"],
)
def test_the_tray_is_here_on_macos_and_on_linux_outside_wsl(
    platform: str, env: dict[str, str], here: bool, monkeypatch: pytest.MonkeyPatch
) -> None:
    # /proc/version: a WSL kernel says so where the variables were not passed; here it says
    # nothing of WSL, so the variables decide.
    monkeypatch.setattr("aisquare.core.clipboard._PROC_VERSION", Path("/nonexistent/version"))
    assert REAL(platform, env) is here


@pytest.fixture(autouse=True)
def no_wsl_here(monkeypatch: pytest.MonkeyPatch) -> None:
    """Linux here is Linux outside WSL unless a test says otherwise: neither WSL variable, and a
    /proc/version that says nothing of WSL."""
    monkeypatch.setattr("aisquare.core.clipboard._PROC_VERSION", Path("/nonexistent/version"))
    for name in ("WSL_DISTRO_NAME", "WSL_INTEROP"):
        monkeypatch.delenv(name, raising=False)


def _upgrade(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, platform: str, receipt: str, **env: str
) -> list[str]:
    """The upgrade of a uv tool with ``receipt``, on the OS the route was detected on."""
    for name, value in env.items():
        monkeypatch.setenv(name, value)
    route = _uv_route(tmp_path, _receipt(receipt), platform=platform)
    return install_route.upgrade_argv(route, current="0.10.1")


def test_this_process_s_os_and_variables_are_the_default() -> None:
    """The no-argument call reads this process: how the route's own ``platform`` is read."""
    assert REAL() is REAL(sys.platform, os.environ)


@pytest.mark.parametrize("platform", ["darwin", "linux"])
def test_an_upgrade_on_macos_and_linux_adds_the_tray_to_the_extras_it_had(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, platform: str
) -> None:
    assert _upgrade(tmp_path, monkeypatch, platform, PLAIN)[-1] == "aisquare-cli[tray]>=0.10.1"
    assert _upgrade(tmp_path / "s", monkeypatch, platform, SERVE)[-1] == (
        "aisquare-cli[serve,tray]>=0.10.1"
    ), "every extra the receipt had is kept"


@pytest.mark.parametrize(
    ("platform", "env"),
    [("win32", {}), ("linux", {"WSL_DISTRO_NAME": "Ubuntu"})],
    ids=["windows", "wsl"],
)
def test_an_upgrade_on_windows_and_wsl_keeps_the_extras_it_had(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, platform: str, env: dict[str, str]
) -> None:
    assert _upgrade(tmp_path, monkeypatch, platform, PLAIN, **env)[-1] == "aisquare-cli>=0.10.1"
    assert _upgrade(tmp_path / "s", monkeypatch, platform, SERVE, **env)[-1] == (
        "aisquare-cli[serve]>=0.10.1"
    )


def test_the_route_s_os_decides_not_this_process_s(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Every choice of the route reads the OS it was detected on (reviewer #2902's nit)."""
    monkeypatch.setattr(
        install_route,
        "tray_extra_here",
        lambda platform=None, env=None: (
            REAL(platform, {}) if platform is not None else pytest.fail("read this process's OS")
        ),
    )
    windows = _uv_route(tmp_path, _receipt(PLAIN), platform="win32")
    mac = _uv_route(tmp_path / "m", _receipt(PLAIN), platform="darwin")
    assert install_route.upgrade_argv(windows, current="0.10.1")[-1] == "aisquare-cli>=0.10.1"
    assert install_route.upgrade_argv(mac, current="0.10.1")[-1] == "aisquare-cli[tray]>=0.10.1"


def test_a_pinned_upgrade_gets_the_tray_too_and_an_install_with_it_keeps_one(
    tmp_path: Path,
) -> None:
    route = _uv_route(tmp_path, _receipt(PLAIN), platform="darwin")
    assert install_route.upgrade_argv(route, "0.10.2", current="0.10.1")[-1] == (
        "aisquare-cli[tray]@0.10.2"
    )
    had = _uv_route(
        tmp_path / "t", _receipt('{ name = "aisquare-cli", extras = ["tray"] }'), platform="darwin"
    )
    assert install_route.upgrade_argv(had, current="0.10.1")[-1] == "aisquare-cli[tray]>=0.10.1"


@pytest.mark.parametrize(
    ("requirement", "tail"),
    [
        (
            '{ name = "aisquare-cli", extras = ["serve"], directory = "/src/aisquare-cli" }',
            "/src/aisquare-cli[serve]",
        ),
        ('{ name = "aisquare-cli", editable = "/src/aisquare-cli" }', "/src/aisquare-cli"),
        (
            '{ name = "aisquare-cli", git = "https://github.com/o/r?rev=v0.7.0" }',
            "aisquare-cli @ git+https://github.com/o/r",
        ),
    ],
    ids=["directory", "editable", "git"],
)
def test_an_install_from_a_source_keeps_its_own_extras(
    tmp_path: Path, requirement: str, tail: str
) -> None:
    """A checkout, a directory or git, on macOS, where the tray is: what is installed is the
    source's, and a developer's extras are theirs to choose."""
    route = _uv_route(tmp_path, _receipt(requirement), platform="darwin")
    argv = install_route.upgrade_argv(route, current="0.10.1")
    assert argv[-1] == tail and not any("tray" in part for part in argv), argv


@pytest.mark.parametrize(
    "options",
    [
        'index-url = "https://mirror.example/simple"',
        'index = [{ url = "https://mirror.example/simple", explicit = false, default = true, '
        'format = "simple", authenticate = "auto" }]',
        'no-index = true\nfind-links = ["/wheels"]',
    ],
    ids=["index-url", "default-index", "offline"],
)
def test_an_install_from_an_index_of_its_own_keeps_its_extras(tmp_path: Path, options: str) -> None:
    """A mirror, or a folder of wheels with no index: the tray's packages may not be there, and
    the upgrade would fail to resolve (reviewer #2902's nit: PyPI alone gets the tray)."""
    tail = f"\n[tool.options]\n{options}\n"
    route = _uv_route(tmp_path, _receipt(SERVE, tail=tail), platform="darwin")
    argv = install_route.upgrade_argv(route, current="0.10.1")
    assert argv[-1] == "aisquare-cli[serve]>=0.10.1", argv
    extra = _uv_route(
        tmp_path / "x",
        _receipt(SERVE, tail='\n[tool.options]\nextra-index-url = ["https://x.example/simple"]\n'),
        platform="darwin",
    )
    assert install_route.upgrade_argv(extra, current="0.10.1")[-1] == (
        "aisquare-cli[serve,tray]>=0.10.1"
    ), "an extra index beside PyPI still resolves from PyPI"


@pytest.mark.parametrize("kind", ["pipx", "venv", "system"])
def test_the_routes_that_are_not_a_uv_tool_never_name_the_tray(tmp_path: Path, kind: str) -> None:
    """pipx, pip in a venv or outside one: told their own command, unchanged, on macOS too
    (reviewer #2902's m-a: a control with the tray on)."""
    prefix = _prefix(tmp_path)
    if kind == "pipx":
        (prefix / install_route.PIPX_METADATA_NAME).write_text("{}", encoding="utf-8")
    facts = _facts(prefix, platform="darwin")
    if kind == "system":
        facts = _facts(prefix, platform="darwin", base_prefix=prefix)
    route = install_route.classify(facts)
    assert route.kind == {"pipx": install_route.PIPX, "venv": install_route.VENV}.get(
        kind, install_route.SYSTEM
    )
    for target in (None, "0.10.2"):
        argv = install_route.upgrade_argv(route, target, current="0.10.1")
        assert argv and not any("tray" in part for part in argv), argv


def test_a_uvx_run_is_told_an_install_without_the_tray(tmp_path: Path) -> None:
    route = install_route.InstallRoute(install_route.UVX, _facts(tmp_path, platform="darwin"))
    assert install_route.upgrade_argv(route) == ["uv", "tool", "install", "aisquare-cli"]
