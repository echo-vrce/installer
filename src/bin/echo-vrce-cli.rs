// SPDX-License-Identifier: GPL-3.0-or-later
//! The command line, in a binary a shell will actually wait for.
//!
//! The window's executable cannot also be this one. It is a GUI subsystem binary, because
//! double-clicking it must not flash a console, and PowerShell reports no exit code for
//! those: `$LASTEXITCODE` comes back empty, so the documented exit codes would be invisible
//! to the scripts they are for. The subsystem is a field in the PE header, decided at link
//! time, so no runtime cleverness fixes it inside one file.
//!
//! So there are two binaries sharing every line of code, and this one is twenty of them.
//! Node and VS Code ship two executables on Windows for the same reason.

use echo_vrce_installer::{cli, engine::selfupdate, os};

fn main() {
    os::quiet_hard_error_dialogs();
    // Whichever of the two binaries starts first clears the previous version away, because
    // somebody who only ever uses the command line should not be left with the leftovers of
    // an update forever.
    selfupdate::sweep_previous();
    cli::prepare_console();
    cli::catch_interrupt();
    cli::restore_sigpipe();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(cli::run(&argv));
}
