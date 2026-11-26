# The rlx service

Some parts of rlx keep working only while an rlx window is open: Remote, the reads of your
accounts' usage, and the retries of a manager nudge that was held back. The rlx service is a
background rlx with no window that keeps them going. With it running, you can quit every rlx
window, at the Mac or over SSH, and:

- **Remote** keeps serving your phone, with its ngrok link, its auto-off and its switches;
- **usage reads** go on every minute, so every agent's status line keeps the fleet's figure and
  the before-limit hand-over has fresh readings to go on;
- a **manager nudge** that a draft or a dialog held back goes out once the input box is clear;
- the **usage sweep** for *Usage & stats*, and the anonymous usage shipment (when sharing is
  on), keep running.

Limit hand-overs, the watcher and the captain never needed a window, and still don't.

## Commands

```sh
rlx service start      # start it in the background, unless it already runs
rlx service status     # whether it runs, its version, uptime, and how each part is
rlx service stop       # stop it: Remote and ngrok are wound down cleanly
rlx service restart
rlx service run        # run it in this terminal instead (Ctrl-C stops it)
rlx service install    # start it at each login (on by default), and now
rlx service uninstall  # stop starting it at login, and stop the one the OS started
```

`rlx service status --json` prints the same as one object: `running`, `pid`, `version`,
`this_version`, `version_mismatch`, `host`, `started_at`, `uptime_s` and `parts`, one entry per
part with its `state` (`ok`, `off`, `waiting` or `error`), a `detail` line and `last_run`, and
`at_login`: `on`, `how` (`launchd` or `systemd`), `item` (the login item's file), `chosen` (`on`,
`off`, or `null` when nothing was chosen yet) and `why_off`. It exits 3 when no service runs.

There is one service per home (`~/.aisquare`). Its status is in `~/.aisquare/service/`, and
its log in `~/.aisquare/log/service.log`, which rolls over at 1 MB with one old file kept.

`rlx upgrade` stops the service before it replaces rlx and starts it again on the new release.
`rlx uninstall` stops it and takes its login item away. Neither refuses to run beside it, as they
do beside an rlx window.

## With rlx windows open

An rlx window that finds the service running leaves those parts to it:

- Remote's switch, auto-off picker, passphrase and link work as usual. The switch is saved, and
  the service turns Remote on or off within two seconds. The panel says Remote is served by the
  rlx service.
- The window shows the usage the service read, and retries no nudge itself.
- The chip under the status line counts the service: `2 rlx · Mac (here) + service`.

If the service stops while a window is open, that window takes the parts back within 30
seconds: Remote comes back on there if its switch is on, as it is saved then. The window says
so once.

If Remote can't start in the service (its port is taken, say), the service tries again a few
times over about a minute, then turns Remote's switch off. The window's Remote panel says why.

With no service, rlx windows work as they always have.

## One release per home

The service is one more rlx on the home ([Running more than one rlx](multi-instance.md)):

- An rlx window of another release offers to quit the service, as it offers to quit another
  window, before it starts.
- The service doesn't start beside an rlx window of another release. It says which one, and
  you quit that one first.
- `rlx service status` names a version mismatch between the service and the rlx you run it from.

## The Keychain and SSH (macOS)

On macOS, Claude Code keeps each login in the login Keychain, which only answers inside your
login session. Start the service from a terminal at the Mac. A service started from an SSH
session can't read the Keychain, so its usage reads come back empty. `rlx service start` warns
when you run it over SSH, and the `usage` part says `Keychain locked` in `rlx service status`.

The service that starts at login runs inside your login session, so its usage reads work. A
menu bar icon comes in a later release.

## Starting at login

The service starts when you log in, and it is on by default: the installers, and the first
`rlx init`, set it up. It starts **at login, not at boot**: on macOS the Keychain that holds your
Claude Code logins answers only inside your login session, and the usage reads need it.

- **macOS:** a LaunchAgent, `~/Library/LaunchAgents/studio.aisquare.rlx.service.plist`. It
  loads in your login session at the Mac only (`LimitLoadToSessionType` is `Aqua`), and launchd
  starts the service again if it crashes (`KeepAlive` on an unclean exit), never after a stop.
  It is not a LaunchDaemon, which would start at boot, outside any login session, where the
  Keychain does not answer. Its end never takes the fleet's tmux server with it
  (`AbandonProcessGroup`), and what it says before its own log opens goes to
  `~/Library/Logs/rlx/studio.aisquare.rlx.service.log`.
- **Linux:** a systemd user unit, `~/.config/systemd/user/rlx.service`, enabled for your user's
  `default.target`, and started again 5 seconds after a failure. A stop ends the service alone,
  never the fleet's tmux server and its agents (`KillMode=process`). It runs while you are logged
  in. On a headless server where it should run with nobody logged in, `rlx service install`
  asks whether to turn on systemd's lingering for your user (`loginctl enable-linger`). That is
  a system-wide setting, so it is never turned on without asking; `--linger` answers yes.
