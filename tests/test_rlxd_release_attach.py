"""rlxd-release.yml's ``attach`` job attaches only what was built from the release's tag.

Review of #593, major 1: a dispatch from any branch carrying the release's version passed
the version check, and ``gh release upload --clobber`` replaced the release's archives and
its ``rlxd-SHA256SUMS`` with that branch's build. So ``attach`` first requires the run's ref
to be ``refs/tags/$TAG`` and the tag, at origin, to still name the commit the run built.

That step is RUN here, as the runner runs it (``bash --noprofile --norc -eo pipefail``, the
job's ``shell: bash``), in a git clone of a throwaway origin, for a branch, the tag
(lightweight and annotated), a tag moved since, and none. From the tag, the dry run then
says what it would upload. The rest of the review's workflow items are pinned as text: the
build image by digest, ``GH_TOKEN`` on the gh steps alone, and no cancel of a dispatch that
attaches.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest

from tests.ci_matrix import ROOT, workflow

RELEASE = workflow("rlxd-release.yml")
REF_CHECK = "The run is the tag's, and built the tag's commit"
UPLOAD = "Upload to the release (a dry run prints it)"
TAG = "v9.9.9"

_RUNS_BASH = pytest.mark.skipif(
    sys.platform == "win32" or shutil.which("bash") is None or shutil.which("git") is None,
    reason="SKIPPED, NOT RUN: runs the step's bash, as on ubuntu-latest, with git",
)


def _step(job: str, name: str) -> dict[str, Any]:
    found = [step for step in RELEASE["jobs"][job]["steps"] if step.get("name") == name]
    assert len(found) == 1, f"{job} has {len(found)} steps named {name!r}, wanted one"
    step: dict[str, Any] = found[0]
    return step


class Repo:
    """A clone of a throwaway origin, with git kept off the user's own config."""

    def __init__(self, root: Path) -> None:
        self.root = root
        empty = root / "gitconfig"
        empty.write_text("")
        self.env = {
            **os.environ,
            "GIT_CONFIG_GLOBAL": str(empty),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@example.com",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@example.com",
        }
        self.origin = root / "origin.git"
        self.work = root / "work"
        self.git("init", "-q", "--bare", str(self.origin), cwd=root)
        self.git("clone", "-q", str(self.origin), str(self.work), cwd=root)

    def git(self, *args: str, cwd: Path | None = None) -> str:
        done = subprocess.run(
            ["git", *args],
            cwd=cwd or self.work,
            env=self.env,
            capture_output=True,
            text=True,
            check=True,
        )
        return done.stdout.strip()

    def commit(self, message: str) -> str:
        self.git("commit", "-q", "--allow-empty", "-m", message)
        self.git("push", "-q", "origin", "HEAD:refs/heads/main")
        return self.git("rev-parse", "HEAD")

    def tag(self, sha: str, *, annotated: bool = False, force: bool = False) -> None:
        args = ["tag", *(["-a", "-m", TAG] if annotated else []), *(["-f"] if force else [])]
        self.git(*args, TAG, sha)
        self.git("push", "-q", *(["-f"] if force else []), "origin", f"refs/tags/{TAG}")

    def run(self, step: dict[str, Any], **env: str) -> subprocess.CompletedProcess[str]:
        """``step``'s script under the runner's ``shell: bash``, in the clone."""
        return subprocess.run(
            ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", str(step["run"])],
            cwd=self.work / step.get("working-directory", "."),
            env={**self.env, **env},
            capture_output=True,
            text=True,
        )


@pytest.fixture
def repo(tmp_path: Path) -> Repo:
    return Repo(tmp_path)


@_RUNS_BASH
def test_a_dispatch_from_a_branch_is_refused(repo: Repo) -> None:
    """The reviewer's case: a branch at the release's version, dispatched with release_tag."""
    head = repo.commit("a branch at the release's version")
    repo.tag(head)
    done = repo.run(
        _step("attach", REF_CHECK), GITHUB_REF="refs/heads/feat/x", GITHUB_SHA=head, TAG=TAG
    )
    assert done.returncode == 1, done.stdout + done.stderr
    assert "::error::ref_not_tag: this run is for refs/heads/feat/x" in done.stdout
    assert f"--ref {TAG}" in done.stdout


@_RUNS_BASH
@pytest.mark.parametrize("annotated", [False, True], ids=["lightweight", "annotated"])
def test_a_run_on_the_tag_that_built_its_commit_passes(repo: Repo, annotated: bool) -> None:
    head = repo.commit("the release")
    repo.tag(head, annotated=annotated)
    done = repo.run(
        _step("attach", REF_CHECK), GITHUB_REF=f"refs/tags/{TAG}", GITHUB_SHA=head, TAG=TAG
    )
    assert done.returncode == 0, done.stdout + done.stderr
    assert f"{TAG} is {head}, the commit this run built" in done.stdout


