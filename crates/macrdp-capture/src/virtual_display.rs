//! A virtual display sized to the RDP client.
//!
//! While it exists it's the Mac's main display and every other display mirrors
//! it (a headless Mac's placeholder display simply goes away), so the desktop
//! is exactly the client's size, 1:1.
//!
//! macOS reliably gives a process only one virtual display: once it's removed,
//! the next one the same process creates never gets a working mode. So each
//! display lives in a short-lived helper process (macrdp-server started with
//! `HELPER_ARG`). Dropping `VirtualDisplay` closes the helper's stdin, the
//! helper removes the display and exits. If macrdp dies, the helper sees its
//! stdin close and does the same, so a display can't be left behind.

use std::ffi::{c_char, c_void, CStr};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

extern "C" {
    fn macrdp_vd_create(w: u32, h: u32, error: *mut c_char, error_len: usize) -> *mut c_void;
    fn macrdp_vd_display_id(vd: *const c_void) -> u32;
    fn macrdp_vd_destroy(vd: *mut c_void);
}

/// First argument that starts macrdp-server as a virtual display helper
pub const HELPER_ARG: &str = "--virtual-display-helper";

/// How long to wait for the helper to bring the display online
const READY_TIMEOUT: Duration = Duration::from_secs(15);

pub struct VirtualDisplay {
    child: Child,
    stdin: Option<ChildStdin>,
    display_id: u32,
    width: u32,
    height: u32,
}

impl VirtualDisplay {
    /// Creates a `width` x `height` display (via a helper copy of the current
    /// executable) and makes it the main one. Blocks until it's online.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let exe = std::env::current_exe().context("can't find own executable")?;
        Self::spawn(&exe, width, height)
    }

    /// Like `new`, with an explicit helper executable (one that handles `HELPER_ARG`).
    pub fn spawn(helper: &Path, width: u32, height: u32) -> Result<Self> {
        let mut child = Command::new(helper)
            .args([HELPER_ARG, &width.to_string(), &height.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("can't start virtual display helper {}", helper.display()))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("helper has no stdout")?;

        // Read the helper's one-line answer on a thread so it can time out
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let line = match rx.recv_timeout(READY_TIMEOUT) {
            Ok(line) => line,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("virtual display helper didn't answer within {READY_TIMEOUT:?}");
            }
        };

        // From here on, dropping `vd` shuts the helper down
        let mut vd = Self { child, stdin, display_id: 0, width, height };
        match line.trim().split_once(' ') {
            Some(("ready", id)) => vd.display_id = id.parse().context("bad display id from helper")?,
            Some(("error", reason)) => return Err(anyhow!("virtual display {width}x{height}: {reason}")),
            _ => return Err(anyhow!("virtual display helper failed: {:?}", line.trim())),
        }
        tracing::info!(width, height, display_id = vd.display_id, main = vd.is_main(), "Virtual display created");
        Ok(vd)
    }

    pub fn display_id(&self) -> u32 {
        self.display_id
    }

    /// Whether macOS made it the main display (top-left at 0,0)
    pub fn is_main(&self) -> bool {
        core_graphics::display::CGDisplay::main().id == self.display_id
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        // Closing stdin tells the helper to remove the display and exit
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    tracing::warn!("Virtual display helper didn't exit; killing it");
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if self.display_id != 0 {
            tracing::info!(display_id = self.display_id, "Virtual display removed");
        }
    }
}

/// Entry point for the helper process (`HELPER_ARG <width> <height>`): creates
/// the display, prints `ready <id>` or `error <reason>`, then holds the display
/// until stdin closes.
pub fn run_helper(args: &[String]) -> ! {
    let size = (|| -> Option<(u32, u32)> { Some((args.first()?.parse().ok()?, args.get(1)?.parse().ok()?)) })();
    let mut out = std::io::stdout();
    let Some((w, h)) = size else {
        let _ = writeln!(out, "error usage: {HELPER_ARG} <width> <height>");
        std::process::exit(2);
    };
    let mut error = [0 as c_char; 256];
    let handle = unsafe { macrdp_vd_create(w, h, error.as_mut_ptr(), error.len()) };
    if handle.is_null() {
        let reason = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned();
        let _ = writeln!(out, "error {reason}");
        std::process::exit(1);
    }
    let _ = writeln!(out, "ready {}", unsafe { macrdp_vd_display_id(handle) });
    let _ = out.flush();

    // Hold the display until the parent closes our stdin (or dies)
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
    unsafe { macrdp_vd_destroy(handle) };
    std::process::exit(0);
}