- **Windows:** the fleet runs in WSL, and so does the service: the same systemd user unit inside
  WSL, which needs systemd turned on there (`systemd=true` under `[boot]` in `/etc/wsl.conf`,
  the default for new installs). **A WSL distribution does not start when you sign in to
  Windows**, so the service starts when WSL does (when you open your WSL terminal, say), not at
  your Windows sign-in. A Windows logon task comes with rlxd in 0.10.3. On Windows itself,
  outside WSL, there is nothing to set up: `rlx service install` says so, and nothing is
  recorded.

The login item runs this install's `rlx` (the one uv put in `~/.local/bin`) as
`rlx service run --at-login`, with the PATH of the shell that installed it, and the variable that
names the home when one does; nothing else. From a checkout or a development virtual
environment, `rlx service install` refuses: a login item should not run that code.

`rlx service start`, and `rlx upgrade`'s restart, start the service through launchd or systemd
when its login item is set up, so the service that runs is the one they keep alive. `rlx service
stop` stops it as before: a stop is not a crash, so it stays stopped. If a service already runs
when you log in, the one at login exits quietly, and is not started again either.

### Which homes

One login item per home. Only the default home's is set up by itself. A home named by `RLX_HOME`
or `AISQUARE_HOME` (a scratch home, a test's) gets one only from `rlx service install` run with
that variable set: its item is `studio.aisquare.rlx.service.<8 hex>` (`rlx-<8 hex>.service` on
Linux), so two homes never share one. A variable that names the default home's own path, as the
captain's windows are given, still means the default home and its one item. `rlx init` sets nothing up under CI, and says so; the
installers still do, since installing is your act.

### Turning it off

Any of these turns it off:

- App settings ▸ User preferences ▸ *Start rlx at login*;
- `rlx service uninstall`;
- the OS's own startup settings: on macOS, System Settings ▸ General ▸ Login Items, under *Allow
  in the Background*; on Linux, `systemctl --user disable rlx.service`.

The switch and `rlx service uninstall` take the login item away and stop the service the OS
started from it. rlx remembers the choice, in `config.toml` (`[service] start_at_login`), and
never turns it back on by itself: `rlx init` and the installers set it up only while nothing has
been chosen, and otherwise only refresh a login item that is there (a new path to rlx, a new
PATH), never make one. So a removal in the OS's settings stays removed. Only `rlx service
install`, or the switch in App settings, adds it back; on macOS, an item turned off under Login
Items has to be turned on there, and `rlx service install` says so. `rlx init --no-service`, and
the installers' `--no-service`, leave it off and keep that: later runs of `rlx init` and the
installers never add it (a choice made before stays as it is). App settings' *Reset all settings*
and `rlx init --reinit` keep this choice: it is about a login item, not a setting of rlx's.
While `config.toml` cannot be read, `rlx init`, the installers, `rlx service install` and
`uninstall` and the switch leave the login item as it is (they add none, refresh none and take
none away) and say why; `rlx uninstall` and a move back to a release before the service still
take it away, as that release could not. `rlx uninstall` takes every
home's login item away too, and so does `rlx upgrade` to a release before the service (0.10.1
and earlier), which has no command for them to run and none to take them away: a choice of on is
forgotten with it, so a later release sets it up again, and a choice of off is kept. A reset or
`--reinit` of a `config.toml` that does not parse carries the choice over from its text, or,
when the text holds none, records it as the OS has it (an item in place is on, none is off).

`rlx service status` says whether it starts at login, and if not, why.

### Checking it by hand

The suite never runs the real `launchctl` or `systemctl`. Before a release, check it once on a
throwaway macOS user and on a Linux VM, never on a machine you work on:

1. Install rlx with the installer. `rlx service status` says `starts at login: yes`, and the
   service runs.
2. Log out and in again (on Linux, or reboot). With no rlx window open, `rlx service status`
   says it runs, and its `usage` part reads your accounts (on macOS: not `Keychain locked`).
3. Kill the service (`kill -9` its pid). Within a minute it runs again, with a new pid.
4. `rlx service stop`: it stays stopped (a stop is no crash). `rlx service start`: it runs
   again, started by launchd or systemd (`launchctl print gui/$(id -u)/studio.aisquare.rlx.service`
   or `systemctl --user status rlx.service` shows it running).
5. Start an agent from the TUI, then `rlx service restart`: the agent's tmux pane is still there.
6. Turn it off in the OS's settings (macOS: Login Items; Linux: `systemctl --user disable
   rlx.service`). `rlx service status` says it does not start at login, and why. Log out and in:
   it does not start. Run `rlx init` and the installer again: it is still off.
7. `rlx service install`: it is back on (on macOS after turning it on under Login Items, as it
   says), and runs after the next login.
8. App settings ▸ User preferences: turn *Start rlx at login* off: the service stops, and it does
   not start at the next login. Turn it back on: it runs again.
9. With `AISQUARE_HOME` set to a scratch folder, `rlx init`: no second login item appears.

## Where it runs

macOS and Linux. On Windows, the fleet runs in WSL, and so does the service. `rlx service`
refuses on native Windows.
