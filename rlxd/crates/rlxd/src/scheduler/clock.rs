//! The scheduler's clock: the system's, or in test builds a manual one.
//!
//! A test build started with `RLXD_TEST_CLOCK=<RFC 3339 instant>` keeps the scheduler's
//! time there, and only `debug.clock_advance` and `debug.clock_set` move it, so every
//! scheduling rule has a fast, exact test. A release build has neither the variable nor
//! the methods.

use std::sync::Mutex;

use jiff::Timestamp;

/// The scheduler's time.
pub struct Clock {
    manual: Option<Mutex<Timestamp>>,
}

impl Clock {
    /// The system's clock; in a test build, a manual one when `RLXD_TEST_CLOCK` names an
    /// instant.
    pub fn from_env() -> Self {
        Self {
            manual: test_start().map(Mutex::new),
        }
    }

    /// Now.
    pub fn now(&self) -> Timestamp {
        match &self.manual {
            Some(at) => *at.lock().unwrap_or_else(|p| p.into_inner()),
            None => Timestamp::now(),
        }
    }

    /// Whether it is a manual clock, which only the debug methods move.
    pub fn is_manual(&self) -> bool {
        self.manual.is_some()
    }

    /// Sets a manual clock to `at`, which may not be before it: the time it is at then.
    #[cfg_attr(not(feature = "debug-methods"), allow(dead_code))]
    pub fn set(&self, at: Timestamp) -> Result<Timestamp, String> {
        let Some(manual) = &self.manual else {
            return Err("the scheduler runs on the system's clock: start rlxd with \
                        RLXD_TEST_CLOCK=<RFC 3339 instant> for a test clock"
                .to_owned());
        };
        let mut now = manual.lock().unwrap_or_else(|p| p.into_inner());
        if at < *now {
            return Err(format!("the test clock goes forward only: it is at {now}"));
        }
        *now = at;
        Ok(at)
    }
}

#[cfg(feature = "debug-methods")]
fn test_start() -> Option<Timestamp> {
    let text = std::env::var("RLXD_TEST_CLOCK").ok()?;
    match text.parse() {
        Ok(at) => Some(at),
        Err(e) => {
            crate::log::warning!("RLXD_TEST_CLOCK {text:?} is not an instant ({e}); ignored");
            None
        }
    }
}

#[cfg(not(feature = "debug-methods"))]
fn test_start() -> Option<Timestamp> {
    None
}
