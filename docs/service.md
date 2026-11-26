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

Starting the service at login, and a menu bar icon, come in a later release.

## Where it runs

macOS and Linux. On Windows, the fleet runs in WSL, and so does the service. `rlx service`
refuses on native Windows.
