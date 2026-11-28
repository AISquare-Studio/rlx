# rlx's home: where it is and how it is laid out

rlx keeps its configuration and state in one folder, its home. Since 0.10.2 the home is
`~/.rlx`. A home from before 0.10.2 (`~/.aisquare`) keeps working as it is, in its own layout,
until `rlx upgrade` moves it ([Moving a home from before 0.10.2](#moving-a-home-from-before-0102)).

## The layout

```text
~/.rlx/
  config.toml               the configuration (rlx config, Settings)
  state.json                small runtime state: the switches, the pinned project, the theme
  agents.json               the agents rlx found and connected
  rlx.db                    the store: projects, groups, fleet rows, boards, tasks, notes, memory
  accounts/
    claude/<slot>/          one Claude Code login per managed account (its CLAUDE_CONFIG_DIR)
    codex/<slot>/           one Codex login per managed account (its CODEX_HOME)
    aisquare/credentials    the AISquare sign-in's credentials
  projects/<project-id>/    a project's own files: its manager's state (manager/state.md),
                            its code index (code/), its explainability key
  fleet/
    tmux.conf               the options the fleet's tmux server starts with
    launch-seals/           one launch record per agent: where the fleet may act for it
  run/
    instances/              the rlx instances running on this home
    locks/                  locks held while one process does a thing (a sweep, a shipment)
  remote/                   Remote: remote.json, its audit log, the phones' push keys, the page
  watch/                    what the watcher keeps
  logs/
  cache/                    regenerable: each account's scratch, the usage readings' cache
  tmp/<key>/                each agent's own TMPDIR, removed when the agent ends
```

Some folders are the same in both layouts and not listed above: `briefs/`, `reports/`,
`captain/`, `personas/`, `explainability/`, `launchers/`, `handoffs/`, `screenshots/`. The plain
Claude Code login stays in `~/.claude`, and the plain Codex login in `~/.codex`. rlx never moves
either.

## Projects, managers and agents

The hierarchy lives in the store, not in folders:

- a **group** holds projects (`project_group`);
- a **project** has a board: its tasks, notes and sessions;
- a project's **fleet** is its agent rows: the **manager** and the coders, testers and reviewers
  it runs, each with its label, role, account and session.

Folders hold only what has to be a file:

- account homes (`accounts/`);
- launch seals (`fleet/launch-seals/`);
- a project's manager state and code index (`projects/<project-id>/`);
- logs and sockets.

An agent's worktree is in its project's repository, never under the home.

## Where the home is

The first of these that applies:

1. `$RLX_HOME`, if it is set.
2. `$AISQUARE_HOME`, if it is set. This is the name from before 0.10.2, still honoured.
3. `~/.rlx`, if it exists.
4. `~/.aisquare`, if it exists: a home from before 0.10.2.
5. `~/.rlx`, for a new user. rlx makes it, laid out as above, the first time it needs it. It
   never makes `~/.aisquare`.

When both variables are set, `RLX_HOME` wins. rlx hands the home to every agent it starts, by
both names.

## How a home is laid out

The variable says **where** the home is. The folder says **how** it is laid out:

- A home a variable names is in the new layout if it has `rlx.db` at its top.
- It is in the old layout if it has any of the old layout's own names there: `context.db`,
  `launch-seals/`, `instances/`, `log/`, `fleet-tmux.conf`, `remote.json`, `credentials`, or an
  agent's accounts folder (`claude-accounts/`, `codex-accounts/`). A home whose store was moved
  aside is still read as old.
- A home with neither, an empty or new folder, gets the new layout.
- `~/.aisquare`, when there is no `~/.rlx`, is in the old layout.

So an rlx and every agent it starts read one home the same way, whichever name they were handed
it by.

The old layout keeps everything at the top of the home:

| | Old layout | New layout |
|---|---|---|
| The store | `context.db` | `rlx.db` |
| Logs | `log/` | `logs/` |
| Claude Code accounts | `claude-accounts/<slot>/` | `accounts/claude/<slot>/` |
| Codex accounts | `codex-accounts/<slot>/` | `accounts/codex/<slot>/` |
| AISquare sign-in | `credentials` | `accounts/aisquare/credentials` |
| Launch seals | `launch-seals/` | `fleet/launch-seals/` |
| The fleet's tmux options | `fleet-tmux.conf` | `fleet/tmux.conf` |
| Running instances | `instances/` | `run/instances/` |
| Locks | `usage-sweep.lock`, `usage-share.lock`, `active-ping.lock` | `run/locks/` |
| Remote | `remote.json`, `remote-audit.log`, `remote-dist/`, `remote-needs.json`, `remote-push.json`, `ngrok-authtoken`, `remote-ngrok-v*.yml` | `remote/remote.json`, `remote/audit.log`, `remote/dist/`, `remote/needs.json`, `remote/push.json`, `remote/ngrok-authtoken`, `remote/remote-ngrok-v*.yml` |

Everything else has the same name in both.

## Moving a home from before 0.10.2

`rlx upgrade` moves an old home to the new layout: `~/.aisquare` becomes `~/.rlx`, and a home
`RLX_HOME` or `AISQUARE_HOME` names is laid out anew where it is. Nothing else moves a home: no
other command, no hook and no start of rlx does.

A variable that names `~/.aisquare` itself (`RLX_HOME` or `AISQUARE_HOME`; every window the fleet
opens has both) still means the default home: it moves to `~/.rlx` and is linked, as without the
variable. After the move, a variable that still names `~/.aisquare` (a shell's startup file, a
shell started from a fleet window) means `~/.rlx`, with every account's folder spelled from it,
so each account reads signed in. It does so by any spelling of `~/.aisquare`: a link to it, a
trailing slash, or resolved. Such a variable is read in every way as no variable: on a machine
with no home yet rlx makes `~/.rlx`, and a `~/.aisquare` left as a link to nothing is passed by.

In the app, the ⬆ row under Settings offers **Move rlx's home** while the home is old, even when
no newer release is out. Its *Restart & move* closes rlx, moves the home and reopens rlx. An
update to a newer release moves the home too. Doctor's `home-layout` row says the home is laid
out before 0.10.2 until it moves. An install rlx does not upgrade itself (Homebrew, pipx) moves
its home with `rlx move-home`.

The release that moves homes is 0.10.2, and an upgrade from 0.10.1 runs 0.10.1's code, which does
not know how. So on a home from before 0.10.2, run `rlx upgrade` twice: the first installs 0.10.2,
and the second (nothing newer to install) moves the home.

### Only while nothing runs on the home

The move refuses, and names each thing it found, while any of these run on the home:

- an rlx (the app), here or on another machine that shares the home;
- a fleet agent of any project;
- a window on rlx's tmux server, or on a server a launch record names;
- the watcher;
- Remote, served from another process;
- an agent started by rlx 0.10.2 or later with `rlx launch`, on any account and any system,
  or with `rlx accounts run` or `rlx team spawn --exec` on macOS or Linux.

Stop them, then run `rlx upgrade` again. Nothing is touched until the home is quiet. The app's
Move rlx's home says what runs before it closes rlx.

Some agents leave no record, so the move cannot see them: stop them first. On Windows,
`rlx accounts run` and `rlx team spawn --exec` start the agent and leave no record. An agent
started by rlx 0.10.1, by any of the three commands, left none either.

The rlx service (`rlx service`, [service.md](service.md)) is not in that list: the move stops it
first and starts it again afterwards, on the home where it then is. A service that does not stop
refuses the move.

Only one move of a home runs at a time, and a drop of its cache counts as one. While it runs,
the move holds a lock file beside the home, in the folder that holds it, outside the folders the
move renames: `~/.rlx-home-move-<digest>.lock` for `~/.aisquare`. The file is removed when the
move ends. A move that was killed leaves its lock file, and the next run takes it and removes it.
If rlx can't make that file, because the folder that holds a home named by a variable isn't
yours to write, the move refuses and names the file.

### What it does

Each step is written down first in a journal, `backups/home-migration/journal.json` in the home,
so a crash at any step is finished by the next run:

1. **The store is backed up**, by SQLite's own backup, to `backups/home-migration/context.db`. It
   stays there until the next release, then you can delete it.
2. **Each Claude Code login is copied in the Keychain** (macOS). Claude Code names a managed
   account's Keychain item after its folder, and the folder moves. So rlx copies each item to the
   name its new folder gives, as Claude Code writes one, and reads the copy back with Claude
   Code's own lookup. The secret goes to `security` on its input, never on a command line.
   `security` reads one line of input at most 4,096 bytes long, so a login over about 1,990
   bytes (a Claude Code login with several MCP sign-ins can be) is not copied. That account is
   pinned instead: its marker records the folder its old item is named by, and every launch on
   it sets `CLAUDE_SECURESTORAGE_CONFIG_DIR` to it, so Claude Code and rlx keep reading the old
   item. Its old item is not deleted.
3. **The folders are renamed** to their new places (the table above), on the same disk: a rename,
   never a copy. The store goes last in a named home. In the default home, `~/.aisquare` is then
   renamed to `~/.rlx`.
4. **Stored paths are rewritten**: the store's (projects, fleet rows, sessions, accounts, briefs,
   settings), `agents.json`, `state.json`, `config.toml` and `remote.json`. Each launch record is
   re-issued by the fleet's own seal code with its paths moved. A path outside the home is never
   changed.
5. **The move is checked**: every fleet agent that could be restarted from its launch record
   before can still be, by the same check a restart passes, and every account signed in before is
   signed in now. If not, the whole move is rolled back.
6. **Hooks are written again** in each managed account, for its new folder (Codex's trust of them
   included).
7. **`~/.aisquare` is left as a link to `~/.rlx`** until the next release, so an old reference (a
   running agent's environment, a shell snapshot, a script) still finds the home. On Windows it is
   a junction. If neither can be made, the move says so.
8. **The old Keychain items are deleted.**

Before step 8, the move can be rolled back whole: the link removed, each rewritten file put back,
the store restored from its backup, each rename reversed, and the Keychain copies deleted. A
failure at any of those steps does this by itself. After a crash, `rlx move-home --rollback`
does it. The roll-back is written down as it goes, so a crash during it is finished by the next
run. From step 8 on, the move can only be finished.

The cache moves with the home, as a rename costs nothing. After the move, `rlx upgrade` at a
terminal offers to drop it, saying its size; off a terminal it says how. `rlx move-home
--drop-cache` drops it, after the move as well: rlx makes it again as it needs it. The cache holds
each account's scratch (a live session's files, a background task's output), so it is dropped
only while nothing runs on the home, the rlx service stopped and started again around it.
Otherwise it is refused, naming what runs.

`asq-statusline`, which Claude Code runs to draw its status line, is not a command of rlx's. It
may write a file under `cache/statusline/` while a move is unfinished, which is harmless:
`cache/` has that name in both layouts. If something makes `~/.aisquare` again before the move
links it, what was made is kept beside it (`~/.aisquare-made-during-the-move-<when>`), and the
link is made.

While a move is unfinished (a crash stopped it), no other rlx command runs: each says how to
finish it (`rlx move-home`) or put the home back (`rlx move-home --rollback`). A help page and
`rlx uninstall --dry-run` still answer, and an agent's hook does nothing meanwhile. A journal
this rlx cannot read, or a newer release's, is refused the same way by every command and by the
move: finish that move with the release that began it, or check the home and move the journal
aside. A journal this rlx cannot read includes one that cannot be opened, one whose bytes are
not UTF-8, and one too deeply nested to parse. Otherwise a command would make a new, empty
store where the old one was, and the move could not be finished.

### A journal rlx cannot read

The move writes its journal (`backups/home-migration/journal.json`) twice: the journal itself,
then a copy beside it (`journal-copy.json`). When the journal cannot be read and its copy can,
nothing is lost. Every command says the move is part-way, and `rlx move-home` finishes it from
the copy, writing the journal whole again. Something at either place that is not a file (a
folder, a link) is renamed aside first, as `journal.json.unreadable-<when>`, and kept. A copy a
newer rlx wrote is that release's move: finish it, or roll it back, with that release.

When neither can be read, rlx cannot tell from them how far the move got. Every command stops,
names the file, and says what the home itself shows:

- **Nothing moved yet:** no name is in the new layout's places (the table above), and there is
  no `accounts/`, `logs/` or `remote/` at the home's top. Move the journal aside, and rlx runs
  again; `rlx upgrade` then moves the home from the start.
- **Part-way:** names in both layouts' places. A move renames one name at a time, so this is a
  move stopped in the middle. Do not move the journal aside: rlx would then read a home with
  some of its folders gone (an account's logins among them), and no later move could finish
  it. Leave the home as it is. The journal's folder holds the store as it was before the move
  (`context.db`).
- **Moved:** every name in its new place, though a step after the renames (the paths stored in
  the home, the hooks, the link from `~/.aisquare`) may be left. Leave it as it is too.

In those last two cases rlx cannot finish the move or put it back by itself. Report it at
[issues](https://github.com/AISquare-Studio/rlx/issues), with what the home's top holds; the
home itself is not changed meanwhile.

If this user may not read the journal (its permissions changed), rlx says so: make the file and
its folders readable again, and rlx takes the move up from it.

An install rlx does not upgrade itself (Homebrew, pipx) has its home moved by `rlx upgrade`
too, which then says how that install upgrades.

### What it leaves

- **Notes, task text and other prose** that mention the old home keep their text. The move counts
  them, and the link keeps those paths working until the next release.
- **Claude Code's shell snapshots** in each account keep the old home's path. They work through
  the link, and Claude Code writes new ones.
- **No move back.** An rlx from before 0.10.2 keeps its home in `~/.aisquare`, in the old layout.
  Run after the move, it finds no `context.db` there and starts empty. `rlx upgrade --version`
  warns of this before it moves back.

### Linux and Windows

Claude Code keeps its login in `.credentials.json` in the account's folder there, so it moves with
the folder and no credential store is touched. A Codex account's login is its `auth.json`, which
moves with its folder on every platform. A Codex account set to keep its login in the system's
keyring (`cli_auth_credentials_store = "keyring"`) is keyed by its folder, and is not moved: sign
in to it again after the move.

No Windows CI run moves a home: the move's tests use a stand-in for `security`, a POSIX script,
and run on macOS and Linux only. On Windows the renames, the roll-back and the junction
`~/.aisquare` → `~/.rlx` are not exercised by any test; the Windows fleet runs in WSL in
0.10.2, where the move runs as on Linux.

### Checking the Keychain copy by hand

The tests run against a stand-in for `security`. To check the copy against a real Keychain, use a
throwaway macOS user or a VM, never your own login Keychain:

1. In the throwaway user, install rlx 0.10.1, add a managed Claude Code account
   (`rlx accounts add`), and sign in to it.
2. Note the item: `security find-generic-password -s "Claude Code-credentials-<hash>"` names it,
   where `rlx accounts list --json` shows the account's folder.
3. Upgrade to 0.10.2 and run `rlx upgrade` again to move the home.
4. Check that `rlx accounts list` shows the account signed in, that `claude` launched on it
   (`rlx launch --account <n>`) asks for no login, and that Keychain Access shows the item under
   its new name and no longer under the old one.

## For contributors

Every path under the home is built in one file, `src/aisquare/core/paths.py`, by a helper named
for what it holds (`paths.db_path()`, `paths.launch_seals_dir()`, `paths.agent_accounts_dir(...)`).
No other module joins the home with a name of its own, and a test guards it.
