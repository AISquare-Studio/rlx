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

    // Without the token, the client home has none to read.
    let run = client.rlxd(&["call", "--socket", &endpoint, "status"]);
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert_eq!(run.error_code(), "unauthorized");
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
    assert_eq!(run.code(), Some(1), "{run:?}");
    assert!(run.stdout.is_empty(), "{run:?}");
    assert!(run.stderr.contains("cannot read the token"), "{run:?}");
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
    let as_app = |extra: &[&str]| {
        let mut args = vec!["call", "--socket", &endpoint, "--token", &token];
        args.extend_from_slice(extra);
        client.rlxd(&args)
    };
    for extra in [
        &["--as-app", "status"][..],
        &["--role", "app", "--gate-credential", "made-up", "status"],
        &["gate.pair"],
    ] {
        let run = as_app(extra);
        assert_eq!(run.code(), Some(1), "{extra:?}: {run:?}");
        assert_eq!(run.error_code(), "not_gate", "{extra:?}");
    }
    assert!(!served.gate_json().exists(), "nothing was paired");
    assert_eq!(client.test_credential(), None);

    // In dev mode, the test client pairs with that rlxd, keeps the credential in its own
    // home, and is the gate there.
    served.stop();
    served.start_dev();
    let run = as_app(&["--as-app", "status"]);
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
    assert!(client.test_credential().is_some());
    assert!(
        !client.gate_json().exists(),
        "the client home has no gate of its own"
    );
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
        /// The home `below.root()/<name>`.
        fn below(below: TestHome, name: &str) -> Self {
            let root = below.root().join(name);
            std::fs::create_dir_all(&root).unwrap();
            Self { below, root }
        }

        /// A home whose `run/rlxd.sock`, absolute, is `len` bytes long.
        fn whose_socket_is(len: usize) -> Self {
            let below = TestHome::new();
            let base = std::path::absolute(below.root()).unwrap();
            let taken = base.join("x").join("run").join("rlxd.sock");
            let name = "h".repeat(len - (taken.as_os_str().len() - 1));
            let home = Self::below(below, &name);
            let socket = std::path::absolute(home.layout().socket_in_home()).unwrap();
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
