//! ticker, the counter rlxd's PTY tests run: `tick <n>`, a line every 200 ms, until its
//! terminal goes. Native, so a test waits on rlxd and not on a program's start: Windows
//! PowerShell, the counter before, took past 10 s to start on a loaded runner (#2794).
//! Only rlxd's tests build it (the `fake-claude` feature).

use std::io::Write;
use std::time::Duration;

fn main() {
    let mut stdout = std::io::stdout();
    for n in 1_u64.. {
        if writeln!(stdout, "tick {n}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
