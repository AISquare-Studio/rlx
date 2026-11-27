//! The probe scenarios (handoff 0.10.3, 7.4): what `claude --version` and `claude auth
//! status --json` answer, which is how rlxd's `status` reads Claude Code. A run started
//! as one of them plays `hello`.
//!
//! - `auth-status`: Claude Code 2.1.314, signed in with claude.ai as `probe@example.com`
//!   on the pro plan, as every other scenario answers too;
//! - `signed-out`: not signed in (`loggedIn: false`, exit 1, as Claude Code exits);
//! - `probe-hangs`: neither command answers for a minute;
//! - `auth-env`: signed in as `probe+<CLAUDE_CONFIG_DIR>@example.com`, so a test sees
//!   which environment the probe ran with.

use std::process::ExitCode;
use std::time::Duration;

use serde_json::json;

use crate::VERSION;

/// How long `probe-hangs` keeps a probe waiting.
const HANG: Duration = Duration::from_secs(60);

/// `claude --version`, as scenario `name` answers it.
pub fn version(name: &str) -> ExitCode {
    if name == "probe-hangs" {
        std::thread::sleep(HANG);
    }
    println!("{VERSION} (Claude Code)");
    ExitCode::SUCCESS
}

/// `claude auth status --json`, as scenario `name` answers it: the shape Claude Code
/// prints, with `probe@example.com` and nothing else of anyone's.
pub fn auth_status(name: &str) -> ExitCode {
    match name {
        "probe-hangs" => {
            std::thread::sleep(HANG);
        }
        "signed-out" => {
            println!(
                "{}",
                json!({"loggedIn": false, "authMethod": "none", "apiProvider": "firstParty"})
            );
            return ExitCode::from(1);
        }
        _ => {}
    }
    let email = if name == "auth-env" {
        let dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
        format!("probe+{dir}@example.com")
    } else {
        "probe@example.com".to_owned()
    };
    println!(
        "{}",
        json!({"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
            "email": email, "orgId": "00000000-0000-4000-8000-000000000000",
            "orgName": "Probe Org", "subscriptionType": "pro"})
    );
    ExitCode::SUCCESS
}
