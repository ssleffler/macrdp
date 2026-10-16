//! Keeps the display awake for remote sessions.
//!
//! While the display is off macOS draws nothing, so screen capture produces no
//! frames and the client sees black. Injected input doesn't count as user
//! activity either, so a remote user can't wake it. These helpers do that via
//! IOKit power assertions.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};

type IOPMAssertionID = u32;
const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;
/// kIOPMUserActiveLocal: a local user is active; turns the display on if it is off
const K_IOPM_USER_ACTIVE_LOCAL: u32 = 0;
const K_IO_RETURN_SUCCESS: i32 = 0;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        level: u32,
        name: CFStringRef,
        id: *mut IOPMAssertionID,
    ) -> i32;
    fn IOPMAssertionDeclareUserActivity(
        name: CFStringRef,
        user_type: u32,
        id: *mut IOPMAssertionID,
    ) -> i32;
    fn IOPMAssertionRelease(id: IOPMAssertionID) -> i32;
}

/// Shortest gap between user-activity declarations from remote input
const ACTIVITY_INTERVAL: Duration = Duration::from_secs(5);

/// The process-wide user-activity assertion and when it was last renewed
static ACTIVITY: Mutex<Option<(IOPMAssertionID, Instant)>> = Mutex::new(None);

/// Tells macOS a user is active: wakes the display and resets the idle timer
/// (screen saver, display sleep, auto-lock). Cheap to call often; renews at
/// most every few seconds.
pub fn declare_user_activity() {
    declare_activity(false);
}

fn declare_activity(force: bool) {
    let mut state = ACTIVITY.lock().unwrap();
    if let Some((_, last)) = *state {
        if !force && last.elapsed() < ACTIVITY_INTERVAL {
            return;
        }
    }
    let name = CFString::new("macrdp remote session");
    let mut id = state.map(|(id, _)| id).unwrap_or(0);
    let rc = unsafe {
        IOPMAssertionDeclareUserActivity(name.as_concrete_TypeRef(), K_IOPM_USER_ACTIVE_LOCAL, &mut id)
    };
    if rc == K_IO_RETURN_SUCCESS {
        *state = Some((id, Instant::now()));
    } else {
        tracing::warn!(rc, "Failed to declare user activity (display may stay asleep)");
    }
}

/// When remote input last declared user activity (within ACTIVITY_INTERVAL)
pub fn last_user_activity() -> Option<Instant> {
    ACTIVITY.lock().unwrap().map(|(_, at)| at)
}

/// Keeps the display from sleeping while it is alive.
pub struct DisplayAwake(IOPMAssertionID);

impl DisplayAwake {
    /// Wakes the display and keeps it on until dropped.
    pub fn new() -> Option<Self> {
        declare_activity(true);
        let kind = CFString::new("PreventUserIdleDisplaySleep");
        let name = CFString::new("macrdp remote session");
        let mut id: IOPMAssertionID = 0;
        let rc = unsafe {
            IOPMAssertionCreateWithName(
                kind.as_concrete_TypeRef(),
                K_IOPM_ASSERTION_LEVEL_ON,
                name.as_concrete_TypeRef(),
                &mut id,
            )
        };
        if rc != K_IO_RETURN_SUCCESS {
            tracing::warn!(rc, "Failed to keep the display awake for this session");
            return None;
        }
        tracing::info!("Display wake requested; keeping it awake while the client is connected");
        Some(Self(id))
    }
}

impl Drop for DisplayAwake {
    fn drop(&mut self) {
        unsafe { IOPMAssertionRelease(self.0) };
        tracing::info!("Client gone; display may sleep again");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wakes the real display: `cargo test -p macrdp-input -- --ignored`
    #[test]
    #[ignore]
    fn holds_display_awake_while_alive() {
        let assertions = || {
            let out = std::process::Command::new("pmset").args(["-g", "assertions"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let awake = DisplayAwake::new().expect("assertion not created");
        let held = assertions();
        assert!(held.contains("PreventUserIdleDisplaySleep") && held.contains("macrdp remote session"));
        std::thread::sleep(Duration::from_secs(3));
        drop(awake);
        let after = assertions();
        assert!(!after.lines().any(|l| l.contains("PreventUserIdleDisplaySleep") && l.contains("macrdp remote session")));
    }
}
