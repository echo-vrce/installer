// SPDX-License-Identifier: GPL-3.0-or-later
//! Process-wide OS behaviour that has to be set before anything else runs.

/// Stop Windows putting its own modal dialogs in front of this process.
///
/// The installer's whole design is that the user picks paths by hand, so it has to expect
/// to be pointed at the wrong file. Probing one is a `CreateProcess`, and when that fails
/// on a file that is not a valid executable, Windows does not simply return an error: it
/// puts up a hard-error box ("Unsupported 16-Bit Application") owned by this process. The
/// window behind it stops responding to the mouse, and the only screen that could correct
/// the setting is the one now unreachable. A wrong choice in a file picker locked the app.
///
/// The dialog is a property of the process error mode, which is inherited rather than
/// chosen: launched from PowerShell it never appeared, because PowerShell had already
/// turned it off, and launched by double-click it always did. Setting it here makes the
/// behaviour the same however the app was started, and turns the failure back into what it
/// should have been all along - an error value the code above already knows how to report.
///
/// `SEM_NOGPFAULTERRORBOX` is deliberately *not* set: a crash of our own should still be
/// visible to the user and to Windows Error Reporting.
pub fn quiet_hard_error_dialogs() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX,
        };
        // Read-modify-write: SetErrorMode returns the previous mask, and replacing it
        // outright would clear flags something else in the process had set on purpose.
        let previous = SetErrorMode(0);
        SetErrorMode(previous | SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX);
    }
}
