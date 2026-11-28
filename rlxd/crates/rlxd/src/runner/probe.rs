//! The connectors' one-turn probe (handoff 0.10.3, 5.3): `connectors.list {refresh: true,
//! allow_turn: true}`, while there is no route to the list without a turn.
//!
//! It is rlxd's own Claude Code, not a run: `run.list` doesn't show it, and no event of it
//! is published but the connector events its `system/init` makes. It runs with the user's
//! MCP servers loaded, no session, and one turn at most ([`argv::probe_args`]), in
//! `<home>/rlxd/connectors-probe/`. Each tool call it attempts is denied by rlxd itself:
//! its own settings' hook is `rlxd __probe-deny`, which opens no connection, so no call
//! reaches the app ([`argv::probe_settings`]).
//!
//! As a route-2 try does, it settles with stdin open, sends its prompt, and takes the
//! `system/init` that follows. Then stdin is closed, so the turn ends, and the tree is
//! ended once it has, or [`PROBE_END`] later. rlxd's log says when each probe ran, with
//! `model_turns: 1`, and holds neither its prompt nor what its `system/init` listed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rlxd_proto::{ErrorBody, codes};
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};

use super::process::Launch;
use super::stream::{self, Line};
use super::{Runner, argv, blocking, refusal, run_env};
use crate::claude;
use crate::connectors::{Account, Read};
use crate::log::{info, warning};

/// The probe's prompt. It asks for nothing that needs a tool.
const PROMPT: &str = "Reply with the single word OK.";

/// How long the probe may take to its `system/init`, past its settle.
const PROBE_INIT: Duration = Duration::from_secs(60);

/// How long the probe's turn has to end after its `system/init`, before it is ended.
const PROBE_END: Duration = Duration::from_secs(30);

impl Runner {
    /// Runs one probe on `account`'s environment and puts its `system/init` in the
    /// connectors' cache as `account`'s. One probe runs at a time (and no no-turn read
    /// beside it): a second waits for the first, then runs its own. Refused with
    /// `claude_not_found` when there is no Claude Code, or `probe_failed`, saying why.
    pub async fn probe_connectors(&self, account: &Account) -> Result<(), ErrorBody> {
        let _one = self.probing.lock().await;
        let Some(program) = claude::resolve().and_then(|p| std::path::absolute(p).ok()) else {
            return Err(refusal(
                codes::CLAUDE_NOT_FOUND,
                format!(
                    "Claude Code was not found: there is no `claude` on rlxd's PATH, and {} \
                     names none",
                    claude::BIN_ENV
                ),
            ));
        };
        let failed = |why: String| {
            refusal(
                codes::PROBE_FAILED,
                format!("the connectors probe failed: {why}"),
            )
        };
        let rlxd = std::env::current_exe()
            .map_err(|e| failed(format!("rlxd can't find its own path: {e}")))?;
        let settings = argv::probe_settings(&rlxd).map_err(failed)?;
        let dir = self.home.rlxd_dir().join("connectors-probe");
        let settings_file: PathBuf = dir.join("settings.json");
        blocking(|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            crate::files::write_private(&settings_file, &settings.to_string())
        })
        .map_err(|e| failed(format!("its folder could not be set up: {e}")))?;
        info!(
            "connectors: a one-turn probe of account {} starts (model_turns: 1)",
            account.slot
        );
        let mut env = run_env();
        env.extend(account.env());
        let launch = Launch {
            program: program.clone(),
            args: argv::probe_args(&settings_file),
            cwd: dir,
            env,
            unset: account.unset(),
        };
        let (mut child, tree) = self.spawner.spawn(launch).await.map_err(|e| {
            failed(format!(
                "Claude Code ({}) could not start: {e}",
                program.display()
            ))
        })?;
        let tree = Arc::new(tree);
        *self.probe_tree.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&tree));
        let read = self.probe_init(&mut child).await;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.shutdown().await;
        }
        if tree.reap(&mut child, Some(PROBE_END)).await.is_none() {
            tree.kill();
            tree.reap(&mut child, None).await;
        }
        *self.probe_tree.lock().unwrap_or_else(|p| p.into_inner()) = None;
        match read {
            Ok(init) => {
                blocking(|| {
                    self.connectors
                        .observe_on(account, init.observed(), Read::Init);
                });
                info!("connectors: the probe ended");
                Ok(())
            }
            Err(why) => {
                warning!("connectors: the probe failed: {why}");
                let error = failed(why);
                blocking(|| self.connectors.refresh_failed(account, error.clone()));
                Err(error)
            }
        }
    }

    /// The probe's `system/init`: after the settle, the prompt, then the first one read.
    async fn probe_init(&self, child: &mut tokio::process::Child) -> Result<stream::Init, String> {
        let (Some(stdin), Some(stdout)) = (child.stdin.as_mut(), child.stdout.take()) else {
            return Err("Claude Code's stdio was not piped".to_owned());
        };
        let mut stdout = BufReader::new(stdout);
        let mut line = Vec::new();
        let settled = tokio::time::Instant::now()
            + self.connectors.gate_times().settle
            + super::connector_gate::START_ALLOWANCE;
        // As on route 2: what Claude Code writes before the prompt is not the probe's.
        loop {
            tokio::select! {
                read = stream::next_line(&mut stdout, &mut line, stream::MAX_LINE) => match read {
                    Ok(Some(())) => {}
                    Ok(None) => return Err("Claude Code ended before its prompt".to_owned()),
                    Err(_) => return Err("Claude Code's output could not be read".to_owned()),
                },
                () = tokio::time::sleep_until(settled) => break,
            }
        }
        let prompt = json!({"type": "user", "message": {"role": "user",
            "content": [{"type": "text", "text": PROMPT}]}, "parent_tool_use_id": null});
        let mut first = prompt.to_string();
        first.push('\n');
        stdin
            .write_all(first.as_bytes())
            .await
            .and(stdin.flush().await)
            .map_err(|e| format!("its prompt could not be sent: {e}"))?;
        let until = Instant::now() + PROBE_INIT;
        loop {
            let read = tokio::select! {
                read = stream::next_line(&mut stdout, &mut line, stream::MAX_LINE) => read,
                () = tokio::time::sleep_until(until.into()) => {
                    return Err(format!("no system/init came in {} s", PROBE_INIT.as_secs()));
                }
            };
            match read {
                Ok(Some(())) => {}
                Ok(None) => return Err("Claude Code ended before its system/init".to_owned()),
                Err(_) => return Err("Claude Code's output could not be read".to_owned()),
            }
            for parsed in stream::parse(&line) {
                if let Line::Init(init) = parsed {
                    return Ok(init);
                }
            }
        }
    }
}

/// `rlxd __probe-deny`, the probe's hook: reads the call Claude Code hands it, and prints
/// rlxd's deny. Exits 0, as Claude Code reads a hook's JSON only then.
pub fn probe_deny() -> std::process::ExitCode {
    use std::io::{Read, Write};
    let mut call = Vec::new();
    // What the call was doesn't change the answer; a stdin that can't be read doesn't
    // either.
    let _ = std::io::stdin().take(1 << 20).read_to_end(&mut call);
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", argv::probe_deny_output()).and_then(|()| out.flush());
    std::process::ExitCode::SUCCESS
}
