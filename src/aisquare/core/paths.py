"""rlx's home and its layout: the one file that says where everything under the home lives.

Since 0.10.2 (#30, H1) the home is ``~/.rlx``, laid out as docs/home-layout.md describes::

    ~/.rlx/
    ├── config.toml  state.json  agents.json
    ├── rlx.db                    # the store: projects, groups, fleet rows, boards, tasks, ...
    ├── accounts/claude/<slot>/   # one CLAUDE_CONFIG_DIR per managed account
    ├── accounts/codex/<slot>/    # one CODEX_HOME per managed Codex account
    ├── accounts/aisquare/        # the AISquare sign-in's credentials
    ├── projects/<project-id>/    # a project's own files (its manager's state, its code index)
    ├── fleet/                    # tmux.conf, launch-seals/
    ├── run/                      # instances/, locks/: what lives while an rlx runs
    ├── remote/  watch/  logs/  cache/

A home from before it (``~/.aisquare``) keeps the layout it was made in until H2 migrates it:
``context.db``, ``claude-accounts/``, ``launch-seals/`` and the rest at its top (:data:`OLD`).

WHERE the home is: ``$RLX_HOME``, else ``$AISQUARE_HOME`` (the name before it, still honoured),
else ``~/.rlx`` if it exists, else ``~/.aisquare`` if it exists, else ``~/.rlx`` (a new user).
HOW it is laid out: a home a variable names is laid out by what is in it (``rlx.db``: the new
layout; ``context.db``, ``launch-seals/``, ``instances/``, ``log/``, an agent's ``*-accounts/`` and
the old layout's other top-level names: the old one;
nothing yet: the new one), so the fleet's agents, which are handed the home as
``AISQUARE_HOME``, read it as the rlx that started them does. ``~/.aisquare`` with no ``~/.rlx``
is the old layout. Every path under the home is built by a helper here, named for what it holds.
"""

from __future__ import annotations

import errno
import functools
import os
import stat
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, TypeVar

_T = TypeVar("_T")

RLX_HOME_ENV = "RLX_HOME"
"""Where rlx's home is (#30): wins over :data:`HOME_ENV_VAR` when both are set."""
HOME_ENV_VAR = "AISQUARE_HOME"
"""The home's variable before 0.10.2, still honoured as an alias of :data:`RLX_HOME_ENV`: the
test suite, the 0.10.3 sandbox and every agent the fleet starts are handed the home by it."""
HOME_NAME = ".rlx"
"""The home's folder in the user's home directory, since 0.10.2."""
OLD_HOME_NAME = ".aisquare"
"""The home's folder before 0.10.2, which a user who has one keeps until H2 migrates it."""

Layout = Literal["rlx", "aisquare"]
"""How a home is laid out: ``rlx`` since 0.10.2 (docs/home-layout.md), ``aisquare`` before it."""
NEW: Layout = "rlx"
OLD: Layout = "aisquare"
NEW_LAYOUT_MARKERS = ("rlx.db",)
"""What only a home in the new layout has at its top."""
OLD_LAYOUT_MARKERS = (
    "context.db",
    "launch-seals",
    "instances",
    "log",
    "fleet-tmux.conf",
    "remote.json",
    "credentials",
)
"""What only a home in the old layout has at its top: one of them makes a home old, and so
does an agent's accounts folder (:data:`OLD_ACCOUNTS_SUFFIX`). Not the store alone: a home only
the CLI used has no ``instances/`` or ``launch-seals/``, and the damaged store's advice moves
``context.db`` aside; read as new then, it would get the new layout beside its old files (the
tester's blocker on #30)."""
OLD_ACCOUNTS_SUFFIX = "-accounts"
"""An agent's accounts folder at an old home's top (``<agent>-accounts``): the agent's name is
its adapter's to say (``AgentAccounts.dir_name``), the suffix is the old layout's."""


def restrict_to_owner(path: Path) -> bool:
    """Make ``path`` readable and writable by its owner only. True when enforced.

    ``chmod(0o600)`` is the whole story on POSIX, and *nothing* on Windows:
    the group/other bits have no NTFS equivalent, so ``os.chmod`` silently
    leaves the file readable by every other account on the machine. Since the
    two callers are an API key and a bearer token, "silently" is the problem.

    ONE icacls call, and each piece of it is load-bearing. ``/inheritance:r``
    removes only *inherited* entries and ``/grant:r`` replaces the grant only
    for the principal it names, so an **explicit** ``BUILTIN\\Users`` ACE —
    inherited from a widened parent at creation time, or set by hand — survives
    both and leaves the file readable by every account on the box. ``/remove``
    is what drops an explicit ACE, and it names the three broad principals by
    SID rather than by display name, which is localised.

    This used to be ``/reset`` followed by a second call, and the pair was not
    atomic. ``/reset`` discards the explicit entries by RESTORING INHERITANCE
    FROM THE PARENT — so between the two calls the file sat on the parent's
    DACL with the secret already written into it, readable by whoever that
    parent grants, for two ``CreateProcess`` calls' worth of time. Worse, a
    failure of the second call left the file *wider than before this function
    was called*, because the first had already thrown away the owner-only DACL
    a previous call had set: a regression reported as ``False`` rather than a
    no-op. One call cannot half-apply.

    The trade is stated rather than hidden: ``/remove`` drops the three ACEs it
    names, where ``/reset`` dropped every explicit ACE. A file carrying an
    explicit grant to some OTHER principal — a domain group, a service account,
    a second local user — keeps it. So the DACL the call left is READ BACK, in
    process, and any principal still granted anything other than this account,
    SYSTEM, Administrators and the owner placeholders makes the answer False:
    the file is not what the callers promise, and they say so (review of #65,
    R3). The read changes nothing, so it cannot widen what the call narrowed.
    ``tests/test_paths.py`` pins both halves: the fourth principal survives,
    and it is reported.

    The trustee is a SID from ``whoami``, not ``getpass.getuser()``. CPython
    returns the first set of ``LOGNAME``, ``USER``, ``LNAME``, ``USERNAME``
    before asking the OS, and the first three are set by MSYS2, Git Bash and
    anything sourcing a POSIX profile. With ``USER=alice`` and a Windows
    account of ``CORP\\a.smith``, ``icacls /grant:r alice:(R,W)`` fails with
    "No mapping between account names and security IDs was done" — which, in
    the old two-call shape, failed *after* the reset had stripped the DACL. A
    SID also sidesteps localised names and domain qualification.

    An ``Administrators`` entry can remain when the parent grants one, which is
    not worth chasing: an admin can take ownership regardless, exactly as root
    reads a 0600 file on POSIX.

    Returns False when the restriction could not be applied, so a caller can
    say so rather than implying a protection that is not there.
    """
    if sys.platform != "win32":
        path.chmod(stat.S_IRUSR | stat.S_IWUSR)
        return True
    # Imported HERE, not at module scope: `subprocess` pulls `signal`,
    # `threading`, `select`, `contextlib` and `warnings`, and this module is a
    # leaf that almost everything imports on a cold start. That is measurable
    # cost on `aisquare --version` on BOTH platforms to buy something only
    # Windows uses. The repo already treats this as a rule worth a test —
    # tests/test_iam_single_reader.py and test_import_cost_of_the_integration.py.
    import subprocess

    sid = _current_user_sid()
    if sid is None:
        return False
    argv = [
        _system32("icacls.exe"),
        str(path),
        "/inheritance:r",
        # Users, Everyone, Authenticated Users — by SID, because the display
        # names are localised and would not match on a non-English Windows.
        "/remove",
        "*S-1-5-32-545",
        "*S-1-1-0",
        "*S-1-5-11",
        "/grant:r",
        f"*{sid}:(R,W)",
    ]
    try:
        result = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    if result.returncode != 0:
        return False
    sddl = dacl_sddl(path)
    return sddl is not None and _grants_only_owner(sddl, sid, _local_account_domain())


