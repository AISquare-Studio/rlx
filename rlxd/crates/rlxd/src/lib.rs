//! rlxd, the rlx engine: one local daemon per home that hosts processes and pushes events
//! to its clients (handoff 0.10.3, section 1).
//!
//! This is its core (1.3, 7.2, 7.4) and its event bus (1.7, 7.3): `rlxd serve [--detach]
//! [--gate-dev-mode]`, `rlxd status [--json]`, `rlxd stop`, `rlxd call` and `rlxd
//! events`. One rlxd runs per home, on a local socket or named pipe, and a client gets in
//! with the shared token through `hello`. Every reply is the 7.2 envelope, and
//! `events.subscribe` streams the home's events. `settings.get`, `settings.set` and
//! `settings.list` serve the app settings every device shares. rlxd keeps one resident
//! Python brain running, `rlx brain --stdio` (`brain`), and `hook` hands it the Claude
//! Code hooks rlxd-hook sends, publishing `hook.*` on the bus (`hooks`). `run.start`,
//! `run.cancel`, `run.resume`, `run.get`, `run.list` and `run.events` drive the headless
//! runner and its durable log (2.1, 5.4, 7.5), and `space.ensure` and `space.erase` the
//! spaces its runs work in (2.5). The PTY host and the rest are later items, each a method on this
//! socket, publishing on the bus.
//! `rlxd here [--start]` is the probe rlx.app runs over SSH: what is installed, whether
//! rlxd runs, and the protocol versions it serves.
//!
//! The app gate (5.1) is here too: only rlx's app holds it, proven by a per-device gate
//! credential and, for a device paired locally, by the peer's identity ([`peer`]). The
//! user manages the paired devices with `rlxd gate devices`, `rlxd gate pair` and `rlxd
//! gate unpair`.
//!
//! Exit codes: 0 when the envelope printed is `ok`, 1 when it is not, 2 on a usage error,
//! 3 when rlxd is not running. `rlxd here` exits 3 and 4 (unsupported) with an `ok`
//! envelope: see its module. `rlxd events` exits 4 when its `--timeout` runs out.
//!
//! The binary is [`main`]. The library exists for rlxd's own tests. [`registry`], rlxd's
//! side of the daemon registry, is what `tests/registry_interop.rs` reads Python's
//! records with, [`peer`]'s compiled-in identity is what the gate's peer tests sign
//! their helpers against, and [`bus`] is the event bus the later items publish on, run
//! events included. Nothing else is public.
//!
//! The cargo feature `debug-methods` adds the socket's test-only methods, such as
//! `debug.emit`. Only rlxd's own tests turn it on; no release build has it.

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

/// rlxd/THIRD_PARTY_NOTICES: tmux's copyright and ISC permission notice for each file
/// ported from it, which every rlxd binary carries and `rlxd licenses` prints.
pub const THIRD_PARTY_NOTICES: &str = include_str!("../../../THIRD_PARTY_NOTICES");

mod attach;
mod brain;
pub mod bus;
mod claude;
mod cli;
mod clock;
mod config;
mod connector_map;
mod connectors;
mod durable;
mod events;
mod files;
mod gate;
mod here;
mod hooks;
mod host;
mod listener;
mod lock;
mod log;
mod out;
pub mod peer;
pub mod pty;
pub mod registry;
mod runner;
pub(crate) mod scheduler;
mod serve;
mod server;
mod settings;
mod spaces;
mod spawn;
#[cfg(feature = "debug-methods")]
mod test_hook;
mod token;
pub mod vt;
#[cfg(windows)]
mod windows;

/// What `rlxd --version` says after `rlxd`: the version, and for a build with the
/// `gate-dev-mode` feature, a marker saying so. A release build says the version alone.
#[cfg(feature = "gate-dev-mode")]
const VERSION_TEXT: &str = concat!(env!("CARGO_PKG_VERSION"), " (dev: gate-dev-mode)");
#[cfg(not(feature = "gate-dev-mode"))]
const VERSION_TEXT: &str = env!("CARGO_PKG_VERSION");

