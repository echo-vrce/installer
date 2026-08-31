// SPDX-License-Identifier: GPL-3.0-or-later
//! The engines behind the UI: manifests, hashing, downloads, extraction.
//!
//! Nothing in here knows about egui, and nothing in here touches a Windows-only API, so
//! all of it is testable on a Linux box with no VM and no headset. That is deliberate:
//! this is where correctness bugs live, and correctness is the cheapest thing to test.

pub mod adb;
pub mod download;
pub mod elevate;
pub mod hash;
pub mod install;
pub mod manifest;
pub mod meta;
pub mod patch;
pub mod path_input;
pub mod pc_install;
pub mod pc_patch;
pub mod quest;
pub mod revive;
pub mod selfupdate;
pub mod quest_install;
pub mod quest_update;
pub mod tools;
pub mod unzip;
pub mod update;
pub mod watch;

#[cfg(test)]
pub(crate) mod testserver;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation, shared between the UI and whatever worker thread is running.
///
/// Checked between chunks rather than interrupting a syscall, so a cancel takes effect
/// within one buffer read. Cloning gives another handle to the same flag.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Clears the flag so the same handle can drive a retry.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Stops a spawned console program from flashing a window on Windows.
///
/// A GUI subsystem process has no console, so every `Command::spawn` of a console program -
/// and adb is polled every couple of seconds while a window is open - makes Windows create
/// one, show it, and tear it down. The result is a black box blinking on screen forever,
/// which is what the original installer does and what this was supposed to avoid.
///
/// `CREATE_NO_WINDOW` is the documented way to say "run it, do not give it a console of its
/// own". Output still arrives: it is piped, not drawn.
pub fn hide_console(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