def _system32(program: str) -> str:
    """``program`` in ``%SystemRoot%\\System32``, by its full path.

    Run by bare name, ``CreateProcess`` looks in the application's directory
    and the CURRENT directory before System32. An ``icacls.exe`` or
    ``whoami.exe`` planted in a project would run instead, and a fake
    ``whoami`` could turn the owner's grant into one for Everyone (review of
    #65, R8).
    """
    return str(Path(os.environ.get("SYSTEMROOT", r"C:\Windows")) / "System32" / program)


def _current_user_sid() -> str | None:
    """This account's SID from ``whoami``, or ``None`` when it cannot be read.

    ``tests/winacl.py`` reads SIDs the same way and for the same reason — a
    name-based check would be the same vacuous pass one level down.
    """
    import subprocess  # Windows-only; see restrict_to_owner

    try:
        return _whoami_sid()
    except (OSError, subprocess.TimeoutExpired):
        return None


@functools.cache
def _whoami_sid() -> str:
    """The SID ``whoami`` names for this account; raises when it names none.

    Cached: the account cannot change under a running process, and every
    restriction otherwise cost a second subprocess (review of #65, R8). A
    failure RAISES rather than returning ``None``, because an exception is
    never cached, so a ``whoami`` that timed out once is asked again next time.
    """
    import subprocess  # Windows-only; see restrict_to_owner

    result = subprocess.run(
        [_system32("whoami.exe"), "/user", "/fo", "csv", "/nh"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=15,
        check=False,
    )
    if result.returncode != 0:
        raise OSError(f"whoami exited {result.returncode}: {result.stderr.strip()}")
    # '"domain\\user","S-1-5-21-..."'
    sid = result.stdout.strip().split(",")[-1].strip().strip('"')
    if not sid:
        raise OSError(f"whoami named no SID: {result.stdout.strip()!r}")
    return sid


#: Trustees an owner-only file may still grant, as SDDL writes them (an
#: abbreviation, or the SID on a host that spells it out): SYSTEM,
#: Administrators, OWNER RIGHTS and CREATOR OWNER. An administrator can take
#: ownership of any file regardless, the same deal 0600 offers against root.
_PRIVILEGED_TRUSTEES = frozenset(
    {"SY", "S-1-5-18", "BA", "S-1-5-32-544", "OW", "S-1-3-4", "CO", "S-1-3-0"}
)

#: SDDL abbreviates two ACCOUNT SIDs by their RID, so this account can come
#: back as one of these instead of spelled out: the built-in Administrator (a
#: GitHub runner's login) and Guest. Both are THIS MACHINE's accounts: the SID
#: is the local account domain's, and a domain's own Administrator, whose SID
#: ends in -500 too, is spelled out.
_ACCOUNT_ABBREVIATIONS = {"LA": "-500", "LG": "-501"}

#: ACE types that deny. Every other type in a DACL grants something.
_DENYING_ACES = frozenset({"D", "OD", "XD"})


def _grants_only_owner(sddl: str, sid: str, local_domain: str | None) -> bool:
    """Whether the DACL in ``sddl`` grants nobody but ``sid`` and the privileged trustees.

    Each ACE is ``(type;flags;rights;object;inherited object;trustee)``. A
    deny ACE narrows and is skipped. An ACE this cannot read (a conditional
    one carries a nested expression) counts as a grant to someone else: a
    restriction this cannot vouch for is not reported as one. A NULL DACL
    grants everyone everything.

    ``LA`` and ``LG`` are this account only when ``sid`` is that account of
    ``local_domain``, this machine's account domain
    (:func:`_local_account_domain`). Judged by the RID alone, a DOMAIN
    Administrator (``S-1-5-21-<domain>-500``) counted a grant to the LOCAL
    Administrator as its own, and the file was reported owner-only while
    another account could read the secret (review of the #65 re-fold). With
    no domain known, they are someone else's.
    """
    if "NO_ACCESS_CONTROL" in sddl:
        return False
    for ace in sddl.split("(")[1:]:
        fields = ace.split(")", 1)[0].split(";")
        if len(fields) != 6:
            return False
        kind, trustee = fields[0], fields[5]
        if kind in _DENYING_ACES or trustee in _PRIVILEGED_TRUSTEES or trustee == sid:
            continue
        suffix = _ACCOUNT_ABBREVIATIONS.get(trustee)
        if suffix is None or local_domain is None or sid != f"{local_domain}{suffix}":
            return False
    return True


@functools.cache
def _local_account_domain() -> str | None:
    """This machine's account domain SID (``S-1-5-21-a-b-c``); ``None`` when it cannot be read.

    What ``LA`` stands for, less its RID: ``ConvertStringSidToSidW`` resolves an
    SDDL abbreviation as the DACL's own text means it, so there is no second
    rule to keep in step with SDDL's. Read in process through ``advapi32``, as
    :func:`dacl_sddl` reads the DACL. Cached: the machine's domain does not
    change under a running process.
    """
    if sys.platform != "win32":
        return None
    import ctypes
    from ctypes import wintypes

    advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    to_sid = advapi32.ConvertStringSidToSidW
    to_sid.argtypes = [wintypes.LPCWSTR, ctypes.POINTER(ctypes.c_void_p)]
    to_sid.restype = wintypes.BOOL
    to_text = advapi32.ConvertSidToStringSidW
    to_text.argtypes = [ctypes.c_void_p, ctypes.POINTER(wintypes.LPWSTR)]
    to_text.restype = wintypes.BOOL
    local_free = kernel32.LocalFree
    local_free.argtypes = [ctypes.c_void_p]
    local_free.restype = ctypes.c_void_p

    binary = ctypes.c_void_p()
    if not to_sid("LA", ctypes.byref(binary)):
        return None
    try:
        text = wintypes.LPWSTR()
        if not to_text(binary, ctypes.byref(text)):
            return None
        try:
            administrator = text.value or ""
        finally:
            local_free(ctypes.cast(text, ctypes.c_void_p))
    finally:
        local_free(binary)
    domain, _, rid = administrator.rpartition("-")
    return domain if rid == "500" and domain.startswith("S-1-5-21-") else None


def dacl_sddl(path: Path) -> str | None:
    """``path``'s DACL as SDDL, read in process; ``None`` off Windows or when it cannot be read.

    Through ``advapi32`` rather than ``icacls``: ``icacls <path>`` prints
    display names, which are localised and contain spaces, and a read that
    costs no subprocess keeps the restriction at one.
    """
    if sys.platform != "win32":
        return None
    import ctypes
    from ctypes import wintypes

    advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    get_security = advapi32.GetNamedSecurityInfoW
    get_security.argtypes = [
        wintypes.LPCWSTR,  # pObjectName
        ctypes.c_int,  # ObjectType: SE_FILE_OBJECT
        wintypes.DWORD,  # SecurityInfo
        ctypes.POINTER(ctypes.c_void_p),  # ppsidOwner
        ctypes.POINTER(ctypes.c_void_p),  # ppsidGroup
        ctypes.POINTER(ctypes.c_void_p),  # ppDacl
        ctypes.POINTER(ctypes.c_void_p),  # ppSacl
        ctypes.POINTER(ctypes.c_void_p),  # ppSecurityDescriptor
    ]
    get_security.restype = wintypes.DWORD
    to_sddl = advapi32.ConvertSecurityDescriptorToStringSecurityDescriptorW
    to_sddl.argtypes = [
        ctypes.c_void_p,  # SecurityDescriptor
        wintypes.DWORD,  # RequestedStringSDRevision
        wintypes.DWORD,  # SecurityInformation
        ctypes.POINTER(wintypes.LPWSTR),  # StringSecurityDescriptor
        ctypes.POINTER(wintypes.ULONG),  # StringSecurityDescriptorLen
    ]
    to_sddl.restype = wintypes.BOOL
    local_free = kernel32.LocalFree
    local_free.argtypes = [ctypes.c_void_p]
    local_free.restype = ctypes.c_void_p

    se_file_object, dacl_security_information, sddl_revision_1 = 1, 0x4, 1
    descriptor = ctypes.c_void_p()
    status = get_security(
        str(path),
        se_file_object,
        dacl_security_information,
        None,
        None,
        None,
        None,
        ctypes.byref(descriptor),
    )
    if status != 0:
        return None
    try:
        text = wintypes.LPWSTR()
        if not to_sddl(
            descriptor, sddl_revision_1, dacl_security_information, ctypes.byref(text), None
        ):
            return None
        try:
            return text.value
        finally:
            local_free(ctypes.cast(text, ctypes.c_void_p))
    finally:
        local_free(descriptor)


def expand_user(path: Path) -> Path:
    """``path`` with a leading ``~`` expanded, or as written when pathlib cannot expand it.

    ``Path.expanduser`` raises RuntimeError, on every Python, for a ``~olduser`` naming
    a user this machine does not have: a recorded or exported ``~olduser/.claude`` ended
    doctor, status, ``agents list``, ``accounts list`` and uninstall in a traceback
    (sweep of #257). Read as written it names a directory that does not exist, and
    :func:`names_no_home` lets anything that would MAKE it refuse instead. Not for
    :func:`aisquare_home`: a home read as written would be made in the cwd.
    """
    try:
        return path.expanduser()
    except RuntimeError:
        return path


def names_no_home(path: Path) -> bool:
    """Whether ``path`` still starts with a ``~user`` that :func:`expand_user` could not
    expand: a home this machine does not have, where nothing may be made (relative, it
    would land in the cwd)."""
    return bool(path.parts) and path.parts[0].startswith("~") and not path.is_absolute()


def named_home(environment: Mapping[str, str] | None = None) -> Path | None:
    """The home a variable names, ``$RLX_HOME`` first, then ``$AISQUARE_HOME``; ``None``
    when neither is set (``environment``: another process's, else this one's)."""
    source = os.environ if environment is None else environment
    for variable in (RLX_HOME_ENV, HOME_ENV_VAR):
        value = source.get(variable)
        if value:
            return Path(value).expanduser()
    return None


def home_of(
    environment: Mapping[str, str] | None = None, *, user_home: Path | None = None
) -> tuple[Path, Layout]:
    """Where the home of a process with ``environment`` (this one's by default) is, and how
    it is laid out (the module's docstring has the rule). ``user_home`` is that process's home
    directory, when it is not this one's (``core.instance_scan``)."""
    named = named_home(environment)
    if named is not None:
        return named, layout_of(named)
    base = Path.home() if user_home is None else user_home
    new = base / HOME_NAME
    if new.exists():
        return new, NEW
    old = base / OLD_HOME_NAME
    if old.exists():
        return old, OLD
    return new, NEW


_LAYOUTS: dict[tuple[str, int], Layout] = {}
"""Each home's layout as its markers decided it, keyed by its folder and the folder's
modification time, which an entry made or removed at its top moves: one ``stat`` per question
once a marker is there. A home with no marker yet is looked at again every time."""


def layout_of(root: Path) -> Layout:
    """How the home at ``root`` is laid out, by what is at its top: :data:`NEW` with
    ``rlx.db``, :data:`OLD` with any of :data:`OLD_LAYOUT_MARKERS`, :data:`NEW` with neither
    (a home not made yet, or made and not used)."""
    try:
        stamp = root.stat().st_mtime_ns
    except OSError:
        return NEW  # nothing there yet: a new home
    key = (str(root), stamp)
    found = _LAYOUTS.get(key)
    if found is not None:
        return found
    if any((root / marker).exists() for marker in NEW_LAYOUT_MARKERS):
        found = NEW
    elif any((root / marker).exists() for marker in OLD_LAYOUT_MARKERS) or _has_old_accounts(root):
        found = OLD
    else:
        # Not kept: a marker made within the folder's timestamp's resolution (a jiffy on
        # Linux, about 15.6 ms on NTFS) leaves its modification time as it was, and a kept
        # "nothing yet" would outlive it. A home's markers, once there, stay.
        return NEW
    if len(_LAYOUTS) > 64:  # a test suite's many homes: keep it small
        _LAYOUTS.clear()
    _LAYOUTS[key] = found
    return found


def _has_old_accounts(root: Path) -> bool:
    """Whether ``root`` has an agent's accounts folder at its top, as the old layout keeps them."""
    try:
        with os.scandir(root) as entries:
            return any(
                entry.name.endswith(OLD_ACCOUNTS_SUFFIX) and entry.is_dir() for entry in entries
            )
    except OSError:
        return False


def home_layout() -> Layout:
    """How this process's home is laid out (:func:`home_of`)."""
    return home_of()[1]


def aisquare_home() -> Path:
    """Return rlx's home directory (without creating it): :func:`home_of`'s.

    The path is taken verbatim and nothing constrains it to a native disk. This
    is the one place that choice is made, so its consequence is recorded here
    rather than only beside the code that suffers it.

    ``core.config.save_config`` publishes changes with the durable-replace
    recipe — sibling temp, fsync, ``os.replace`` over the target, fsync the
    parent. Two of its properties come from the FILESYSTEM, not from our code:
    the replace is atomic, so a concurrent reader sees the whole old file or the
    whole new one and never a partial document; and the fsyncs cost about what a
    local disk costs, measured at +2.15 ms median for the directory flush.

    Both were measured on a native disk, where the default ``~/.aisquare``
    lives. NEITHER IS MEASURED FOR A WINDOWS-BACKED MOUNT (WSL 9p/DrvFs,
    ``/mnt/c/...``), where a rename becomes a Windows operation and an fsync is
    a round trip to the host. ``AISQUARE_HOME=/mnt/c/...``, or a Windows-side
    HOME, puts config.toml exactly there.

    What is NOT in doubt on such a mount: the temp file is created in the
    TARGET'S OWN DIRECTORY, so temp and target always share a filesystem and the
    precondition ``os.replace`` needs holds by construction. The open question is
    narrower than "does this work on 9p" — it is whether that filesystem's
    rename carries the same whole-old-or-whole-new guarantee, and what an fsync
    costs there.

    Until that is measured, treat a Windows-backed AISQUARE_HOME as unsupported
    FOR THE ATOMICITY GUARANTEE SPECIFICALLY. Everything still functions; what
    has not been ruled out is a torn read during a concurrent write, which on a
    native disk has been.

    ``aisquare doctor`` now reports which filesystem this path is on, so an
    operator does not have to know any of the above to find out they are in the
    unmeasured case — see ``services.diagnostics._check_home_filesystem``. It
    reports rather than refuses: being on a translated filesystem is unmeasured,
    not known-broken.
    """
    return home_of()[0]


def rlx_home() -> Path:
    """:func:`aisquare_home`, by the product's name."""
    return aisquare_home()


def _in_layout(new: tuple[str, ...], old: tuple[str, ...]) -> Path:
    """The path of one thing under the home: ``new`` in the new layout, ``old`` in the old."""
    root, layout = home_of()
    return root.joinpath(*(new if layout == NEW else old))


def cache_dir() -> Path:
    """Directory for disposable cached data."""
    return aisquare_home() / "cache"


def ci_cache_dir() -> Path:
    """Directory for the CI test bed's cached delivery descriptors.

    Descriptors only — the client keeps no cache of hook responses. The server
    caches and reports what it did in ``briefing.cache``; a second cache here
    would make a cached turn's timing describe a network call it never made.
    """
    return cache_dir() / "ci"


def ci_descriptor_path(run_id: str) -> Path:
    """Where the delivery descriptor for ``run_id`` is cached until it expires.

    The run id comes from the environment, so it is treated as a filename
    the way any outside string is: anything outside the id alphabet becomes
    ``_`` and the name is bounded, so no value can name a path outside the
    cache directory.
    """
    safe = "".join(ch if ch.isalnum() or ch in "_-" else "_" for ch in run_id)[:96]
    return ci_cache_dir() / f"descriptor-{safe or 'unknown'}.json"


def log_dir() -> Path:
    """Directory for capture and diagnostic logs."""
    return _in_layout(("logs",), ("log",))


def config_path() -> Path:
    """Path of the TOML config file."""
    return aisquare_home() / "config.toml"


def db_path() -> Path:
    """Path of the SQLite database holding context entries and projects."""
    return _in_layout(("rlx.db",), ("context.db",))


def feature_use_path() -> Path:
    """The spool the feature counters append to (``core.feature_use``): one line per use,
    folded into the store's ``feature_use`` table whenever the stats are read."""
    return aisquare_home() / "feature-use.log"


def usage_sweep_lock_path() -> Path:
    """The lock file one transcript sweep at a time holds (``services.usage_stats``)."""
    return _in_layout(("run", "locks", "usage-sweep.lock"), ("usage-sweep.lock",))


def last_injection_path() -> Path:
    """Path of the record describing the most recent context injection."""
    return cache_dir() / "last_injection.json"


def state_path() -> Path:
    """Path of the small runtime-state file (e.g. the pinned active project)."""
    return aisquare_home() / "state.json"


def remote_state_path() -> Path:
    """``remote.json`` — the Remote Control server's token, password, switches and devices."""
    return _in_layout(("remote", "remote.json"), ("remote.json",))


def remote_audit_path() -> Path:
    """One line per write the Remote Control server let through (``ts sid endpoint summary``)."""
    return _in_layout(("remote", "audit.log"), ("remote-audit.log",))


def remote_dist_dir() -> Path:
    """Default location of the built Remote Control page the server serves."""
    return _in_layout(("remote", "dist"), ("remote-dist",))


def remote_needs_path() -> Path:
    """``remote-needs.json`` — the needs-you items a phone dismissed, and when the watcher
    first saw the ones it dates itself (a pane gone, tmux not answering)."""
    return _in_layout(("remote", "needs.json"), ("remote-needs.json",))


def remote_push_path() -> Path:
    """``remote-push.json`` — the Web Push keys, each device's subscription, what was pushed."""
    return _in_layout(("remote", "push.json"), ("remote-push.json",))


def project_data_dir(project_id: str) -> Path:
    """Per-project data directory (the explainability key, the symbol index)."""
    return aisquare_home() / "projects" / project_id


def brief_evidence_dir(project_id: str, brief_id: str) -> Path:
    """Where a work brief keeps copies of its checks' artifacts, beside ``reports/``.

    Per project, so ``project forget --purge`` removes a project's copies whole.
    """
    return aisquare_home() / "briefs" / project_id / brief_id


def manager_state_file(project_id: str) -> Path:
    """Where a project's manager keeps its state (P1 WS-4, #281): goal, decisions, open items
    and next step, rewritten as a plan step completes. Its start pack reads it back after a
    compaction or a restart, and a quiet compaction waits for a fresh one: each through
    :func:`read_manager_state`, never a plain open, as a sandboxed manager may write its
    directory (``services.store_broker.own_state_dir``)."""
    return project_data_dir(project_id) / "manager" / "state.md"


#: The most of a manager's state file aisquare reads (:func:`read_manager_state`): 16 times
#: what its cycle asks it to keep (``harness.STATE_FILE_MAX`` characters, 4 bytes each at most).
MANAGER_STATE_LIMIT = 64 * 1024


class NotReadBelow(OSError):
    """A file :func:`read_below` would not read, and why (``strerror``): never a missing one,
    which is ``FileNotFoundError``."""


@dataclass(frozen=True)
class ReadBelow:
    """What :func:`read_below` read: the file's bytes, and its ``fstat`` from the descriptor
    they were read through."""

    data: bytes
    info: os.stat_result


def read_manager_state(project_id: str) -> ReadBelow:
    """The state file of ``project_id``'s manager (:func:`manager_state_file`), read below the
    aisquare home by :func:`read_below` at most :data:`MANAGER_STATE_LIMIT` bytes. Its
    directory is the one a sandboxed manager's launch lets its sandbox write (SMOKE13 N1),
    and aisquare reads this file outside any sandbox at every manager's start: a link there
    to ``/dev/zero`` read without end, a FIFO hung the hook, and a link to a file outside
    (``~/.ssh/…``) would have put that file in the start pack (#78 reopen). Raises
    ``FileNotFoundError`` for none, :class:`NotReadBelow` for anything but a regular file
    there."""
    return read_below(
        aisquare_home(),
        ("projects", project_id, "manager", "state.md"),
        MANAGER_STATE_LIMIT,
    )


def read_below(root: Path, names: Sequence[str], limit: int) -> ReadBelow:
    """Read the file ``root``/``names`` when it is a regular file of at most ``limit`` bytes,
    reached from ``root`` one name at a time with no link followed: what aisquare reads,
    outside any sandbox, of a directory a sandbox may write.

    Each name is one component, never ``.``, ``..`` or empty (:class:`NotReadBelow`), so the
    walk cannot climb out of ``root``. On POSIX each directory is opened from the one before
    with ``O_NOFOLLOW``, and the file too, with ``O_NONBLOCK`` (a FIFO opens at once, with no
    writer, and is then refused, never read) and ``O_NOCTTY``. Its ``fstat`` decides before a
    byte is read: a link (``ELOOP``), a directory, a FIFO, a device or a socket is refused,
    and so is a file over ``limit``, by its size and again by what a read returns (one that
    grows meanwhile). Windows has no descriptor walk: each name is ``lstat``-ed first, a link
    or a junction refused, then the file opened, refused unless its ``fstat`` is the file the
    ``lstat`` found (:func:`_open_below_windows`), and asked as on POSIX.

    Raises ``FileNotFoundError`` when nothing is there, and :class:`NotReadBelow`, saying
    why, for everything else it refuses; any other ``OSError`` as the system raised it."""
    _one_name_each(root, names)
    if sys.platform == "win32":
        descriptor = _open_below_windows(root, names)
    else:
        descriptor = _open_below_posix(root, names)
    try:
        info = os.fstat(descriptor)
        kind = _not_regular(info.st_mode)
        if kind is not None:
            raise NotReadBelow(errno.EINVAL, f"it is {kind}, not a regular file")
        if info.st_size > limit:
            raise NotReadBelow(errno.EFBIG, f"it is {info.st_size:,} bytes, over {limit:,}")
        chunks: list[bytes] = []
        size = 0
        while size <= limit:
            chunk = os.read(descriptor, limit + 1 - size)
            if not chunk:
                break
            chunks.append(chunk)
            size += len(chunk)
        if size > limit:
            raise NotReadBelow(errno.EFBIG, f"it grew over {limit:,} bytes while read")
        return ReadBelow(b"".join(chunks), info)
    finally:
        os.close(descriptor)


def stat_manager_state(project_id: str) -> os.stat_result:
    """The ``lstat`` of ``project_id``'s manager's state file (:func:`stat_below`), for when
    it was written: a link is not its state file, nor its target's age its age, and a FIFO
    there is no file at all (#78 reopen). Raises as :func:`read_manager_state` does."""
    return stat_below(aisquare_home(), ("projects", project_id, "manager", "state.md"))


def stat_below(root: Path, names: Sequence[str]) -> os.stat_result:
    """The ``lstat`` of ``root``/``names`` when it is a regular file, reached as
    :func:`read_below` reaches it (each directory on the way from the one before, with no
    link followed), the file itself never opened: a link, a FIFO, a directory or anything but
    a regular file there is :class:`NotReadBelow`, nothing there ``FileNotFoundError``."""
    _one_name_each(root, names)
    if sys.platform == "win32":
        info = _lstat_below_windows(root, names)
    else:
        parent = _dirs_below_posix(root, names[:-1])
        try:
            info = os.stat(names[-1], dir_fd=parent, follow_symlinks=False)
        finally:
            os.close(parent)
    kind = _not_regular(info.st_mode)
    if kind is not None:
        raise NotReadBelow(errno.EINVAL, f"it is {kind}, not a regular file")
    return info


def _one_name_each(root: Path, names: Sequence[str]) -> None:
    """Refuse (:class:`NotReadBelow`) no names, or a name that is not one component below
    ``root``: empty, ``.``, ``..`` or holding a separator, which could climb out of it."""
    if not names:
        raise NotReadBelow(errno.EINVAL, f"no file named below {root}")
    for name in names:
        if name in ("", ".", "..") or "/" in name or os.sep in name or (os.altsep or "/") in name:
            raise NotReadBelow(errno.EINVAL, f"{name!r} is not one name below {root}")


def _not_regular(mode: int) -> str | None:
    """What a file of ``mode`` is when it is not a regular file, in words; ``None`` for one."""
    for test, said in (
        (stat.S_ISDIR, "a directory"),
        (stat.S_ISFIFO, "a FIFO"),
        (stat.S_ISLNK, "a symbolic link"),
        (stat.S_ISSOCK, "a socket"),
        (stat.S_ISCHR, "a device"),
        (stat.S_ISBLK, "a device"),
    ):
        if test(mode):
            return said
    return None if stat.S_ISREG(mode) else "not a regular file"


def _open_below_posix(root: Path, names: Sequence[str]) -> int:
    """A descriptor on the file ``root``/``names``: its directory by :func:`_dirs_below_posix`,
    then the file from it with ``O_NOFOLLOW``, ``O_NONBLOCK`` and ``O_NOCTTY``."""
    nofollow = getattr(os, "O_NOFOLLOW", 0)
    final = os.O_RDONLY | nofollow | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_NOCTTY", 0)
    parent = _dirs_below_posix(root, names[:-1])
    try:
        return os.open(names[-1], final, dir_fd=parent)
    except OSError as exc:
        if exc.errno == errno.ELOOP:
            raise NotReadBelow(exc.errno, f"{names[-1]} is a symbolic link") from None
        raise
    finally:
        os.close(parent)


def _dirs_below_posix(root: Path, dirs: Sequence[str]) -> int:
    """A descriptor on the directory ``root``/``dirs``, each opened from the last with
    ``O_NOFOLLOW``: a link or a name that is not a directory on the way is
    :class:`NotReadBelow`."""
    directory = getattr(os, "O_DIRECTORY", 0)
    descend = os.O_RDONLY | directory | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(root, os.O_RDONLY | directory)
    try:
        for name in dirs:
            try:
                deeper = os.open(name, descend, dir_fd=descriptor)
            except OSError as exc:
                if exc.errno in (errno.ELOOP, errno.ENOTDIR):
                    what = "a symbolic link" if exc.errno == errno.ELOOP else "not a directory"
                    raise NotReadBelow(exc.errno, f"{name} on the way is {what}") from None
                raise
            os.close(descriptor)
            descriptor = deeper
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def _lstat_below_windows(root: Path, names: Sequence[str]) -> os.stat_result:
    """The ``lstat`` of ``root``/``names``, each name ``lstat``-ed in turn: a link or a
    junction anywhere refused (Windows has no ``O_NOFOLLOW`` and no descriptor walk)."""
    here = root
    found = os.lstat(root)
    for name in names:
        here = here / name
        found = os.lstat(here)
        reparse = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
        if stat.S_ISLNK(found.st_mode) or getattr(found, "st_file_attributes", 0) & reparse:
            raise NotReadBelow(errno.ELOOP, f"{name} on the way is a link or a junction")
    return found


def _open_below_windows(root: Path, names: Sequence[str]) -> int:
    """A descriptor on ``root``/``names`` once :func:`_lstat_below_windows` found no link
    or junction on the way and a regular file there: Windows refuses to open a directory
    (``PermissionError``), so what it is is asked first, as :func:`read_below` asks the
    descriptor after.

    The file is then opened by its name, which follows a link: one put in its place after
    the ``lstat`` (Developer Mode lets any process make one) was read, its target's bytes in
    a manager's start pack (review r39 of #113, finding 4). So the descriptor's ``fstat``
    must be the file the ``lstat`` found (``st_dev`` and ``st_ino``: its volume and its file
    id on Windows), else it is refused. Only the file's own name can be swapped so: the
    directories above it are the store's, which no sandbox writes (the one it may is the
    file's own, ``services.store_broker.own_state_dir``)."""
    found = _lstat_below_windows(root, names)
    kind = _not_regular(found.st_mode)
    if kind is not None:
        raise NotReadBelow(errno.EINVAL, f"it is {kind}, not a regular file")
    descriptor = os.open(root.joinpath(*names), os.O_RDONLY | getattr(os, "O_BINARY", 0))
    try:
        opened = os.fstat(descriptor)
        if (opened.st_dev, opened.st_ino) != (found.st_dev, found.st_ino):
            raise NotReadBelow(
                errno.ELOOP, f"{names[-1]} was put in another file's place as it was opened"
            )
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


def credentials_path() -> Path:
    """Path of the credentials file (API keys, tokens)."""
    return _in_layout(("accounts", "aisquare", "credentials"), ("credentials",))


def agent_accounts_dir(name: str) -> Path:
    """Where the CLI keeps an agent's homes, one per slot: its accounts' ``dir_name``
    (``claude-accounts``, ``codex-accounts``), as ``core.claude_accounts.accounts_root`` asks."""
    return _in_layout(("accounts", name.removesuffix("-accounts")), (name,))


def agents_registry_path() -> Path:
    """Path of the agents registry (detected and connected agents)."""
    return aisquare_home() / "agents.json"


def explainability_key_path() -> Path:
    """The workspace ingest key (mode 600)."""
    return aisquare_home() / "explainability-key"


def oidc_cache_dir() -> Path:
    """The sign-in tokens ``services.iam`` caches, one file per server."""
    return cache_dir() / "oidc"


def explainability_dir() -> Path:
    """Directory for explainability artefacts written by the launcher."""
    return aisquare_home() / "explainability"


def explainability_joins_path() -> Path:
    """Path of the session→Run join log (JSON Lines, append-only).

    Deliberately NOT under ``cache/``: this is the only local copy of the key
    that joins board rows to gateway Runs, and ``cache/`` is documented as
    disposable.
    """
    return explainability_dir() / "joins.jsonl"


def truncation_marker_path() -> Path:
    """Records that ``context.db`` was found emptied and rebuilt.

    Deliberately NOT under ``cache/``: that directory is documented as
    disposable, and this is the only surviving evidence that a board's history
    was lost. It exists because the fact is knowable for exactly one line —
    ``open_store`` sees a zero-length file, and one statement later the schema is
    back and nothing can tell an emptied store from a new machine.

    Cleared by the operator, not by us. A warning that clears itself is one
    nobody has to answer.
    """
    return aisquare_home() / "store-was-truncated"


# --- the home's layout, one helper per thing (#30) -------------------------------------------
#
# Every path under the home is built here, by a helper named for what it holds, so that the
# layout lives in this one file: a module asks for ``launch_seals_dir()``, never joins
# ``aisquare_home()`` with a name of its own.


def shown(path: Path) -> str:
    """``path`` as a person reads it: ``~/…`` under their home directory, else whole."""
    try:
        return "~/" + path.relative_to(Path.home()).as_posix()
    except ValueError:
        return str(path)


def projects_dir() -> Path:
    """Every project's own folder (:func:`project_data_dir`), one per project id."""
    return aisquare_home() / "projects"


def briefs_dir() -> Path:
    """The work briefs' evidence, one folder per project (:func:`project_briefs_dir`)."""
    return aisquare_home() / "briefs"


def project_briefs_dir(project_id: str) -> Path:
    """One project's briefs' evidence (:func:`brief_evidence_dir` is one brief's)."""
    return briefs_dir() / project_id


def reports_dir() -> Path:
    """The command reports a person or an agent asked to keep (``services.command_reports``)."""
    return aisquare_home() / "reports"


def run_dir() -> Path:
    """What a running rlx keeps while it runs: prompts on their way, typing leases, origins."""
    return aisquare_home() / "run"


def instances_dir() -> Path:
    """The registry of the rlx instances running on this home (``core.instances``)."""
    return _in_layout(("run", "instances"), ("instances",))


def launch_seals_dir() -> Path:
    """The fleet's launch records, one per agent (``core.launch_seal``)."""
    return _in_layout(("fleet", "launch-seals"), ("launch-seals",))


def launchers_dir() -> Path:
    """The launchers the fleet writes for its agents (``core.launchers``)."""
    return aisquare_home() / "launchers"


def handoffs_dir() -> Path:
    """The hand-off briefs a crossing switch writes (``services.handoff``)."""
    return aisquare_home() / "handoffs"


def fleet_dir() -> Path:
    """The fleet's own files: its admission log."""
    return aisquare_home() / "fleet"


def tmux_conf_path() -> Path:
    """The tmux config the fleet's server starts with (``core.tmux.bundled_conf``'s)."""
    return _in_layout(("fleet", "tmux.conf"), ("fleet-tmux.conf",))


def watch_dir() -> Path:
    """What the watcher keeps: its lock, its pid, the details it files (``services.watch``)."""
    return aisquare_home() / "watch"


def captain_dir() -> Path:
    """The captain's folder: its brain, its queue, its ui socket's fallback."""
    return aisquare_home() / "captain"


def personas_dir() -> Path:
    """The user's personas (``core.personas``)."""
    return aisquare_home() / "personas"


def shared_memory_dir(root_name: str) -> Path:
    """The memory an agent's accounts share, under its accounts' root (``root_name``)."""
    return aisquare_home() / root_name / "memory"


def persona_prompts_dir() -> Path:
    """The persona prompts a launch writes for its agent: disposable, so under the cache."""
    return cache_dir() / "persona-prompts"


def screenshots_dir() -> Path:
    """Where the board's screenshot key saves, unless told elsewhere (``cli.watch``)."""
    return aisquare_home() / "screenshots"


def bench_dir() -> Path:
    """The usage sweep's benchmark scratch (``services.usage_stats``)."""
    return aisquare_home() / "bench"


def usage_share_lock_path() -> Path:
    """The lock one usage shipment at a time holds (``core.sharing``)."""
    return _in_layout(("run", "locks", "usage-share.lock"), ("usage-share.lock",))


def active_ping_lock_path() -> Path:
    """The lock one daily active ping at a time holds (``services.active_ping``)."""
    return _in_layout(("run", "locks", "active-ping.lock"), ("active-ping.lock",))


def broker_dir(home: Path) -> Path:
    """The launches' broker folders under the home ``home``, as a sandbox's launch names it
    (``services.store_broker``): ``broker`` in either layout."""
    return home / "broker"


def ngrok_token_path() -> Path:
    """The ngrok authtoken saved in Settings → Remote (``services.ngrok_sdk``)."""
    return _in_layout(("remote", "ngrok-authtoken"), ("ngrok-authtoken",))


def ngrok_config_path(name: str) -> Path:
    """Our ngrok config of one ngrok version (``services.ngrok_tunnel``)."""
    return _in_layout(("remote", name), (name,))


NEW_LAYOUT_FOLDERS: tuple[tuple[str, ...], ...] = (
    ("cache",),
    ("logs",),
    ("accounts", "aisquare"),
    ("fleet",),
    ("run", "locks"),
    ("remote",),
)
"""The folders a home in the new layout is made with (docs/home-layout.md): what is filed
under them finds its folder there, whoever writes it first."""


def ensure_home() -> Path:
    """Create the home if missing, laid out as its layout says, and return its root: the new
    layout's folders (:data:`NEW_LAYOUT_FOLDERS`), or the old layout's ``cache`` and ``log``."""
    home, layout = home_of()
    home.mkdir(parents=True, exist_ok=True)
    if layout == NEW:
        for parts in NEW_LAYOUT_FOLDERS:
            home.joinpath(*parts).mkdir(parents=True, exist_ok=True)
    else:
        for directory in (cache_dir(), log_dir()):
            directory.mkdir(parents=True, exist_ok=True)
    return home


# --- Windows file contention ---------------------------------------------------
#
# Here rather than in `core.config` because it is a FILESYSTEM fact, not a
# config one, and four modules want it: `config.load_config` and
# `save_config`'s read of the existing file, `credentials.load_all`,
# `state_file.read_state`, and `atomic.write_replacing`'s rename, which every
# replace-by-rename writer shares. All four already import this module, and
# `paths` imports nothing from `aisquare`, so it is the one place none of them
# has to reach sideways for. `tests/test_windows_contention.py` holds each call
# site to it.


#: Windows error codes meaning "someone else has this file open right now":
#: ERROR_ACCESS_DENIED and ERROR_SHARING_VIOLATION.
_WINDOWS_BUSY = frozenset({5, 32})


def _is_contention(exc: PermissionError) -> bool:
    """Whether ``exc`` is Windows saying "busy" rather than "you may not".

    The two sides report it DIFFERENTLY, which is worth writing down because
    matching only the obvious one silently disables half the retry:

    * ``os.replace`` raises through the Win32 layer and carries ``winerror``
      5 or 32.
    * ``Path.open`` raises through the C runtime, which sets ``errno`` 13 and
      leaves ``winerror`` as **None** — measured, 122 of 122 racing reads.

    A genuine "you may not read this" is indistinguishable from the second
    form, so it is retried too and then raised unchanged. That costs ~0.9s on a
    path that was going to fail anyway, and buys the reader case being covered
    at all.
    """
    winerror = getattr(exc, "winerror", None)
    if winerror is not None:
        return winerror in _WINDOWS_BUSY
    return exc.errno == errno.EACCES


#: Backoff for :func:`despite_windows_contention`. Bounded on purpose — these
#: are operator commands, so a slow failure is nearly as bad as a wrong one.
#: Ten tries over ~0.9s clears the contention that actually occurs (both the
#: read and the rename hold the file for microseconds) without turning a
#: genuine permission problem into a hang.
_BUSY_ATTEMPTS = 10
_BUSY_BACKOFF_SECONDS = 0.02


def despite_windows_contention(action: Callable[[], _T]) -> _T:
    """Run ``action``, retrying while Windows reports the file as busy.

    On POSIX this is a plain call: a rename over an existing name always
    succeeds, and a reader that already has the file open keeps its own inode,
    so neither side can observe the other.

    NTFS shares no such guarantee, and both sides of a config write hit it:

    * ``MoveFileEx`` refuses to replace a file that any other handle has open —
      including one opened purely for reading — so a second session merely
      READING the config failed a write with a bare ``Access is denied``.
    * and for the width of that rename, opening the destination fails too, so
      the reader takes an ``Access is denied`` of its own.

    Measured directly, not inferred: a replace over a target held open for read
    raises WinError 5, and a as-fast-as-possible read/write storm produces both
    directions. The second one is the more expensive of the two, because
    ``cli/launch.py`` treats an unreadable config as "launch untraced" by
    design — so on Windows a config write racing a launch silently cost
    tracing, with nothing raised anywhere to say so.

    Every window here is microseconds wide, which is what makes retrying the
    right remedy rather than a papering-over. The last failure is re-raised
    unchanged once the attempts run out, so a genuine permission problem still
    surfaces as itself rather than as a timeout, and the caller's
    symlink-aware wrapping still applies.
    """
    if sys.platform != "win32":
        return action()
    for attempt in range(_BUSY_ATTEMPTS):
        try:
            return action()
        except PermissionError as exc:
            if not _is_contention(exc):
                raise
            if attempt == _BUSY_ATTEMPTS - 1:
                raise
            time.sleep(_BUSY_BACKOFF_SECONDS * (attempt + 1))
    raise AssertionError("unreachable: the loop either returns or raises")
