//! Where clients connect, beyond the home's own socket: `rlxd call --socket`, as through
//! the local end of an SSH forward, and the short socket a home nested too deep for
//! `<home>/run/rlxd.sock` gets (the remote brief's direct-streamlocal task, acceptance 2).

mod common;

use common::TestHome;
use rlxd_client::Home;

fn text(value: &serde_json::Value) -> &str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("not a string: {value}"))
}

#[test]
fn call_socket_reaches_an_rlxd_from_another_home_with_its_token() {
    let served = TestHome::new();
    served.start();
    let endpoint = Home::at(served.root()).endpoint();
    let token = std::fs::read_to_string(Home::at(served.root()).token()).unwrap();
    // Another home, with no rlxd and no token of its own, as an SSH client is.
    let client = TestHome::new();
    let run = client.rlxd(&[
        "call",
        "--socket",
        &endpoint,
        "--token",
        token.trim(),
        "status",
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(text(&run.data()["paths"]["socket"]), endpoint);
    assert_eq!(
        text(&run.data()["paths"]["home"]),
        served.root().display().to_string()
    );

    // The token from a file, out of the command line.
    let token_file = client.root().join("host-token");
    std::fs::write(&token_file, &token).unwrap();
    let token_file = token_file.display().to_string();
    let run = client.rlxd(&[
        "call",
        "--socket",
        &endpoint,
        "--token-file",
        &token_file,
        "status",
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
}

#[test]
fn socket_never_sends_this_homes_token_or_test_credential() {
    // A home with an rlxd, a token and a kept test credential: --socket to another rlxd
    // must use none of them, and keep nothing from it.
    let local = TestHome::new();
    local.start_dev();
    let run = local.rlxd(&["call", "--as-app", "status"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let kept = local.test_credential().unwrap();
    let served = TestHome::new();
    served.start_dev();
    let endpoint = Home::at(served.root()).endpoint();
    for args in [
        &["call", "--socket", &endpoint, "status"][..],
        &["call", "--socket", &endpoint, "--as-app", "status"],
        &[
            "events", "--socket", &endpoint, "--since", "0", "--limit", "1",
        ],
    ] {
        let run = local.rlxd(args);
        assert_eq!(run.code(), Some(2), "a usage error: {args:?}: {run:?}");
        assert!(run.stdout.is_empty(), "{run:?}");
        assert!(
            run.stderr.contains("--socket needs --token-file"),
            "{run:?}"
        );
    }
    let token = served.token();
    for args in [
        &[
            "call", "--socket", &endpoint, "--token", &token, "--as-app", "status",
        ][..],
        &[
            "events", "--socket", &endpoint, "--token", &token, "--as-app", "--since", "0",
        ],
    ] {
        let run = local.rlxd(args);
        assert_eq!(run.code(), Some(2), "a usage error: {args:?}: {run:?}");
        assert!(run.stderr.contains("--gate-credential-file"), "{run:?}");
    }
    assert!(
        !served.gate_json().exists(),
        "nothing reached the other rlxd's gate"
    );
    assert_eq!(local.test_credential().as_deref(), Some(kept.as_str()));
    assert_eq!(local.app_clients(), 0);
}

#[test]
fn events_socket_streams_from_an_rlxd_of_another_home() {
    let served = TestHome::new();
    let pid = served.start()["pid"].clone();
    let endpoint = Home::at(served.root()).endpoint();
    let token = std::fs::read_to_string(Home::at(served.root()).token()).unwrap();
    let client = TestHome::new();
    let run = client.rlxd(&[
        "events",
        "--socket",
        &endpoint,
        "--token",
        token.trim(),
        "--since",
        "0",
        "--until",
        "daemon.started",
        "--timeout",
        "30",
    ]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let lines = run.lines();
    let started = lines.last().unwrap();
    common::assert_valid("event.json", started);
    assert_eq!(started["type"], "daemon.started", "{run:?}");
    assert_eq!(started["data"]["pid"], pid);

    let run = client.rlxd(&["events", "--socket", &endpoint, "--since", "0"]);
    assert_eq!(run.code(), Some(2), "{run:?}");
    assert!(run.stdout.is_empty(), "{run:?}");
    assert!(
        run.stderr.contains("--socket needs --token-file"),
        "{run:?}"
    );
}

#[test]
fn the_gate_checks_a_connection_through_call_socket_as_any_other() {
    // Outside dev mode, the unsigned test client can't pair or be the gate through
    // --socket either: pairing and the gate hello go to the rlxd at --socket.
    let served = TestHome::new();
    served.start();
    let endpoint = Home::at(served.root()).endpoint();
    let token = served.token();
    let client = TestHome::new();
    // Where the client keeps its credential for that rlxd: not its home's.
    let kept = client.root().join("served-credential");
    let kept_arg = kept.display().to_string();
    let as_app = |extra: &[&str]| {
        let mut args = vec!["call", "--socket", &endpoint, "--token", &token];
        args.extend_from_slice(extra);
        client.rlxd(&args)
    };
    for extra in [
        &["--as-app", "--gate-credential-file", &kept_arg, "status"][..],
        &["--role", "app", "--gate-credential", "made-up", "status"],
        &["gate.pair"],
    ] {
        let run = as_app(extra);
        assert_eq!(run.code(), Some(1), "{extra:?}: {run:?}");
        assert_eq!(run.error_code(), "not_gate", "{extra:?}");
    }
    assert!(!served.gate_json().exists(), "nothing was paired");
    assert!(!kept.exists());

    // In dev mode, the test client pairs with that rlxd, keeps the credential in the file
    // it named, and is the gate there. Its home keeps nothing of that rlxd's.
    served.stop();
    served.start_dev();
    let run = as_app(&["--as-app", "--gate-credential-file", &kept_arg, "status"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    assert_eq!(
        run.data()["clients"]["app"],
        1,
        "the asking app counts itself"
    );
    assert!(
        served.gate_json().exists(),
        "paired with the rlxd at --socket"
    );
    assert!(!std::fs::read_to_string(&kept).unwrap().trim().is_empty());
    assert_eq!(
        client.test_credential(),
        None,
        "nothing kept in the client's home"
    );
    assert!(
        !client.gate_json().exists(),
        "the client home has no gate of its own"
    );
    // The kept credential serves the next call, with no pairing.
    let run = as_app(&["--as-app", "--gate-credential-file", &kept_arg, "status"]);
    assert_eq!(run.code(), Some(0), "{run:?}");
    let run = as_app(&["--role", "app", "--gate-credential", "made-up", "status"]);
    assert_eq!(run.error_code(), "not_gate");
}

#[test]
fn call_socket_where_nothing_listens_is_not_running() {
    let home = TestHome::new();
    let nowhere = if cfg!(windows) {
        r"\\.\pipe\rlxd-no-such-pipe".to_owned()
    } else {
        home.root().join("nothing.sock").display().to_string()
    };
    let run = home.rlxd(&["call", "--socket", &nowhere, "--token", "t", "status"]);
    assert_eq!(run.code(), Some(3), "{run:?}");
    assert_eq!(run.error_code(), "not_running");
    let message = text(&run.json()["error"]["message"]).to_owned();
    assert!(message.contains(&nowhere), "{message}");
    let hint = text(&run.json()["error"]["hint"]).to_owned();
    assert!(
        hint.contains("rlxd here --start"),
        "not this home's rlxd: {hint}"
    );
}

#[test]
fn a_missing_token_file_is_unauthorized_whatever_this_home_runs() {
    // Review #2460: "not running" for a missing token is for the home's own token file
    // only, as no rlxd may ever have run here. A --token-file that is missing, with
    // --socket or without, says so as it is, from rlxd call and rlxd events alike, and
    // whether or not this home runs an rlxd (review #2498).
    let served = TestHome::new();
    served.start();
    let endpoint = Home::at(served.root()).endpoint();
    let client = TestHome::new();
    let missing = client.root().join("no-such-token").display().to_string();
    let nowhere = client.root().join("nothing.sock").display().to_string();
    for home in [&client, &served] {
        for socket in [Some(endpoint.as_str()), Some(nowhere.as_str()), None] {
            let mut call = vec!["call"];
            let mut events = vec!["events"];
            if let Some(socket) = socket {
                call.extend(["--socket", socket]);
                events.extend(["--socket", socket]);
            }
            call.extend(["--token-file", &missing, "status"]);
            events.extend(["--token-file", &missing, "--since", "0"]);
            let run = home.rlxd(&call);
            assert_eq!(run.code(), Some(1), "{call:?}: {run:?}");
            assert_eq!(run.error_code(), "unauthorized", "{call:?}");
            let message = text(&run.json()["error"]["message"]).to_owned();
            assert!(message.contains("cannot read the token"), "{message}");
            let run = home.rlxd(&events);
            assert_eq!(run.code(), Some(1), "{events:?}: {run:?}");
            assert_eq!(run.stderr_code(), "unauthorized", "{events:?}");
            assert!(run.stdout.is_empty(), "{run:?}");
        }
    }
    // The home's own token, missing with no rlxd here: not running, as before.
    let run = client.rlxd(&["call", "status"]);
    assert_eq!(run.code(), Some(3), "{run:?}");
    assert_eq!(run.error_code(), "not_running");
    let run = client.rlxd(&["events", "--since", "0"]);
    assert_eq!(run.code(), Some(3), "{run:?}");
    assert_eq!(run.stderr_code(), "not_running");
}

#[cfg(unix)]
#[test]
fn a_forward_whose_host_runs_no_rlxd_says_to_start_it_there() {
    // ssh takes every connection to a forward's local socket, then closes it when sshd
    // can't reach the host's socket, as when no rlxd runs there. A stand-in that does
    // the same: every connection taken, then dropped unanswered.
    let client = TestHome::new();
    let forward = client.root().join("fwd.sock");
    let listener = std::os::unix::net::UnixListener::bind(&forward).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    let forward = forward.display().to_string();
    let run = client.rlxd(&["call", "--socket", &forward, "--token", "t", "status"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "disconnected");
    let message = text(&run.json()["error"]["message"]).to_owned();
    assert_eq!(
        message,
        format!("the connection through {forward} closed without a whole reply to hello")
    );
    let hint = text(&run.json()["error"]["hint"]).to_owned();
    for cause in [
        "host's rlxd",
        "rlxd here --start",
        "sshd",
        "AllowTcpForwarding",
    ] {
        assert!(hint.contains(cause), "{cause}: {hint}");
    }
    assert!(
        hint.contains("host's rlxd") && hint.contains("rlxd here --start"),
        "{hint}"
    );
    let run = client.rlxd(&[
        "events", "--socket", &forward, "--token", "t", "--since", "0",
    ]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.stderr_code(), "disconnected");
    assert!(run.stderr.contains("rlxd here --start"), "{run:?}");
}

#[cfg(unix)]
#[test]
fn a_close_after_hello_was_answered_is_no_forward_hint() {
    // Review #2498: once rlxd has answered hello on this connection, a close is rlxd's
    // own (a panic, a stop mid-call), not a host with no rlxd: a plain disconnected.
    use std::io::{BufRead, BufReader, Write};
    let client = TestHome::new();
    let forward = client.root().join("answers.sock");
    let listener = std::os::unix::net::UnixListener::bind(&forward).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let id = serde_json::from_str::<serde_json::Value>(&line)
                .map_or(serde_json::json!(1), |hello| hello["id"].clone());
            let reply = serde_json::json!({"schema_version": 1, "id": id, "ok": true, "data": {}});
            let _ = writeln!(stream, "{reply}");
            // Then gone, before the next request's reply.
        }
    });
    let forward = forward.display().to_string();
    let run = client.rlxd(&["call", "--socket", &forward, "--token", "t", "status"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "disconnected");
    assert!(
        run.json()["error"].get("hint").is_none(),
        "no forward hint: {run:?}"
    );
    let message = text(&run.json()["error"]["message"]).to_owned();
    assert!(!message.contains("reply to hello"), "{message}");
    let run = client.rlxd(&[
        "events", "--socket", &forward, "--token", "t", "--since", "0",
    ]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert!(!run.stderr.contains("rlxd here --start"), "{run:?}");

    // --as-app pairs first: its hello is answered, then gate.pair's connection closes.
    // That is rlxd's own close too, and no credential is kept.
    let kept = client.root().join("kept-credential");
    let kept_arg = kept.display().to_string();
    let run = client.rlxd(&[
        "call",
        "--socket",
        &forward,
        "--token",
        "t",
        "--as-app",
        "--gate-credential-file",
        &kept_arg,
        "status",
    ]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "disconnected");
    assert!(
        run.json()["error"].get("hint").is_none(),
        "no forward hint after gate.pair's hello: {run:?}"
    );
    assert!(!kept.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn a_home_whose_path_is_not_utf8_reaches_its_rlxd() {
    // Linux takes any bytes in a name (APFS refuses these). Clients connect to the
    // socket's path as bytes, not its display string, so call and stop reach the rlxd,
    // and serve --detach sees the rlxd it started. Once in the home, once too deep.
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::process::Stdio;

    let below = TestHome::new();
    let deep = [
        b"\xff".as_slice(),
        "d".repeat(rlxd_client::SOCKET_PATH_MAX).as_bytes(),
    ]
    .concat();
    for name in [b"caf\xe9-\xff".as_slice(), deep.as_slice()] {
        let root = below.root().join(OsStr::from_bytes(name));
        std::fs::create_dir_all(&root).unwrap();
        let rlxd = |args: &[&str]| {
            let mut command = below.command(args);
            command.env("AISQUARE_HOME", &root).stdin(Stdio::null());
            common::run(command, None)
        };
        let run = rlxd(&["serve", "--detach"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        let pid = run.data()["pid"].clone();
        let run = rlxd(&["call", "status"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        assert_eq!(run.data()["pid"], pid);
        let run = rlxd(&["stop"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        let run = rlxd(&["status", "--json"]);
        assert_eq!(run.code(), Some(3), "{run:?}");
    }
}

/// A home nested too deep for its own socket (macOS and Linux; Windows has a pipe).
#[cfg(unix)]
mod deep {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    use rlxd_client::{Home, SOCKET_PATH_MAX};

    use super::common::{self, Run, TestHome};
    use super::text;

    /// A home at `root`, below a [`TestHome`] that removes it: rlxd's commands run with
    /// `AISQUARE_HOME` naming it, and dropping it stops its rlxd.
    struct HomeAt {
        below: TestHome,
        root: PathBuf,
    }

    impl HomeAt {
        /// The home `<below.root(), resolved>/<name>`.
        fn below(below: TestHome, name: &str) -> Self {
            let root = std::fs::canonicalize(below.root()).unwrap().join(name);
            std::fs::create_dir_all(&root).unwrap();
            Self { below, root }
        }

        /// A home whose `run/rlxd.sock`, absolute, is `len` bytes long.
        fn whose_socket_is(len: usize) -> Self {
            let below = TestHome::new();
            let base = std::fs::canonicalize(below.root()).unwrap();
            let taken = base.join("x").join("run").join("rlxd.sock");
            let name = "h".repeat(len - (taken.as_os_str().len() - 1));
            let home = Self::below(below, &name);
            let socket = home.layout().socket_in_home();
            assert_eq!(socket.as_os_str().len(), len, "{}", socket.display());
            home
        }

        fn layout(&self) -> Home {
            Home::at(&self.root)
        }

        fn rlxd(&self, args: &[&str]) -> Run {
            let mut command = self.below.command(args);
            command
                .env("AISQUARE_HOME", &self.root)
                .stdin(Stdio::null());
            common::run(command, None)
        }
    }

    impl Drop for HomeAt {
        fn drop(&mut self) {
            let _ = self.rlxd(&["stop"]);
        }
    }

    #[test]
    fn a_home_too_deep_for_its_socket_is_served_on_the_short_one() {
        let home = HomeAt::whose_socket_is(SOCKET_PATH_MAX + 1);
        let layout = home.layout();
        assert!(layout.uses_short_socket());
        let short = layout.short_socket();
        let shown = short.display().to_string();

        // It starts, on the short socket, and says so.
        let run = home.rlxd(&["serve", "--detach"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        assert_eq!(text(&run.data()["paths"]["socket"]), shown);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&short), 0o600);
        assert_eq!(mode(&Home::short_socket_dir()), 0o700);
        assert!(
            !layout.socket_in_home().exists(),
            "nothing in the home's run/"
        );

        // Every client finds it: call, status and here.
        let run = home.rlxd(&["call", "status"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        common::assert_valid("status-reply.json", &run.data());
        assert_eq!(text(&run.data()["paths"]["socket"]), shown);
        let run = home.rlxd(&["status"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        assert!(run.stdout.contains(&format!("socket: {shown}")), "{run:?}");
        let run = home.rlxd(&["here", "--json"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        assert_eq!(run.data()["running"], true);
        assert_eq!(text(&run.data()["socket"]), shown);
        common::assert_valid("here-reply.json", &run.data());

        // A stop removes the socket, and leaves the folder other homes may share.
        let run = home.rlxd(&["stop"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        assert!(!short.exists());
        assert!(Home::short_socket_dir().is_dir());
        let run = home.rlxd(&["here", "--json"]);
        assert_eq!(run.code(), Some(3), "{run:?}");
        assert_eq!(text(&run.data()["socket"]), shown, "where it will listen");
    }

    #[test]
    fn the_gate_holds_on_the_short_socket() {
        let home = HomeAt::whose_socket_is(SOCKET_PATH_MAX + 1);
        assert!(home.layout().uses_short_socket());
        let gate_json = home.layout().gate();
        let run = home.rlxd(&["serve", "--detach"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        assert_eq!(run.data()["gate"], "signed app only");
        for args in [
            &["call", "--as-app", "status"][..],
            &[
                "call",
                "--role",
                "app",
                "--gate-credential",
                "made-up",
                "status",
            ],
            &["call", "gate.pair"],
        ] {
            let run = home.rlxd(args);
            assert_eq!(run.code(), Some(1), "{args:?}: {run:?}");
            assert_eq!(run.error_code(), "not_gate", "{args:?}");
        }
        assert!(!gate_json.exists(), "nothing was paired");

        // Dev mode skips only the peer check: pairing and the credential still hold.
        assert_eq!(home.rlxd(&["stop"]).code(), Some(0));
        let run = home.rlxd(&["serve", "--detach", "--gate-dev-mode"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        let run = home.rlxd(&["call", "--as-app", "status"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        assert_eq!(run.data()["clients"]["app"], 1);
        assert_eq!(
            text(&run.data()["paths"]["socket"]),
            home.layout().short_socket().display().to_string()
        );
        assert!(gate_json.exists());
        let run = home.rlxd(&[
            "call",
            "--role",
            "app",
            "--gate-credential",
            "made-up",
            "status",
        ]);
        assert_eq!(run.error_code(), "not_gate");
    }

    /// `rlxd args…` with `AISQUARE_HOME` spelled `root`.
    fn rlxd_as(home: &HomeAt, root: &Path, args: &[&str]) -> Run {
        let mut command = home.below.command(args);
        command.env("AISQUARE_HOME", root).stdin(Stdio::null());
        common::run(command, None)
    }

    #[test]
    fn every_spelling_of_a_home_reaches_its_one_rlxd() {
        // The review's repro: a home whose own socket fits, and a link to it whose
        // spelling would not. Both spellings decide on the resolved path, so they agree.
        let home = HomeAt::below(TestHome::new(), "h");
        let link = home.root.with_file_name("l".repeat(100));
        std::os::unix::fs::symlink(&home.root, &link).unwrap();
        assert!(link.join("run/rlxd.sock").as_os_str().len() > SOCKET_PATH_MAX);
        let socket = home.layout().socket_in_home().display().to_string();
        for (start, ask) in [(&link, &home.root), (&home.root, &link)] {
            let run = rlxd_as(&home, start, &["serve", "--detach"]);
            assert_eq!(run.code(), Some(0), "{run:?}");
            assert_eq!(text(&run.data()["paths"]["socket"]), socket);
            let run = rlxd_as(&home, ask, &["call", "status"]);
            assert_eq!(run.code(), Some(0), "{run:?}");
            assert_eq!(text(&run.data()["paths"]["socket"]), socket);
            let run = rlxd_as(&home, ask, &["here", "--json"]);
            assert_eq!(run.code(), Some(0), "{run:?}");
            assert_eq!(text(&run.data()["socket"]), socket);
            let run = rlxd_as(&home, ask, &["stop"]);
            assert_eq!(run.code(), Some(0), "{run:?}");
        }
    }

    #[test]
    fn a_home_whose_socket_just_fits_keeps_it() {
        // The OS's real limit, at its edge: SOCKET_PATH_MAX bytes bind.
        let home = HomeAt::whose_socket_is(SOCKET_PATH_MAX);
        let layout = home.layout();
        assert!(!layout.uses_short_socket());
        let run = home.rlxd(&["serve", "--detach"]);
        assert_eq!(run.code(), Some(0), "{run:?}");
        assert_eq!(
            text(&run.data()["paths"]["socket"]),
            layout.socket_in_home().display().to_string()
        );
        assert!(layout.socket_in_home().exists());
        assert!(!layout.short_socket().exists());
    }
}
