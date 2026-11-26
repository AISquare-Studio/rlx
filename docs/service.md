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
```

`rlx service status --json` prints the same as one object: `running`, `pid`, `version`,
`this_version`, `version_mismatch`, `host`, `started_at`, `uptime_s` and `parts`, one entry per
part with its `state` (`ok`, `off`, `waiting` or `error`), a `detail` line and `last_run`. It
exits 3 when no service runs.

There is one service per home (`~/.aisquare`). Its status is in `~/.aisquare/service/`, and
its log in `~/.aisquare/log/service.log`, which rolls over at 1 MB with one old file kept.

`rlx upgrade` stops the service before it replaces rlx and starts it again on the new release.
`rlx uninstall` stops it. Neither refuses to run beside it, as they do beside an rlx window.

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

Starting the service at login comes in a later release. The tray icon is below: it is in
the menu bar only in your login session too.

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
upgrade (`rlx upgrade`, or Update in the app) of an install made before it. With uv by hand:
`uv tool install 'aisquare-cli[tray]'`. Without it, the service runs as before, and `rlx service
status` says the tray needs the extra.

**How it runs:** in a process of its own, which the service starts, restarts after a crash (at
most five times per service start) and stops with itself. The service, the rlx commands and the
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
