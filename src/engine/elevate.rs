// SPDX-License-Identifier: GPL-3.0-or-later
//! Re-running one operation with administrator rights.
//!
//! Installing into `C:\Program Files\...` needs rights this process does not have and
//! cannot acquire: on Windows a running process cannot elevate itself. The only way is to
//! start a second one and let the OS ask. So that is what this does.
//!
//! What makes it cheap is that the second process is `echo-vrce-cli`, the command line
//! binary that ships beside this one. There is no purpose-built helper, no service, and no
//! second implementation of the update: the elevated run is the same command anyone could
//! type at a prompt, which also means it can be reproduced by hand when it goes wrong.
//!
//! The child writes to a log file the parent names, so the parent can follow along and show
//! what is happening rather than freezing behind an opaque wait.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum Error {
    /// The UAC prompt was dismissed. Not a failure worth an error dialog: the user said no.
    Declined,
    /// The elevated run started and finished, but reported a problem.
    Failed { code: i32 },
    Spawn(String),
    /// Elevation means nothing here.
    NotSupported,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Declined => write!(f, "the administrator prompt was dismissed"),
            Error::Failed { code } => {
                write!(f, "the elevated run did not succeed (exit code {code})")
            }
            Error::Spawn(m) => write!(f, "could not start an elevated copy: {m}"),
            Error::NotSupported => write!(f, "elevation is only available on Windows"),
        }
    }
}

impl std::error::Error for Error {}

/// The arguments that make the command line binary carry out one operation and then exit.
///
/// Built here rather than at each call site so the shape is one thing: the command, and a
/// log file the parent chose. `--quiet` because nobody is reading the child's console; the
/// log is the channel.
pub fn args_for(command: &[&str], log_path: &Path) -> Vec<String> {
    let mut args: Vec<String> = command.iter().map(|s| s.to_string()).collect();
    args.push("--quiet".into());
    args.push("--log".into());
    args.push(log_path.display().to_string());
    // Progress the parent can draw with, rather than sentences it would have to parse.
    args.push("--events".into());
    args
}

/// How the parent asks an elevated run to stop.
///
/// It cannot be a flag in memory: these are two processes, and the child is the one holding
/// administrator rights, so the parent cannot reach into it. It should not be killing the
/// process either - the child is elevated precisely because it is writing somewhere that
/// matters, and killing it mid-write is how that folder ends up broken.
///
/// So it is a file. The parent creates it, the child notices between chunks and stops the
/// way Ctrl+C stops it: partial download kept, nothing half-written.
pub fn cancel_path(log_path: &Path) -> PathBuf {
    log_path.with_extension("cancel")
}

/// Where an elevated run should write, given the normal log directory.
///
/// A fixed name rather than a timestamp: the parent has to know the path before the child
/// exists, and a name that says what it is beats a name that sorts well.
pub fn log_path(logs_dir: &Path) -> PathBuf {
    logs_dir.join("elevated.log")
}

/// Does this process already hold administrator rights?
///
/// Worth asking before offering: showing "run as administrator" to someone who already is
/// would be both useless and confusing, and the real problem would be something else.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        // TOKEN_ELEVATION is a single DWORD: non-zero means the token is elevated.
        let mut elevated: u32 = 0;
        let mut returned: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevated as *mut u32 as *mut std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        ok != 0 && elevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    // Being root is not the same question, and nothing here asks it.
    false
}

/// Starts an elevated copy of this executable, waits for it, and returns its exit code.
///
/// Blocking: the caller owns the thread. `SEE_MASK_NOCLOSEPROCESS` is the part that matters
/// - without it Windows closes the handle and there is nothing left to wait on, which is
/// why `ShellExecuteW` alone is not enough here even though it does show the prompt.
#[cfg(windows)]
pub fn run_elevated(args: &[String]) -> Result<i32, Error> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };

    let wide = |s: &std::ffi::OsStr| -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    };

    let exe = cli_binary()?;
    let file = wide(exe.as_os_str());
    let verb = wide(std::ffi::OsStr::new("runas"));
    let params = wide(std::ffi::OsStr::new(&quote_args(args)));

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0; // SW_HIDE: the child has no UI, and its log is the channel.

    let started = unsafe { ShellExecuteExW(&mut info) };
    if started == 0 {
        let err = std::io::Error::last_os_error();
        // Dismissing the prompt is an ordinary answer, not a fault to report as one.
        if err.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Err(Error::Declined);
        }
        return Err(Error::Spawn(err.to_string()));
    }
    if info.hProcess.is_null() {
        return Err(Error::Spawn("no process handle was returned".into()));
    }

    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code: u32 = 0;
        let got = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if got == 0 {
            return Err(Error::Spawn("the elevated run could not be waited on".into()));
        }
        Ok(code as i32)
    }
}

