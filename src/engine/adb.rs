// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding and driving adb.
//!
//! Every invocation goes through `std::process::Command` with separate arguments. That is
//! not stylistic. The original builds command *strings* and hands them to Java's
//! `Runtime.exec(String)`, which re-splits on whitespace and ignores quotes, so any path
//! containing a space is silently torn in half; its own source carries a comment warning
//! about exactly that. An argv is never re-parsed by anything.
//!
//! adb is also never assumed to exist. It is located, reported, and can be fetched on
//! request, because "adb is not installed" is a fixable condition rather than a dead end.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config;
use crate::engine::download::{self, Spec};
use crate::engine::{unzip, Cancel};

/// Where Google publishes platform-tools. Roughly 8 MB, which is small enough that
/// fetching on request beats shipping a copy and keeping it current.
pub const PLATFORM_TOOLS_BASE: &str = "https://dl.google.com/android/repository/platform-tools-latest-";

/// The directory Google's archive unpacks into. Named once, because the installer has to
/// reach inside the archive to swap it into place rather than unpack over the live copy.
const PLATFORM_TOOLS_DIR: &str = "platform-tools";

/// How adb was found. Shown in the dependency panel, because "which adb is this?" is the
/// first question when something behaves oddly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A path the user chose. Always wins.
    Configured,
    /// A copy this app downloaded and unpacked.
    Managed,
    /// Found on PATH.
    OnPath,
}

impl Source {
    /// One phrase for "where did this adb come from", which is the first question when
    /// something behaves oddly.
    pub fn describe(self) -> &'static str {
        match self {
            Source::Configured => "chosen by you",
            Source::Managed => "downloaded by this installer",
            Source::OnPath => "found on PATH",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub path: PathBuf,
    pub source: Source,
    /// Reported by `adb version`, or None when it could not be run at all.
    pub version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Connected and authorised: usable.
    Ready,
    /// Connected, but the headset has not granted this computer USB debugging.
    Unauthorized,
    Offline,
    Unknown,
}

impl State {
    fn parse(raw: &str) -> State {
        match raw {
            "device" => State::Ready,
            "unauthorized" => State::Unauthorized,
            "offline" => State::Offline,
            _ => State::Unknown,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            State::Ready => "ready",
            State::Unauthorized => "not authorised",
            State::Offline => "offline",
            State::Unknown => "unrecognised state",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    pub state: State,
    /// From `-l`, when adb offers it. Lets the UI say "Quest 3" rather than a serial.
    pub model: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    NotFound,
    Launch(std::io::Error),
    /// adb ran but said no.
    Failed { code: Option<i32>, output: String },
    /// adb was still running when its time was up, and was killed.
    TimedOut,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "adb was not found"),
            Error::Launch(e) => write!(f, "could not run adb: {e}"),
            Error::TimedOut => write!(
                f,
                "adb stopped responding. This usually means the cable or the port is \
                 unreliable; reseat it and try again."
            ),
            Error::Failed { code, output } => match code {
                Some(c) => write!(f, "adb exited with code {c}: {}", output.trim()),
                None => write!(f, "adb was terminated: {}", output.trim()),
            },
        }
    }
}

impl std::error::Error for Error {}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "adb.exe"
    } else {
        "adb"
    }
}

/// The copy this app manages, whether or not it exists yet.
pub fn managed_path() -> PathBuf {
    config::tools_dir().join(PLATFORM_TOOLS_DIR).join(exe_name())
}

/// Finds an adb to use, in priority order: the user's choice, then a managed copy, then
/// PATH. Returns None only when there is nothing to run at all.
pub fn locate(configured: Option<&Path>) -> Option<Located> {
    if let Some(p) = configured {
        if p.is_file() {
            return Some(describe(p.to_path_buf(), Source::Configured));
        }
    }
    let managed = managed_path();
    if managed.is_file() {
        return Some(describe(managed, Source::Managed));
    }
    if let Some(p) = on_path() {
        return Some(describe(p, Source::OnPath));
    }
    None
}

fn describe(path: PathBuf, source: Source) -> Located {
    let version = Adb { path: path.clone() }.version();
    Located { path, source, version }
}

fn on_path() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(exe_name()))
        .find(|candidate| candidate.is_file())
}

pub struct Adb {
    pub path: PathBuf,
}

impl Adb {
    pub fn at(path: impl Into<PathBuf>) -> Adb {
        Adb { path: path.into() }
    }

