"""rlxd's release binaries are built without any test or dev feature, and checked for it.

The hub's decision in handoff rev 7 (#577): gate dev mode becomes a cargo feature that
release builds never enable (#24 makes it one). So rlxd-release.yml's ``build`` job names
its features (``RELEASE_FEATURES``, none) instead of taking defaults or ``--all-features``,
and refuses a list that names a test or dev feature. ``package`` then checks every binary's
``--version``, as its target's runner gave it, for the dev build's marker: ``(dev:``, as
#24's contract documents it (for example ``rlxd 0.10.3 (dev: gate-dev-mode)``). Any answer
but exactly ``<binary> <version>`` fails too, so another spelling of the marker can't pass.

Both steps are RUN here under the runner's ``bash -eo pipefail``: the build step against a
stand-in ``cargo`` that notes its arguments, the check against answers written as the
build job writes them.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any

import pytest

from tests.ci_matrix import ROOT, workflow

RELEASE = workflow("rlxd-release.yml")
BUILD_STEP = "cargo build --release --target ${{ matrix.target }}"
DEV_CHECK = "No binary is a dev build (rlxd --version has no dev marker)"
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
)
#: rlxd's features that are for tests or development, by the crates' own comments, and
#: gate dev mode's (#24). None may be in a release.
NOT_FOR_RELEASE = ("debug-methods", "gate-test-requirement", "gate-dev-mode")

_RUNS_BASH = pytest.mark.skipif(
    sys.platform == "win32" or shutil.which("bash") is None,
    reason="SKIPPED, NOT RUN: runs the step's bash, as on ubuntu-latest",
)


def _step(job: str, name: str) -> dict[str, Any]:
    found = [step for step in RELEASE["jobs"][job]["steps"] if step.get("name") == name]
    assert len(found) == 1, f"{job} has {len(found)} steps named {name!r}, wanted one"
    step: dict[str, Any] = found[0]
    return step


def _run(step: dict[str, Any], cwd: Path, **env: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", str(step["run"])],
        cwd=cwd,
        env={**os.environ, **env},
        capture_output=True,
        text=True,
    )


# --- the build names its features --------------------------------------------------------


def test_the_release_feature_list_names_no_feature_of_rlxd() -> None:
    """Every feature the crates declare is for tests or development; the list is empty."""
    listed = str(RELEASE["jobs"]["build"]["env"]["RELEASE_FEATURES"])
    assert listed == ""
    for manifest in sorted((ROOT / "rlxd" / "crates").glob("*/Cargo.toml")):
        features = tomllib.loads(manifest.read_text(encoding="utf-8")).get("features", {})
        assert "default" not in features, f"{manifest}: a default feature must be listed"
        for feature in features:
            assert feature not in listed.split(","), f"{feature} is in the release list"


def test_no_release_build_takes_all_features_or_defaults() -> None:
    scripts = [
        str(step.get("run", "")) for job in RELEASE["jobs"].values() for step in job["steps"]
    ]
    assert not [script for script in scripts if "--all-features" in script]
    script = str(_step("build", BUILD_STEP)["run"])
    assert 'features=(--no-default-features --features "$RELEASE_FEATURES")' in script
    builds = [line for line in script.splitlines() if "cargo build" in line]
    assert len(builds) == 2, builds  # the runner's, and the Ubuntu 22.04 image's
    assert script.count('"${features[@]}"') == 2


@pytest.fixture
def stand_in_cargo(tmp_path: Path) -> tuple[Path, Path]:
    """A ``cargo`` on PATH that notes its arguments, one per line, and succeeds."""
    bin_dir, noted = tmp_path / "bin", tmp_path / "cargo-args"
    bin_dir.mkdir()
    cargo = bin_dir / "cargo"
    cargo.write_text(f'#!/bin/sh\nfor a in "$@"; do printf "%s\\n" "$a"; done >> "{noted}"\n')
    cargo.chmod(0o755)
    return bin_dir, noted


@_RUNS_BASH
def test_the_build_passes_the_list_and_nothing_else(
    tmp_path: Path, stand_in_cargo: tuple[Path, Path]
) -> None:
    bin_dir, noted = stand_in_cargo
    done = _run(
        _step("build", BUILD_STEP),
        tmp_path,
        RUNNER_OS="macOS",
        TARGET="aarch64-apple-darwin",
        RELEASE_FEATURES="",
        PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
    )
    assert done.returncode == 0, done.stdout + done.stderr
    args = noted.read_text().splitlines()
    assert args[:5] == ["build", "--release", "--locked", "--target", "aarch64-apple-darwin"]
    assert args[-3:] == ["--no-default-features", "--features", ""]
    assert not any(a.startswith("--all-features") for a in args)


@_RUNS_BASH
@pytest.mark.parametrize(
    "listed", ["gate-dev-mode", "debug-methods", "gate-test-requirement", "x,gate-dev-mode"]
)
def test_the_build_refuses_a_test_or_dev_feature(
    tmp_path: Path, stand_in_cargo: tuple[Path, Path], listed: str
) -> None:
    bin_dir, noted = stand_in_cargo
    done = _run(
        _step("build", BUILD_STEP),
        tmp_path,
        RUNNER_OS="macOS",
        TARGET="aarch64-apple-darwin",
        RELEASE_FEATURES=listed,
        PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
    )
    assert done.returncode == 1, done.stdout + done.stderr
    assert f"::error::release_features: '{listed}' names a test or dev feature" in done.stdout
    assert not noted.exists(), "cargo ran"


# --- package checks every binary's --version ---------------------------------------------


def _answers(root: Path, version: str, *, odd: dict[tuple[str, str], str] | None = None) -> Path:
    """``versions/`` as ``package`` downloads it, with the build job's lines (tab-separated
    binary and answer) for every target; ``odd`` replaces some answers."""
    (root / "pyproject.toml").write_text(f'[project]\nname = "x"\nversion = "{version}"\n')
    versions = root / "versions"
    versions.mkdir()
    for target in TARGETS:
        exe = ".exe" if target.endswith("-windows-msvc") else ""
        lines = []
        for name in ("rlxd", "rlxd-hook"):
            answer = (odd or {}).get((target, name), f"{name} {version}")
            lines.append(f"{name}{exe}\t{answer}\n")
        (versions / f"rlxd-version-{target}.txt").write_text("".join(lines))
    return versions


def _check(root: Path) -> subprocess.CompletedProcess[str]:
    marker = str(RELEASE["jobs"]["package"]["env"]["DEV_MARKER"])
    return _run(_step("package", DEV_CHECK), root, DEV_MARKER=marker)


def test_the_marker_is_the_one_24_documents() -> None:
    assert RELEASE["jobs"]["package"]["env"]["DEV_MARKER"] == "(dev:"


@_RUNS_BASH
def test_ten_clean_answers_pass(tmp_path: Path) -> None:
    _answers(tmp_path, "9.9.9")
    done = _check(tmp_path)
    assert done.returncode == 0, done.stdout + done.stderr
    assert "x86_64-pc-windows-msvc rlxd.exe: rlxd 9.9.9" in done.stdout


@_RUNS_BASH
@pytest.mark.parametrize("target", ["aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc"])
def test_a_dev_build_fails_the_job(tmp_path: Path, target: str) -> None:
    _answers(tmp_path, "9.9.9", odd={(target, "rlxd"): "rlxd 9.9.9 (dev: gate-dev-mode)"})
    done = _check(tmp_path)
    assert done.returncode == 1, done.stdout + done.stderr
    assert f"::error::dev_build: {target}'s rlxd" in done.stdout


@_RUNS_BASH
@pytest.mark.parametrize("answer", ["rlxd 9.9.9 [dev]", "rlxd 9.9.8", "rlxd 9.9.9-dirty"])
def test_any_other_answer_fails_the_job(tmp_path: Path, answer: str) -> None:
    """Another spelling of the marker, or another version, fails all the same."""
    _answers(tmp_path, "9.9.9", odd={("x86_64-apple-darwin", "rlxd"): answer})
    done = _check(tmp_path)
    assert done.returncode == 1, done.stdout + done.stderr
    assert "::error::version_mismatch: x86_64-apple-darwin's rlxd" in done.stdout


@_RUNS_BASH
def test_a_target_whose_answers_are_missing_fails_the_job(tmp_path: Path) -> None:
    versions = _answers(tmp_path, "9.9.9")
    (versions / "rlxd-version-aarch64-apple-darwin.txt").unlink()
    done = _check(tmp_path)
    assert done.returncode == 1, done.stdout + done.stderr
    assert "expected the answers of 10 binaries (5 targets), got 8" in done.stdout


def test_each_build_records_its_answers_for_package() -> None:
    """The build job writes each binary's answer and uploads them; package reads them all."""
    version_step = _step("build", "Each binary is its target's, and answers rlx's version")
    assert 'printf \'%s\\t%s\\n\' "$b$exe" "$got" >> "$answers"' in str(version_step["run"])
    uploads = [
        step["with"]["name"]
        for step in RELEASE["jobs"]["build"]["steps"]
        if str(step.get("uses", "")).startswith("actions/upload-artifact@")
    ]
    assert "rlxd-version-${{ matrix.target }}" in uploads
    downloads = [
        step["with"].get("pattern")
        for step in RELEASE["jobs"]["package"]["steps"]
        if str(step.get("uses", "")).startswith("actions/download-artifact@")
    ]
    assert downloads == ["rlxd-archive-*", "rlxd-version-*"]
