//! fake-claude, the stand-in for Claude Code in the runner's tests: see the `fake-claude`
//! crate. Only rlxd's tests build it (the `fake-claude` feature).

fn main() -> std::process::ExitCode {
    fake_claude::main()
}