    /// Runs `adb <args>` and returns its combined output.
    ///
    /// Arguments are passed individually, so a path containing spaces survives.
    ///
    /// No timeout: install and push legitimately take minutes. Anything that runs while a
    /// window is open should use [`exec_timeout`](Self::exec_timeout) instead.
    pub fn exec(&self, args: &[&str]) -> Result<String, Error> {
        self.run(args, None)
    }

    /// As [`exec`](Self::exec), but gives up and kills the process after `timeout`.
    ///
    /// This exists because adb does not always come back. A failing USB port, a cable being
    /// re-seated, a device mid-re-enumeration: any of them can leave `adb devices` blocked
    /// indefinitely. Without a bound, one such call wedges whatever is waiting on it.
    pub fn exec_timeout(&self, args: &[&str], timeout: Duration) -> Result<String, Error> {
        self.run(args, Some(timeout))
    }

    fn run(&self, args: &[&str], timeout: Option<Duration>) -> Result<String, Error> {
        use std::io::Read;
        use std::sync::mpsc;

        let mut child = spawn_retrying_busy(&self.path, args)?;

        // Drained on their own threads. Waiting on a child while its pipe buffer fills is
        // the classic way to deadlock, and adb is chatty enough not to risk it.
        //
        // They report back through a channel rather than being joined, because a join is
        // not bounded by anything. Killing a process does not close a pipe that something
        // else still holds, and "something else" is any grandchild that inherited it: point
        // this at a launcher and the child exits at once, the timeout below never fires
        // because there is nothing left to wait for, and the read blocks for as long as the
        // program it started stays open. That hung the command line outright.
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let (tx, rx) = mpsc::channel::<(bool, String)>();
        let out_tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(p) = out_pipe.as_mut() {
                let _ = p.read_to_string(&mut buf);
            }
            let _ = out_tx.send((true, buf));
        });
        std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(p) = err_pipe.as_mut() {
                let _ = p.read_to_string(&mut buf);
            }
            let _ = tx.send((false, buf));
        });

        let deadline = timeout.map(|t| Instant::now() + t);
        let status = loop {
            match child.try_wait().map_err(Error::Launch)? {
                Some(status) => break status,
                None => {
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::TimedOut);
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
            }
        };

        // The child is gone, so in every honest case both pipes are already closed and this
        // returns at once. The grace exists only for the case above, and it is deliberately
        // short: there is nothing left worth waiting for.
        let mut out = String::new();
        let mut err = String::new();
        let grace = Instant::now() + READ_GRACE;
        for _ in 0..2 {
            let left = grace.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok((true, buf)) => out = buf,
                Ok((false, buf)) => err = buf,
                Err(_) => break,
            }
        }

        out.push_str(&err);
        if status.success() {
            Ok(out)
        } else {
            Err(Error::Failed { code: status.code(), output: out })
        }
    }

    /// First line of `adb version`, which is the human-readable one.
    pub fn version(&self) -> Option<String> {
        // Bounded, and this is the call that most needs it. It is the identity probe: it is
        // run against whatever file the user picked in the folder chooser, on startup and on
        // every re-check. Point it at a launcher or any long-lived program and an unbounded
        // wait never ends - not because the program hangs, but because a detached child
        // inherits the pipe and holds it open. That wedged the CLI outright and left the
        // window's re-check spinning with no way back.
        let out = self.exec_timeout(&["version"], QUERY_TIMEOUT).ok()?;
        out.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
    }

    /// Lists devices, bounded in time. Always called with a timeout: it runs while a
    /// window is open, and a blocked query must not become a blocked window.
    pub fn devices(&self) -> Result<Vec<Device>, Error> {
        Ok(parse_devices(&self.exec_timeout(&["devices", "-l"], QUERY_TIMEOUT)?))
    }

    /// Stops the local adb server. Worth offering because a stuck server is the cause of a
    /// surprising share of "my headset is not detected" reports.
    pub fn kill_server(&self) -> Result<(), Error> {
        // Bounded, because this is called on the way to replacing the binary: a server too
        // wedged to answer must not also stop the reinstall from happening.
        self.exec_timeout(&["kill-server"], QUERY_TIMEOUT).map(|_| ())
    }
}

/// The file is in use by someone else.
///
/// `ETXTBSY` on Unix, `ERROR_SHARING_VIOLATION` on Windows. Both mean the same thing here
/// and both clear on their own, so both are worth waiting out rather than reporting.
fn is_busy(e: &std::io::Error) -> bool {
    const BUSY: i32 = if cfg!(windows) { 32 } else { 26 };
    e.raw_os_error() == Some(BUSY)
}