@_RUNS_BASH
def test_a_tag_moved_since_the_run_started_is_refused(repo: Repo) -> None:
    built = repo.commit("what the run built")
    repo.tag(built)
    later = repo.commit("pushed after")
    repo.tag(later, force=True)
    repo.git("checkout", "-q", built)
    done = repo.run(
        _step("attach", REF_CHECK), GITHUB_REF=f"refs/tags/{TAG}", GITHUB_SHA=built, TAG=TAG
    )
    assert done.returncode == 1, done.stdout + done.stderr
    assert f"::error::tag_moved: {TAG} names {later} at origin" in done.stdout


@_RUNS_BASH
def test_a_tag_origin_does_not_have_is_refused(repo: Repo) -> None:
    head = repo.commit("never tagged at origin")
    done = repo.run(
        _step("attach", REF_CHECK), GITHUB_REF=f"refs/tags/{TAG}", GITHUB_SHA=head, TAG=TAG
    )
    assert done.returncode == 1, done.stdout + done.stderr
    assert f"::error::tag_moved: {TAG} names nothing at origin" in done.stdout


@_RUNS_BASH
@pytest.mark.parametrize("dry_run", ["true", "", "False"])
def test_from_the_tag_the_dry_run_says_what_it_would_attach(
    repo: Repo, tmp_path: Path, dry_run: str
) -> None:
    """From the tag the ref check passes, and the upload step, as a dry run, prints the
    upload it would run and uploads nothing: anything but an explicit ``false`` is a dry
    run. ``gh`` here is a stand-in that notes any call and fails, so nothing could reach
    the real releases."""
    head = repo.commit("the release")
    repo.tag(head)
    env = {"GITHUB_REF": f"refs/tags/{TAG}", "GITHUB_SHA": head, "TAG": TAG}
    assert repo.run(_step("attach", REF_CHECK), **env).returncode == 0

    archives = repo.work / "r"
    archives.mkdir()
    for name in (
        "rlxd-aarch64-apple-darwin.tar.gz",
        "rlxd-x86_64-pc-windows-msvc.zip",
        "rlxd-SHA256SUMS",
    ):
        (archives / name).write_bytes(b"")
    summary, called = tmp_path / "summary.md", tmp_path / "gh-called"
    stand_ins = tmp_path / "bin"
    stand_ins.mkdir()
    gh = stand_ins / "gh"
    gh.write_text(f'#!/bin/sh\necho "$@" >> "{called}"\nexit 1\n')
    gh.chmod(0o755)
    done = repo.run(
        _step("attach", UPLOAD),
        **env,
        DRY_RUN=dry_run,
        GH_TOKEN="not-a-token",
        GITHUB_REPOSITORY="AISquare-Studio/rlx",
        GITHUB_STEP_SUMMARY=str(summary),
        PATH=f"{stand_ins}{os.pathsep}{os.environ['PATH']}",
    )
    assert done.returncode == 0, done.stdout + done.stderr
    assert not called.exists(), "a dry run ran gh"
    assert f"nothing attached. Would run: gh release upload {TAG} " in done.stdout
    assert "--clobber --repo AISquare-Studio/rlx" in done.stdout
    assert f"## Dry run: nothing attached to {TAG}" in summary.read_text()


def test_the_ref_check_comes_before_anything_is_uploaded() -> None:
    names = [step.get("name") for step in RELEASE["jobs"]["attach"]["steps"]]
    assert names.index(REF_CHECK) < names.index(UPLOAD)
    assert names.index(REF_CHECK) == 1, "the first step after the checkout"


def test_gh_token_is_given_to_the_gh_steps_alone() -> None:
    """contents: write reaches the two steps that run gh, and no other (review, nit 7)."""
    attach = RELEASE["jobs"]["attach"]
    assert "GH_TOKEN" not in attach.get("env", {})
    with_token = [step.get("name") for step in attach["steps"] if "GH_TOKEN" in step.get("env", {})]
    assert with_token == ["The release exists", UPLOAD]
    for step in attach["steps"]:
        if "gh " in str(step.get("run", "")) and "gh workflow run" not in str(step["run"]):
            assert step.get("name") in with_token, f"{step.get('name')} runs gh without it"


def test_a_dispatch_that_attaches_is_never_cancelled() -> None:
    """rlxd.yml's workflow-level group cancels a superseded run, but not a dispatch with a
    release_tag, whose upload may be part-way (review, minor 5); attach's own group doesn't
    cancel either."""
    text = (ROOT / ".github" / "workflows" / "rlxd.yml").read_text(encoding="utf-8")
    assert (
        "cancel-in-progress: ${{ github.event_name != 'workflow_dispatch' "
        "|| inputs.release_tag == '' }}" in text
    )
    assert RELEASE["jobs"]["attach"]["concurrency"]["cancel-in-progress"] is False


def test_the_linux_build_image_is_pinned_by_digest_and_logs_its_packages() -> None:
    """The one third-party input of the release build that is not an action (review, minor 3)."""
    image = _step("build", "The Ubuntu 22.04 build image (glibc ${{ env.GLIBC_OLDEST }})")
    script = str(image["run"])
    assert "FROM ubuntu:22.04@sha256:" in script
    digest = script.split("FROM ubuntu:22.04@sha256:")[1].split()[0]
    assert len(digest) == 64 and all(c in "0123456789abcdef" for c in digest)
    assert "dpkg-query -W" in script