#[cfg(not(windows))]
pub fn run_elevated(_args: &[String]) -> Result<i32, Error> {
    Err(Error::NotSupported)
}

/// The command line binary that sits beside the window's.
///
/// Not this executable: the window is a GUI subsystem binary and has no command line mode
/// any more. They ship together, so a missing one means an incomplete install rather than
/// a bug, and saying so is more use than "could not start an elevated copy".
#[cfg(windows)]
fn cli_binary() -> Result<PathBuf, Error> {
    const NAME: &str = "echo-vrce-cli.exe";
    let here = std::env::current_exe().map_err(|e| Error::Spawn(e.to_string()))?;
    let beside = here.with_file_name(NAME);
    if beside.is_file() {
        return Ok(beside);
    }
    Err(Error::Spawn(format!(
        "{NAME} is not next to the installer. Both files belong in the same folder."
    )))
}

/// Joins arguments into one command line the way Windows will split it again.
///
/// `ShellExecuteExW` takes a single string, so anything containing a space - which install
/// paths reliably do - has to be quoted, and a literal quote or trailing backslash escaped.
/// The rules are the ones `CommandLineToArgvW` applies in reverse.
pub fn quote_args(args: &[String]) -> String {
    args.iter().map(|a| quote_one(a)).collect::<Vec<_>>().join(" ")
}

fn quote_one(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // Backslashes before a quote are doubled, then the quote is escaped.
                for _ in 0..=backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // A trailing backslash would escape the closing quote, so double them out.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stop_request_sits_beside_the_log_it_belongs_to() {
        // Derived from the log path rather than fixed, so two runs cannot stop each other,
        // and so the parent and the child agree without being told twice.
        let log = Path::new(r"C:\Users\me\AppData\Local\EchoVRCE\logs\elevated.log");
        let stop = cancel_path(log);
        assert_eq!(stop.parent(), log.parent(), "it has to be where the child will look");
        assert_eq!(stop.extension().and_then(|e| e.to_str()), Some("cancel"));
        assert_ne!(stop, log.to_path_buf(), "it must not be the log itself");
    }

    #[test]
    fn plain_arguments_are_left_alone() {
        assert_eq!(quote_args(&["update".into(), "-q".into()]), "update -q");
    }

    #[test]
    fn a_path_with_spaces_is_quoted() {
        let args = vec!["--path".to_string(), r"C:\Program Files\Echo VR".to_string()];
        assert_eq!(quote_args(&args), r#"--path "C:\Program Files\Echo VR""#);
    }

    #[test]
    fn a_trailing_backslash_does_not_escape_the_closing_quote() {
        // This is the one that silently swallows the next argument when it is wrong.
        let args = vec!["--path".to_string(), r"C:\Games\Echo VR\".to_string(), "-q".to_string()];
        let line = quote_args(&args);
        assert_eq!(line, r#"--path "C:\Games\Echo VR\\" -q"#);
        assert!(line.ends_with("-q"), "the following argument was swallowed: {line}");
    }

    #[test]
    fn embedded_quotes_survive() {
        let args = vec![r#"a"b"#.to_string()];
        assert_eq!(quote_args(&args), r#""a\"b""#);
    }

    #[test]
    fn an_empty_argument_stays_an_argument() {
        assert_eq!(quote_args(&[String::new()]), "\"\"");
    }

    #[test]
    fn the_command_line_says_where_to_log_and_to_stay_quiet() {
        let args = args_for(&["update", "--path", "D:\\Echo"], Path::new("D:\\logs\\elevated.log"));
        assert_eq!(
            args,
            vec![
                "update",
                "--path",
                "D:\\Echo",
                "--quiet",
                "--log",
                "D:\\logs\\elevated.log",
                "--events"
            ]
        );
    }
}