/// Retries a filesystem operation that failed only because something still had the file.
///
/// Replacing adb is the case this exists for: the adb server is a background process whose
/// executable image *is* the file being replaced, and it does not let go the instant it is
/// asked to stop.
fn retry_busy<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    const TRIES: u32 = 6;
    for attempt in 0..TRIES {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if is_busy(&e) && attempt + 1 < TRIES => {
                std::thread::sleep(Duration::from_millis(150 * (attempt as u64 + 1)));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the loop returns on its last attempt")
}

/// Spawns adb, retrying the one failure that is a race rather than a fault.
///
/// This app unpacks its own copy of adb and then runs it. On Linux, forking from a process
/// that has threads means a child can inherit a write descriptor to a file another thread
/// is still writing, and executing that file then fails with `ETXTBSY` even though nothing
/// is wrong with it. Windows raises a sharing violation for the same shape of race.
///
/// It is brief and it clears itself, so the answer is to wait a moment and try again rather
/// than to report "adb could not be started" for a file that is sitting right there.
fn spawn_retrying_busy(path: &Path, args: &[&str]) -> Result<std::process::Child, Error> {
    use std::process::Stdio;

    const TRIES: u32 = 5;

    for attempt in 0..TRIES {
        match crate::engine::hide_console(&mut Command::new(path))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::NotFound),
            Err(e) if is_busy(&e) && attempt + 1 < TRIES => {
                std::thread::sleep(Duration::from_millis(20 * (attempt as u64 + 1)));
            }
            Err(e) => return Err(Error::Launch(e)),
        }
    }
    unreachable!("the loop returns on its last attempt")
}

/// Parses `adb devices -l`.
///
/// Written against the real shape of that output rather than by guessing: the header line,
/// the `* daemon ...` chatter adb emits on first run, and the `key:value` tail that `-l`
/// adds. The original checks `line.endsWith("device")`, which stops working the moment
/// `-l` is passed, and treats the presence of a help URL as its unauthorised signal.
pub fn parse_devices(output: &str) -> Vec<Device> {
    let mut devices = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('*')
            || line.starts_with("List of devices")
            || line.starts_with("adb server")
            || line.starts_with("error:")
        {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(serial), Some(state)) = (parts.next(), parts.next()) else {
            continue;
        };
        let model = parts
            .find_map(|token| token.strip_prefix("model:"))
            .map(|m| m.replace('_', " "));
        devices.push(Device {
            serial: serial.to_string(),
            state: State::parse(state),
            model,
        });
    }
    devices
}

