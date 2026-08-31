// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding out whether a newer installer exists, and replacing this one with it.
//!
//! Deliberately not the GitHub API. The release publishes a `version.txt` beside the zip,
//! so a check is one plain GET of a file whose URL never changes: no JSON to parse, no
//! rate limit to share with everyone behind the same NAT, and no API contract that can be
//! reshaped underneath us. The cost is that the release workflow has to keep publishing
//! that file, which is one line of CI.

use std::path::{Path, PathBuf};

use crate::endpoints;
use crate::engine::download::{self, Snapshot, Spec};
use crate::engine::{unzip, Cancel};

pub const STALE_AFTER_DAYS: u64 = 7;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Stage(&'static str),
    Downloading(Snapshot),
    Extracting { done: u64, total: u64 },
}

#[derive(Debug)]
pub enum Error {
    Fetch(download::Error),
    Unzip(unzip::Error),
    /// The zip did not contain the two executables where they were expected.
    Incomplete(String),
    /// The folder the app runs from cannot be written to, so nothing was attempted.
    NotWritable(PathBuf),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Fetch(e) => write!(f, "{e}"),
            Error::Unzip(e) => write!(f, "{e}"),
            Error::Incomplete(what) => write!(f, "the download did not contain {what}"),
            Error::NotWritable(dir) => write!(
                f,
                "{} cannot be written to, so the update was not applied",
                crate::fmt::windows_path(dir)
            ),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<download::Error> for Error {
    fn from(e: download::Error) -> Self {
        Error::Fetch(e)
    }
}
impl From<unzip::Error> for Error {
    fn from(e: unzip::Error) -> Self {
        Error::Unzip(e)
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// The version this binary is.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Asks what the newest published version is.
pub fn published(cancel: &Cancel) -> Result<String, Error> {
    let text = download::fetch_text_cancellable(endpoints::UPDATE_VERSION, cancel, &mut |_, _| {})?;
    Ok(clean_version(&text))
}

/// Strips whatever the publishing side wrapped the version in.
///
/// A byte order mark is the one worth naming: PowerShell writes UTF-8 with a BOM often
/// enough that a version compared with it silently never matches anything.
fn clean_version(raw: &str) -> String {
    raw.trim_start_matches('\u{feff}')
        .trim()
        .trim_start_matches('v')
        .to_string()
}

/// Is `remote` a later version than `local`?
///
/// Compares dotted numeric components, longest wins on a tie so `0.9.3.1` beats `0.9.3`.
/// A component that is not a number sorts as zero rather than making the whole comparison
/// fail: a malformed tag should not be read as "there is an update".
pub fn is_newer(remote: &str, local: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.').map(|p| p.trim().parse::<u64>().unwrap_or(0)).collect()
    };
    let (a, b) = (parse(remote), parse(local));
    let len = a.len().max(b.len());
    for i in 0..len {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// The two files an update has to replace.
///
/// Both, always. Replacing only the window leaves it driving a command line binary from
/// the previous version as its elevated worker, and those two talk to each other over a
/// format that is free to change between releases.
const BINARIES: [&str; 2] = ["echo-vrce-installer.exe", "echo-vrce-cli.exe"];

/// Where this installation lives.
pub fn install_dir() -> Result<PathBuf, Error> {
    let exe = std::env::current_exe()?;
    Ok(exe.parent().map(Path::to_path_buf).unwrap_or_default())
}

/// Can an update be applied in place at all?
///
/// Checked before the button is offered rather than after the download. Inside
/// `C:\Program Files` this is false and there is no way around it worth having: the
/// elevation broker runs `echo-vrce-cli.exe`, which is one of the files being replaced.
pub fn can_replace_in_place() -> bool {
    let Ok(dir) = install_dir() else { return false };
    let probe = dir.join(".evrce-write-test");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Downloads the published release and puts it in place.
///
/// Returns the folder the previous version was moved to, which is the same folder with
/// `.old` on each name.
pub fn apply(cancel: &Cancel, on_event: &mut dyn FnMut(Event)) -> Result<(), Error> {
    let dir = install_dir()?;
    if !can_replace_in_place() {
        return Err(Error::NotWritable(dir));
    }

    let work = dir.join(".evrce-update");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;

    on_event(Event::Stage("Checking the published hash"));
    // A failure here is not fatal. The hash is a separate asset and a release published
    // without it should still be installable; what must not happen is installing an
    // archive whose hash was fetched and did not match.
    let sha = download::fetch_text_cancellable(endpoints::UPDATE_SHA256, cancel, &mut |_, _| {})
        .ok()
        .map(|t| clean_version(&t).to_ascii_lowercase())
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()));

    on_event(Event::Stage("Downloading the update"));
    let zip = work.join("update.zip");
    let mut spec = Spec::new(endpoints::UPDATE_ZIP, &zip);
    if let Some(h) = sha {
        spec = spec.with_sha256(h);
    }
    download::fetch(&spec, cancel, &mut |snap| on_event(Event::Downloading(snap)))?;

    on_event(Event::Stage("Unpacking"));
    let unpacked = work.join("unpacked");
    unzip::extract(&zip, &unpacked, cancel, &mut |done, total| {
        on_event(Event::Extracting { done, total })
    })?;

    // The zip holds one folder, whose name carries no version any more, so find the
    // executables rather than assuming where they sit.
    let mut found = Vec::new();
    for name in BINARIES {
        match locate(&unpacked, name) {
            Some(p) => found.push((name, p)),
            None => return Err(Error::Incomplete(name.to_string())),
        }
    }

    on_event(Event::Stage("Replacing"));
    for (name, new) in &found {
        let live = dir.join(name);
        if live.exists() {
            // A running executable cannot be deleted or written over on Windows, but it
            // CAN be renamed: renaming does not invalidate the image already mapped into
            // memory. That is what makes an in-place update possible without a second
            // process, and it is what leaves the previous version sitting beside the new
            // one for anyone who needs to go back by hand.
            let old = dir.join(format!("{name}.old"));
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&live, &old)?;
        }
        std::fs::rename(new, &live)?;
    }

    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

fn locate(root: &Path, name: &str) -> Option<PathBuf> {
    let direct = root.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        if entry.file_type().ok()?.is_dir() {
            let candidate = entry.path().join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Removes the previous version, if one is sitting beside this binary.
///
/// Called at startup, which is the only test that means anything: this code running at all
/// proves the new build launches. Until then the old one stays where the user can find it.
pub fn sweep_previous() {
    let Ok(dir) = install_dir() else { return };
    for name in BINARIES {
        let old = dir.join(format!("{name}.old"));
        if old.exists() && std::fs::remove_file(&old).is_ok() {
            crate::log::line(&format!("removed {}", old.display()));
        }
    }
    let _ = std::fs::remove_dir_all(dir.join(".evrce-update"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_is_newer_and_equal_is_not() {
        assert!(is_newer("0.9.4", "0.9.3"));
        assert!(is_newer("0.10.0", "0.9.9"), "10 is not less than 9 because it starts with a 1");
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.9.3", "0.9.3"));
        assert!(!is_newer("0.9.2", "0.9.3"));
    }

    #[test]
    fn a_longer_version_wins_a_tie() {
        assert!(is_newer("0.9.3.1", "0.9.3"));
        assert!(!is_newer("0.9.3", "0.9.3.1"));
    }

    #[test]
    fn rubbish_is_not_read_as_an_update() {
        // The remote side is the one that can go wrong, and the failure that matters is
        // telling everybody there is a new version when there is not.
        assert!(!is_newer("", "0.9.3"));
        assert!(!is_newer("not-a-version", "0.9.3"));
        assert!(!is_newer("<!doctype html>", "0.9.3"));
    }

    #[test]
    fn a_version_survives_what_the_publisher_wraps_it_in() {
        assert_eq!(clean_version("0.9.4\n"), "0.9.4");
        assert_eq!(clean_version("\u{feff}0.9.4\r\n"), "0.9.4");
        assert_eq!(clean_version("  v0.9.4  "), "0.9.4");
    }

    #[test]
    fn both_binaries_are_replaced() {
        // A window on the new version driving a command line from the old one is a
        // protocol mismatch waiting to happen, so this list is not somewhere to be tidy.
        assert!(BINARIES.contains(&"echo-vrce-installer.exe"));
        assert!(BINARIES.contains(&"echo-vrce-cli.exe"));
        assert_eq!(BINARIES.len(), 2);
    }
}
