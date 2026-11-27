//! The probe scenarios (handoff 0.10.3, 7.4): what `claude --version` and `claude auth
//! status --json` answer, which is how rlxd's `status` reads Claude Code. A run started
//! as one of them plays `hello`.
//!
//! - `auth-status`: Claude Code 2.1.314, signed in with claude.ai as `probe@example.com`
//!   on the pro plan, as every other scenario answers too;
//! - `signed-out`: not signed in (`loggedIn: false`, exit 1, as Claude Code exits);
//! - `probe-hangs`: neither command answers for a minute;
//! - `auth-env`: signed in as `probe+<CLAUDE_CONFIG_DIR>@example.com`, so a test sees
//!   which environment the probe ran with;
//! - `probe-slow`: `auth-status`, each command a second late: longer than `status`
//!   waits for a probe, as a real `claude auth status` can take;
//! - `probe-markers`: [`MARKER`] in every field `auth status --json` prints and on both
//!   commands' stderr, so a test can look for what the probe printed where rlxd writes.

use std::process::ExitCode;
use std::time::Duration;

use serde_json::json;

use crate::VERSION;

/// How long `probe-hangs` keeps a probe waiting.
const HANG: Duration = Duration::from_secs(60);

/// How late `probe-slow` answers.
const SLOW: Duration = Duration::from_secs(1);

/// What `probe-markers` prints in every field and on stderr: nothing rlxd writes may
/// hold it.
pub const MARKER: &str = "probe-output-marker-5d1f";

/// `claude --version`, as scenario `name` answers it.
pub fn version(name: &str) -> ExitCode {
    match name {
        "probe-hangs" => std::thread::sleep(HANG),
        "probe-slow" => std::thread::sleep(SLOW),
        "probe-markers" => {
            eprintln!("{MARKER}-version-stderr");
            println!("{VERSION} (Claude Code) {MARKER}-version");
            return ExitCode::SUCCESS;
        }
        _ => {}
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
        "probe-slow" => std::thread::sleep(SLOW),
        "probe-markers" => {
            eprintln!("{MARKER}-auth-stderr");
            let field = |name: &str| format!("{MARKER}-{name}");
            println!(
                "{}",
                json!({"loggedIn": true, "authMethod": field("authMethod"),
                    "apiProvider": field("apiProvider"),
                    "email": format!("probe+{MARKER}@example.com"), "orgId": field("orgId"),
                    "orgName": field("orgName"), "subscriptionType": field("subscriptionType"),
                    "configDirectory": field("configDirectory"),
                    "loginExpiresAt": field("loginExpiresAt"), "planTier": field("planTier")})
            );
            return ExitCode::SUCCESS;
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
