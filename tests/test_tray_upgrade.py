"""An upgrade brings the tray (#34, the ruling on M2): ``rlx upgrade`` and the app's Update of a
PyPI uv-tool install add the ``tray`` extra on macOS and on Linux outside WSL, to every extra its
receipt had, so an install made before the tray (0.10.1) gets it as install.sh's would. Windows,
WSL and an install from a source keep the extras they had.

``install_route._uv_spec`` reuses the receipt's extras, and a 0.10.1 receipt has no ``tray``:
without this the upgraded install showed nothing, and ``rlx service status`` said the extra was
missing.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from aisquare.services import install_route
from tests.test_lifecycle_upgrade import _receipt, _uv_route

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


def _upgrade(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, platform: str, receipt: str, **env: str
) -> list[str]:
    monkeypatch.setattr("aisquare.core.clipboard._PROC_VERSION", Path("/nonexistent/version"))
    monkeypatch.setattr(install_route, "tray_extra_here", lambda: REAL(platform, env))
    route = _uv_route(tmp_path, _receipt(receipt))
    return install_route.upgrade_argv(route, current="0.10.1")


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


def test_a_pinned_upgrade_gets_the_tray_too_and_an_install_with_it_keeps_one(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(install_route, "tray_extra_here", lambda: True)
    route = _uv_route(tmp_path, _receipt(PLAIN))
    assert install_route.upgrade_argv(route, "0.10.2", current="0.10.1")[-1] == (
        "aisquare-cli[tray]@0.10.2"
    )
    had = _uv_route(tmp_path / "t", _receipt('{ name = "aisquare-cli", extras = ["tray"] }'))
    assert install_route.upgrade_argv(had, current="0.10.1")[-1] == "aisquare-cli[tray]>=0.10.1"


def test_an_install_from_a_source_keeps_its_own_extras(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A checkout, a directory or git: what is installed is the source's, and a developer's
    extras are theirs to choose; only a PyPI install is given the tray."""
    monkeypatch.setattr(install_route, "tray_extra_here", lambda: True)
    route = _uv_route(
        tmp_path,
        _receipt('{ name = "aisquare-cli", extras = ["serve"], directory = "/src/aisquare-cli" }'),
    )
    argv = install_route.upgrade_argv(route, current="0.10.1")
    assert argv[-1] == "/src/aisquare-cli[serve]", argv