/// Platform-tools archive for the running platform.
pub fn platform_tools_url() -> &'static str {
    if cfg!(windows) {
        concat!(
            "https://dl.google.com/android/repository/platform-tools-latest-",
            "windows.zip"
        )
    } else if cfg!(target_os = "macos") {
        concat!(
            "https://dl.google.com/android/repository/platform-tools-latest-",
            "darwin.zip"
        )
    } else {
        concat!(
            "https://dl.google.com/android/repository/platform-tools-latest-",
            "linux.zip"
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub enum InstallStage {
    Downloading,
    Extracting,
}

/// Downloads Google's platform-tools and unpacks them into the app's own directory.
///
/// Everything in the archive is kept, not just adb: on Linux and macOS adb loads a bundled
/// libc++ from a sibling directory, and on Windows it needs its two AdbWinApi DLLs.
pub fn install(
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(InstallStage, u64, Option<u64>),
) -> Result<PathBuf, String> {
    let tools = config::tools_dir();
    std::fs::create_dir_all(&tools).map_err(|e| format!("could not create {}: {e}", tools.display()))?;

    let live = tools.join(PLATFORM_TOOLS_DIR);
    // Anything already here is holding its own executable open. On Windows that is fatal
    // rather than untidy: a running image cannot be replaced at all, which is the whole
    // reason a reinstall used to fail with "used by another process".
    if live.join(exe_name()).is_file() {
        let _ = Adb::at(&live.join(exe_name())).kill_server();
    }

    let archive = tools.join("platform-tools.zip");
    let spec = Spec::new(platform_tools_url(), archive.clone());
    download::fetch(&spec, cancel, &mut |s| {
        on_progress(InstallStage::Downloading, s.done, s.total)
    })
    .map_err(|e| e.to_string())?;

    // Unpacked beside the live copy, never over it. A reinstall that fails halfway must
    // leave the adb that was working exactly where it was: ending up with neither is worse
    // than ending up with the old one.
    let incoming = tools.join(".incoming");
    let _ = std::fs::remove_dir_all(&incoming);
    unzip::extract(&archive, &incoming, cancel, &mut |done, total| {
        on_progress(InstallStage::Extracting, done, Some(total))
    })
    .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&archive);

    let staged = incoming.join(PLATFORM_TOOLS_DIR);
    if !staged.join(exe_name()).is_file() {
        let _ = std::fs::remove_dir_all(&incoming);
        return Err(format!("the download unpacked but {} was not in it", exe_name()));
    }

    swap_in(&live, &staged).map_err(|e| {
        let _ = std::fs::remove_dir_all(&incoming);
        format!("could not replace the existing adb: {e}")
    })?;
    let _ = std::fs::remove_dir_all(&incoming);

    let adb = managed_path();
    if !adb.is_file() {
        return Err(format!("platform-tools unpacked but {} is missing", adb.display()));
    }
    // Prove it runs before claiming success: an unpacked file that will not execute is a
    // worse outcome than a failed download, because nothing else reports it.
    let located = Adb::at(&adb);
    match located.version() {
        Some(_) => Ok(adb),
        None => Err("adb was unpacked but would not run".to_string()),
    }
}

/// Moves `staged` into `live`, keeping the old copy until the new one is in place.
///
/// Three steps rather than one so there is always a complete adb somewhere: the old one is
/// moved aside, the new one takes its place, and only then is the old one deleted. If the
/// middle step fails the old one goes back, so a failed reinstall costs nothing.
fn swap_in(live: &Path, staged: &Path) -> std::io::Result<()> {
    if !live.exists() {
        return retry_busy(|| std::fs::rename(staged, live));
    }
    let aside = live.with_extension("replacing");
    let _ = std::fs::remove_dir_all(&aside);
    retry_busy(|| std::fs::rename(live, &aside))?;

    match retry_busy(|| std::fs::rename(staged, live)) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&aside);
            Ok(())
        }
        Err(e) => {
            // Put back what was working before reporting the failure.
            let _ = std::fs::rename(&aside, live);
            Err(e)
        }
    }
}

/// How long to wait before giving up on a device query. Deliberately short: this runs on a
/// timer while the device step is open, and a hung adb must not stack up.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to keep reading output after the process itself has finished.
///
/// Not a timeout on adb - adb has already exited by this point. It bounds the one case
/// where a closed process leaves an open pipe behind, which is a grandchild that inherited
/// it. Short on purpose: if the output has not arrived by now, it is not coming.
const READ_GRACE: Duration = Duration::from_secs(3);

#[cfg(test)]
mod tests {
    use super::*;