/// The development-only cargo features this build has, for `status.data.dev_features`.
#[cfg(feature = "gate-dev-mode")]
pub(crate) fn dev_features() -> Vec<String> {
    vec!["gate-dev-mode".to_owned()]
}

/// A release build has no development-only feature.
#[cfg(not(feature = "gate-dev-mode"))]
pub(crate) fn dev_features() -> Vec<String> {
    Vec::new()
}

/// The rlx engine: one local daemon per home.
#[derive(Debug, Parser)]
#[command(name = "rlxd", version = VERSION_TEXT, disable_help_subcommand = true)]
struct Command {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Run rlxd for this home: RLX_HOME, else AISQUARE_HOME, else ~/.aisquare.
    Serve {
        /// Start it in the background, and return once it takes connections.
        #[arg(long)]
        detach: bool,
        /// Started by --detach: log to the file only.
        #[arg(long, hide = true)]
        background: bool,
        /// Gate dev mode, for unsigned dev and CI builds of rlx's app: skip the gate
        /// peer's identity check. The gate credential is still required. Never use it
        /// otherwise. Only a build with the gate-dev-mode cargo feature has it; a
        /// release build has no such flag.
        #[cfg_attr(feature = "gate-dev-mode", arg(long))]
        #[cfg_attr(not(feature = "gate-dev-mode"), arg(skip))]
        gate_dev_mode: bool,
    },
    /// Print rlxd's status.
    Status {
        /// Print the status envelope, as `rlxd call status` does.
        #[arg(long)]
        json: bool,
    },
    /// Stop rlxd, and wait until it has stopped.
    Stop,
    // Boxed: its options are the largest variant, more so on Windows, whose paths are
    // wider (clippy::large_enum_variant).
    /// Send one request on a new connection, and print the reply envelope. A stream's
    /// events print one per line instead, and it exits 0 when the stream ends.
    Call(Box<CallOptions>),
    /// The devices paired as the app gate: list, approve or unpair them.
    #[command(subcommand)]
    Gate(GateAction),
    /// Print the home's events as JSON lines: those rlxd holds after --since, then with
    /// --follow every later one. Exits 0 at --until or --limit, 4 at --timeout.
    // Boxed, as Call is.
    Events(Box<EventsOptions>),
    /// Put a PTY in this terminal, as `tmux attach` puts a pane in it. Ctrl-] detaches.
    Attach {
        /// The PTY, as `pty.spawn` named it and `pty.list` lists it.
        pty_id: String,
        /// Watch only: send no keys, and leave its size alone.
        #[arg(long)]
        read_only: bool,
    },
    /// On Windows, the process a PTY's ConPTY starts: joins the PTY's job, then runs the
    /// program in it. rlxd starts it; nothing else should.
    #[cfg(windows)]
    #[command(name = "__pty-shim", hide = true)]
    PtyShim {
        /// The job's name.
        #[arg(long)]
        job: String,
        /// The program and its arguments.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Runs one run's Claude Code and ends its whole process group with it (macOS and
    /// Linux). rlxd starts it; nobody else does.
    #[command(name = "__run-guard", hide = true)]
    RunGuard {
        /// Claude Code and its arguments, after `--`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            num_args = 1..,
            value_parser = clap::value_parser!(std::ffi::OsString)
        )]
        argv: Vec<std::ffi::OsString>,
    },
    /// The connectors' probe's hook (handoff 0.10.3, 5.3): denies the tool call Claude
    /// Code hands it, by rlxd, and opens no connection. rlxd's probe names it; nobody
    /// else does.
    #[command(name = "__probe-deny", hide = true)]
    ProbeDeny,
    /// Test builds only: a run's hook, as rlxd-hook's will be: asks rlxd's gate about the
    /// call on stdin (`debug.pre_tool_use`), and prints its answer, or a deny.
    #[cfg(feature = "debug-methods")]
    #[command(name = "__test-hook", hide = true)]
    TestHook {
        /// The hook's event: `pre-tool-use`.
        event: String,
        /// The run.
        #[arg(long)]
        run: String,
        /// The run's attempt.
        #[arg(long)]
        attempt: u64,
        /// The run's `deadline_ms`, as rlxd-hook's command line carries it: unused here.
        #[arg(long)]
        deadline_ms: Option<u64>,
    },
    /// Print the notices of what rlxd carries from others: tmux's ISC licence, for the
    /// parts of rlxd ported from tmux.
    Licenses,
    /// Say what rlx is installed here and whether its rlxd runs: rlx.app's probe over SSH.
    /// Exits 0 running, 3 installed but not running, 4 unsupported.
    Here {
        /// Print one JSON envelope.
        #[arg(long)]
        json: bool,
        /// Start rlxd when it is installed but not running.
        #[arg(long)]
        start: bool,
        /// Put the client token in the answer (auth.token). It is a secret.
        #[arg(long)]
        show_token: bool,
        /// The protocol versions the caller speaks, such as 1, 1-3 or 1,2. Exits 4 when
        /// rlxd serves none of them. Default: this rlxd's.
        #[arg(long, value_name = "VERSIONS")]
        protocols: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum GateAction {
    /// List the paired devices, with each one's route and when it was last seen.
    Devices,
    /// List the waiting pairing requests, or answer one, on this host. Without
    /// --request-id it lists them (id, device, the asking process and its binary) and
    /// approves nothing.
    Pair {
        /// The request to approve, as the list names it.
        #[arg(long, value_name = "ID")]
        request_id: Option<String>,
        /// Deny the request instead.
        #[arg(long, requires = "request_id")]
        deny: bool,
        /// Approve a request for the name of a device you unpaired.
        #[arg(long, requires = "request_id", conflicts_with = "deny")]
        force: bool,
    },
    /// Unpair a device, or every device, and drop its app connections.
    Unpair {
        /// The device to unpair. Without it, every device is unpaired.
        device: Option<String>,
    },
}

#[derive(Debug, Args)]
struct EventsOptions {
    /// Say hello as the app (role app), with the test gate credential when there is one.
    #[arg(long)]
    as_app: bool,
    /// Only these topics, comma-separated, such as run,needs_you. Every topic otherwise.
    #[arg(long, value_name = "A,B")]
    topics: Option<String>,
    /// First print every event rlxd holds after this seq: 0 for all of them.
    #[arg(long, value_name = "SEQ")]
    since: Option<u64>,
    /// Then go on printing events as they happen, until rlxd stops.
    #[arg(long)]
    follow: bool,
    /// Exit 0 once an event of one of these types has printed, comma-separated. It
    /// implies --follow.
    #[arg(long, value_name = "TYPE[,TYPE...]")]
    until: Option<String>,
    /// Exit 0 once this many events have printed.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    limit: Option<u64>,
    /// Exit 4 when this many seconds have passed first.
    #[arg(long, value_name = "S")]
    timeout: Option<f64>,
    /// Connect to this socket (a named pipe on Windows) instead of the home's, such as
    /// the local end of `ssh -N -L <PATH>:<host's socket> host`. Needs --token-file or
    /// --token, and with --as-app --gate-credential-file: this home's are never sent.
    #[arg(long, value_name = "PATH")]
    socket: Option<std::path::PathBuf>,
    /// The token to send, instead of the one in <home>/rlxd/token. Prefer --token-file:
    /// a command line is visible to other users.
    #[arg(long, value_name = "T")]
    token: Option<String>,
    /// Read the token to send from this file.
    #[arg(long, value_name = "F", conflicts_with = "token")]
    token_file: Option<std::path::PathBuf>,
    /// Where --as-app finds its gate credential, instead of <home>/test-gate-credential.
    #[arg(long, value_name = "F")]
    gate_credential_file: Option<std::path::PathBuf>,
}

#[derive(Debug, Args)]
struct CallOptions {
    /// Say hello as the app (role app), with the test gate credential when there is one.
    #[arg(long, conflicts_with = "role")]
    as_app: bool,
    /// The hello's role: app, tui, remote or cli (the default).
    #[arg(long, value_name = "R")]
    role: Option<String>,
    /// The hello's client name.
    #[arg(long, value_name = "NAME")]
    client: Option<String>,
    /// The hello's gate credential.
    #[arg(long, value_name = "C")]
    gate_credential: Option<String>,
    /// The token to send, instead of the one in <home>/rlxd/token. Prefer --token-file:
    /// a command line is visible to other users.
    #[arg(long, value_name = "T")]
    token: Option<String>,
    /// Read the token to send from this file.
    #[arg(long, value_name = "F", conflicts_with = "token")]
    token_file: Option<std::path::PathBuf>,
    /// Connect to this socket (a named pipe on Windows) instead of the home's, such as
    /// the local end of `ssh -N -L <PATH>:<host's socket> host`. Needs --token-file or
    /// --token, and with --as-app --gate-credential-file: this home's are never sent.
    #[arg(long, value_name = "PATH")]
    socket: Option<std::path::PathBuf>,
    /// Where --as-app keeps its gate credential, instead of <home>/test-gate-credential.
    #[arg(long, value_name = "F", conflicts_with = "gate_credential")]
    gate_credential_file: Option<std::path::PathBuf>,
    /// The hello's protocol version.
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    protocol_version: Option<String>,
    /// The hello's env, a JSON object.
    #[arg(long, value_name = "JSON")]
    env: Option<String>,
    /// Send stdin's frames as they are, with no hello, and print each reply.
    #[arg(long, conflicts_with_all = ["method", "params"])]
    raw: bool,
    /// The method to call, such as status.
    #[arg(required_unless_present = "raw")]
    method: Option<String>,
    /// Its params, a JSON object.
    #[arg(value_name = "PARAMS_JSON")]
    params: Option<String>,
}

/// `rlxd`, as its command line says.
pub fn main() -> ExitCode {
    match Command::parse().action {
        Action::Serve {
            detach: true,
            gate_dev_mode,
            ..
        } => serve::detach(gate_dev_mode),
        Action::Serve {
            background,
            gate_dev_mode,
            ..
        } => serve::serve(background, gate_dev_mode),
        Action::Status { json } => cli::status(json),
        Action::Stop => cli::stop(),
        Action::ProbeDeny => runner::probe_deny(),
        Action::Licenses => {
            print!("{THIRD_PARTY_NOTICES}");
            ExitCode::SUCCESS
        }
        Action::Here {
            json,
            start,
            show_token,
            protocols,
        } => here::here(here::HereArgs {
            json,
            start,
            show_token,
            protocols,
        }),
        Action::Call(options) => cli::call(cli::CallArgs {
            as_app: options.as_app,
            role: options.role,
            client: options.client,
            gate_credential: options.gate_credential,
            target: cli::Target {
                socket: options.socket,
                token: options.token,
                token_file: options.token_file,
                gate_credential_file: options.gate_credential_file,
            },
            protocol_version: options.protocol_version,
            env: options.env,
            raw: options.raw,
            method: options.method,
            params: options.params,
        }),
        Action::Gate(GateAction::Devices) => cli::gate_devices(),
        Action::Gate(GateAction::Pair {
            request_id,
            deny,
            force,
        }) => cli::gate_pair(request_id, !deny, force),
        Action::Gate(GateAction::Unpair { device }) => cli::gate_unpair(device),
        #[cfg(windows)]
        Action::PtyShim { job, argv } => pty::shim(&job, &argv),
        Action::Attach { pty_id, read_only } => {
            attach::attach(attach::AttachArgs { pty_id, read_only })
        }
        #[cfg(feature = "debug-methods")]
        Action::TestHook {
            event,
            run,
            attempt,
            deadline_ms: _,
        } => test_hook::main(&event, &run, attempt),
        #[cfg(unix)]
        Action::RunGuard { argv } => runner::guard::main(&argv),
        #[cfg(not(unix))]
        Action::RunGuard { .. } => ExitCode::from(2),
        Action::Events(options) => events::events(events::EventsArgs {
            as_app: options.as_app,
            topics: options.topics,
            since: options.since,
            follow: options.follow,
            until: options.until,
            limit: options.limit,
            timeout: options.timeout,
            target: cli::Target {
                socket: options.socket,
                token: options.token,
                token_file: options.token_file,
                gate_credential_file: options.gate_credential_file,
            },
        }),
    }
}
