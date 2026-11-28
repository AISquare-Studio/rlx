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

Remote is experimental. While it is off in Settings' *Experimental*, the service doesn't load
it, and `rlx service status` says `Remote is experimental and off`. Turned on there, the
service starts it within two seconds if its switch is on. Turned off, the service stops a
Remote it serves within two seconds and keeps its switch, so Remote comes back when it's on
again.

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

The service that starts at login runs inside your login session, so its usage reads work, and
its tray icon (below) shows in the menu bar.

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
when its login item is set up, so the service that runs is the one they keep alive (on macOS, a
login item launchd has not loaded yet is loaded, which starts it). `rlx service
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
and earlier), which has no command for them to run and none to take them away: a choice of on
whose item was in place and on is forgotten with it, so a later release sets it up again; a choice
of off, or of on whose item you removed or turned off in the OS, is kept. If that upgrade's
installer fails, or installs a release that has the service after all, the items are put back
as they were (but one whose file you could not read, which is taken away all the same: `rlx
service status` then says how it stands). Putting them back starts nothing: each starts at your
next login, and the upgrade starts the service again only if it was running before. A reset (App settings' *Reset all*, `rlx config reset`) or `--reinit` of a
`config.toml` that does not parse carries the choice over from its text, or, when the text holds
none or cannot be read (`chmod 000`), records the default home's as the OS has it (an item in
place is on, none is off). `--reinit --yes` writes its defaults readable, never at a mode that
keeps you out.

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

## The tray

While the service runs, rlx's backslash `\` sits in the macOS menu bar, or in the Linux tray,
in rlx's blue. Its menu:

- **Open rlx** opens rlx in a terminal. On macOS that's the app your Mac opens `.command`
  files with (Terminal, unless you changed it), or the app you pick in settings. On Linux it's
  `xdg-terminal-exec`, then `$TERMINAL`, then the first common terminal it finds.
- **Restart rlx** restarts the service.
- **Quit rlx…** stops the service, after asking. Your agents keep running in tmux. The question
  names them, and says what they lose while the service is stopped: hand-overs at their limits,
  and Remote.
- Two lines say how many agents are running and how many accounts have room.

**Opening rlx with a double-click.** On Linux, activating the icon opens rlx (a double-click, or
a click, depending on your desktop); the menu is its right-click menu. macOS has no double-click
for a menu bar icon: one with a menu opens it on any click. So on macOS, **Open rlx** is the
menu's first item.

**Settings:** App settings ▸ User preferences ▸ Tray.

- Show the icon or not.
- The icon: the backslash, the whole `r\x` mark, or the backslash as a template, which macOS
  paints like its other menu bar icons. On Linux the template shows the blue backslash.
- The terminal **Open rlx** uses.

A running service follows a change within half a minute.

**Where it shows:**

- macOS, in your login session. A service started over SSH has no menu bar, so it shows no
  icon.
- Linux, where the desktop has a StatusNotifierItem host: KDE Plasma has one, and GNOME has one
  with the AppIndicator extension. Without one there's no icon, and `rlx service status` says so.
- Not on Windows in this release, native or in WSL.

**What it needs:** the `tray` extra. On macOS and Linux the installer adds it, and so does an
upgrade (`rlx upgrade`, or Update in the app) of a uv install from PyPI made before it. A pipx,
pip or uvx install, one from a checkout, a folder or git, and one from another index keep the
extras they had: add it by hand, with uv `uv tool install 'aisquare-cli[tray]'`. Without it,
the service runs as before, and `rlx service status` says the tray needs the extra.

**How it runs:** in a process of its own, which the service starts, starts again after a crash
(it starts it at most five times per service start) and stops with itself. After the fifth
crash `rlx service status` shows the `tray` part as an error until the service starts again. The service, the rlx commands and the
hooks import nothing of the tray. `rlx service status` lists it as the `tray` part.

### Idle cost

Measured on macOS for 600 s, after a 30 s settle. Two services ran side by side, each in a
scratch home under `env -i`, with a guard that refused the Keychain tool, ngrok and the network.
One had the tray switched off. The other had it on, with its process drawing nothing: no tray may
be started on the machine that measured it. CPU is `ps` CPU time, RSS is `ps` at the end, and
wakeups and context switches are `top`'s.

| | CPU | RSS | idle wakeups | context switches |
|---|---|---|---|---|
| The service, tray off | 0.42 s = 0.070% | 84 MB | 0 | 141/min |
| The service, tray on | 0.43 s = 0.072% | 84 MB | 0 | 141/min |
| The tray's process (drawing nothing) | 0.00 s | 27 MB | 0 | 2/min |

With a fleet: eight live agents on two tmux servers, in each home (also 600 s, side by side):

| | CPU | RSS | idle wakeups | context switches |
|---|---|---|---|---|
| The service, tray off | 0.41 s = 0.068% | 84 MB | 0 | 127/min |
| The service, tray on | 0.53 s = 0.088% | 83 MB | 0 | 159/min |
| The tray's process (drawing nothing) | 0.00 s | 27 MB | 0 | 2/min |

- Without a fleet, the service is unchanged by the tray. With one, the tray's part lists the
  running agents every 30 s, for the menu: one read-only `tmux display-message` for each server,
  40 in the 600 s here. That costs about 0.02% CPU and 30 context switches a minute, and no idle
  wakeup.
- The tray's process is about 27 MB before the OS's part. Importing AppKit (macOS) takes it to
  about 49 MB; jeepney (Linux) adds almost nothing.
- The real menu bar's and tray's numbers are the manual check's (step 7).

**Budget:** the service's own budget is unchanged (CPU under 0.5%, RSS under 120 MB, under 150
wakeups a minute). The tray's process is held to CPU under 0.1% and under 5 wakeups a minute,
and to RSS under 60 MB on macOS and under 35 MB on Linux.

### Manual check (owner step)

On a throwaway macOS user, and on a Linux VM with KDE and one with GNOME. Never on a machine
whose fleet is live.

1. Install with the installer, which adds the `tray` extra. Start the service.
2. Within a few seconds the blue `\` shows in the menu bar or the tray.
3. On macOS, click it: **Open rlx** is first, and opens your terminal running rlx. On Linux,
   activate the icon: rlx opens.
4. **Quit rlx…**: the question names the running agents. Cancel keeps the service; Quit stops
   it. The icon goes, `rlx service status` says it isn't running, and the agents' tmux panes are
   still there.
5. In App settings ▸ User preferences ▸ Tray, pick the template icon: it redraws within half a
   minute. Switch it off: it goes.
6. GNOME without the AppIndicator extension: no icon, and `rlx service status` says the session
   has no place for a tray icon.
7. Note the tray process's memory and CPU, and its idle wakeups, over ten minutes (`ps` and
   `top`, as in the table above), with the real menu bar or tray this time.

## Where it runs

macOS and Linux. On Windows, the fleet runs in WSL, and so does the service. `rlx service`
refuses on native Windows.