    fn tools_tree(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("evrce-swap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A stand-in for an adb that never returns: it outlives the probe and, on Windows,
    /// the detached child it starts keeps the pipe open after the child itself is killed.
    #[cfg(unix)]
    fn slow_fake_adb(tag: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir()
            .join(format!("evrce-slow-adb-{tag}-{}", std::process::id()));
        std::fs::write(&p, "#!/bin/sh\nsleep 120\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    #[cfg(unix)]
    fn probing_a_program_that_never_answers_gives_up() {
        let fake = slow_fake_adb("version");
        let started = Instant::now();
        let got = Adb::at(&fake).version();
        assert!(got.is_none(), "a program that says nothing is not a working adb");
        assert!(
            started.elapsed() < QUERY_TIMEOUT + Duration::from_secs(5),
            "version() must be bounded: it is run against whatever file the user picked, \
             and it took {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_file(fake);
    }

    #[test]
    #[cfg(unix)]
    fn a_detached_child_holding_the_pipe_does_not_wedge_the_probe() {
        // The shape that actually hung: the program exits immediately, so nothing is left
        // to time out, but the process it started keeps the output pipe open behind it.
        // This is what a Windows .bat that ends in `start ""` does.
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir()
            .join(format!("evrce-detach-adb-{}", std::process::id()));
        std::fs::write(&p, "#!/bin/sh\nsleep 120 &\nexit 0\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = Instant::now();
        let got = Adb::at(&p).version();
        assert!(got.is_none(), "no version was printed, so this is not a working adb");
        assert!(
            started.elapsed() < QUERY_TIMEOUT,
            "the child exited at once, so this must return at once too, not in {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn swapping_replaces_the_old_copy() {
        let root = tools_tree("ok");
        let live = root.join("platform-tools");
        let staged = root.join(".incoming").join("platform-tools");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(live.join("adb"), b"old").unwrap();
        std::fs::write(staged.join("adb"), b"new").unwrap();

        swap_in(&live, &staged).unwrap();

        assert_eq!(std::fs::read(live.join("adb")).unwrap(), b"new");
        assert!(!live.with_extension("replacing").exists(), "the old copy is not left behind");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_swap_puts_the_working_copy_back() {
        // The property that matters. A reinstall that goes wrong must leave the adb that
        // was working where it was: ending up with neither is worse than keeping the old.
        let root = tools_tree("restore");
        let live = root.join("platform-tools");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("adb"), b"old").unwrap();
        // Nothing staged, so the second rename cannot succeed.
        let staged = root.join(".incoming").join("platform-tools");

        assert!(swap_in(&live, &staged).is_err());
        assert_eq!(
            std::fs::read(live.join("adb")).unwrap(),
            b"old",
            "the working copy must be back in place"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn swapping_into_an_empty_slot_just_moves() {
        let root = tools_tree("fresh");
        let live = root.join("platform-tools");
        let staged = root.join(".incoming").join("platform-tools");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("adb"), b"new").unwrap();

        swap_in(&live, &staged).unwrap();
        assert_eq!(std::fs::read(live.join("adb")).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Real `adb devices -l` output, including the daemon chatter that appears on the
    /// first run and trips naive parsers.
    const REAL: &str = "* daemon not running; starting now at tcp:5037
* daemon started successfully
List of devices attached
1WMHH8ABC123           device product:hollywood model:Quest_3 device:hollywood transport_id:1

";

    #[test]
    fn parses_a_connected_headset() {
        let d = parse_devices(REAL);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].serial, "1WMHH8ABC123");
        assert_eq!(d[0].state, State::Ready);
        // Underscores become spaces, because "Quest_3" is not how anyone writes it.
        assert_eq!(d[0].model.as_deref(), Some("Quest 3"));
    }

    /// The state the majority of "it does not work" reports are actually in.
    #[test]
    fn parses_an_unauthorised_headset() {
        let d = parse_devices("List of devices attached\n1WMHH8ABC123\tunauthorized\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].state, State::Unauthorized);
        assert_eq!(d[0].state.describe(), "not authorised");
    }

    #[test]
    fn parses_offline_and_unknown_states() {
        let d = parse_devices("List of devices attached\nA offline\nB sideloading\n");
        assert_eq!(d[0].state, State::Offline);
        assert_eq!(d[1].state, State::Unknown);
    }

    #[test]
    fn reports_no_devices_rather_than_inventing_one() {
        assert!(parse_devices("List of devices attached\n\n").is_empty());
        assert!(parse_devices("").is_empty());
        // Daemon chatter alone is not a device.
        assert!(parse_devices("* daemon not running; starting now at tcp:5037\n").is_empty());
    }

    #[test]
    fn ignores_adb_error_lines() {
        let out = "List of devices attached\nerror: device unauthorized.\n";
        assert!(parse_devices(out).is_empty());
    }

    /// Two headsets is the case that breaks the original outright: it never passes
    /// `-s <serial>`, so adb refuses with "more than one device".
    #[test]
    fn parses_more_than_one_device() {
        let out = "List of devices attached\nAAA device model:Quest_2\nBBB unauthorized\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].model.as_deref(), Some("Quest 2"));
        assert_eq!(d[1].state, State::Unauthorized);
    }

    #[test]
    fn platform_tools_url_matches_the_running_platform() {
        let url = platform_tools_url();
        assert!(url.starts_with(PLATFORM_TOOLS_BASE));
        let expected = if cfg!(windows) {
            "windows.zip"
        } else if cfg!(target_os = "macos") {
            "darwin.zip"
        } else {
            "linux.zip"
        };
        assert!(url.ends_with(expected), "got {url}");
    }

    #[test]
    fn managed_path_lives_under_the_app_directory() {
        let p = managed_path();
        assert!(p.starts_with(config::tools_dir()));
        assert!(p.ends_with(exe_name()));
    }

    /// A configured path that does not exist must not be preferred over a working one.
    #[test]
    fn a_missing_configured_path_is_ignored() {
        let missing = PathBuf::from("/definitely/not/an/adb");
        // No managed copy and probably no adb on PATH in a test environment, so this
        // asserts the fallthrough happened rather than a specific result.
        let found = locate(Some(&missing));
        assert!(found.map(|f| f.path) != Some(missing));
    }
}
