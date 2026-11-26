//! rlxd, the rlx engine: one local daemon per home that hosts processes and pushes events
//! to its clients (handoff 0.10.3, section 1).
//!
//! This is its core (1.3, 7.2, 7.4): `rlxd serve [--detach]`, `rlxd status [--json]`,
//! `rlxd stop` and `rlxd call`. One rlxd runs per home, on a local socket or named pipe,
//! and a client gets in with the shared token through `hello`. Every reply is the 7.2
//! envelope. The event bus, the PTY host, the runner and the rest are later items,
//! each a method on this socket. `rlxd here [--start]` is the probe rlx.app runs over
//! SSH: what is installed, whether rlxd runs, and the protocol versions it serves.
//!
//! Exit codes: 0 when the envelope printed is `ok`, 1 when it is not, 2 on a usage error,
//! 3 when rlxd is not running. `rlxd here` exits 3 and 4 (unsupported) with an `ok`
//! envelope: see its module.
//!
//! The binary is [`main`]. The library exists for rlxd's own tests: [`registry`], rlxd's
//! side of the daemon registry, is what `tests/registry_interop.rs` reads Python's
//! records with. Nothing else is public.

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

mod claude;
mod cli;
mod clock;
mod here;
mod host;
mod listener;
mod lock;
mod log;
mod out;
pub mod registry;
mod serve;
mod server;
mod token;
#[cfg(windows)]
mod windows;

/// The rlx engine: one local daemon per home.
#[derive(Debug, Parser)]
#[command(name = "rlxd", version, disable_help_subcommand = true)]
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
    },
    /// Print rlxd's status.
    Status {
        /// Print the status envelope, as `rlxd call status` does.
        #[arg(long)]
        json: bool,
    },
    /// Stop rlxd, and wait until it has stopped.
    Stop,
    /// Send one request on a new connection, and print the reply envelope.
    Call(CallOptions),
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
    /// The token to send, instead of the one in <home>/rlxd/token.
    #[arg(long, value_name = "T")]
    token: Option<String>,
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
        Action::Serve { detach: true, .. } => serve::detach(),
        Action::Serve { background, .. } => serve::serve(background),
        Action::Status { json } => cli::status(json),
        Action::Stop => cli::stop(),
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
            token: options.token,
            protocol_version: options.protocol_version,
            env: options.env,
            raw: options.raw,
            method: options.method,
            params: options.params,
        }),
    }
}
