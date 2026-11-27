# rlx's home: where it is and how it is laid out

rlx keeps its configuration and state in one folder, its home. Since 0.10.2 the home is
`~/.rlx`. A home from before 0.10.2 (`~/.aisquare`) keeps working as it is, in its own layout,
until a later release migrates it.

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

## For contributors

Every path under the home is built in one file, `src/aisquare/core/paths.py`, by a helper named
for what it holds (`paths.db_path()`, `paths.launch_seals_dir()`, `paths.agent_accounts_dir(...)`).
No other module joins the home with a name of its own, and a test guards it.
