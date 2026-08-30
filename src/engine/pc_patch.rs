// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying the licence patch to a PC install.
//!
//! Two steps, deliberately separate: the file is fetched into staging first and only then
//! copied into the game folder. That split is what lets a retry after a wrong path reuse
//! the download instead of asking the bot for another one, which matters more than it
//! sounds: patch links are personal, they expire after 24 hours, and generating one takes
//! about ten seconds of somebody else's bot.

use std::path::{Path, PathBuf};

use crate::engine::download::{self, Snapshot, Spec};
use crate::engine::install;
use crate::engine::Cancel;

/// The file the patch consists of, and where it has to end up.
pub const PATCH_FILE: &str = "pnsovr.dll";

#[derive(Debug)]
pub enum Error {
    Download(download::Error),
    /// The chosen folder is not an Echo install.
    NoInstall(PathBuf),
    Io(std::io::Error),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Download(download::Error::Status { code: 404, .. }) => write!(
                f,
                "That patch link is gone. Discord links expire after 24 hours, so generate a \
                 new one."
            ),
            Error::Download(e) => write!(f, "{e}"),
            Error::NoInstall(p) => write!(
                f,
                "No Echo VR install at {}. The patch goes next to echovr.exe.",
                p.display()
            ),
            Error::Io(e) => write!(f, "{e}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl Error {
    pub fn needs_elevation(&self) -> bool {
        matches!(self, Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied)
    }

    /// True when generating a fresh link is the fix, rather than retrying this one.
    pub fn needs_new_link(&self) -> bool {
        matches!(self, Error::Download(download::Error::Status { code: 404, .. }))
    }
}

/// Fetches the patch into staging. Kept out of the game folder until it is known good.
pub fn stage(
    url: &str,
    staging: &Path,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(Snapshot),
) -> Result<PathBuf, Error> {
    std::fs::create_dir_all(staging)?;
    let dest = staging.join(PATCH_FILE);
    // No expected hash: the file is generated per request, so there is nothing published to
    // check it against. The announced length is enforced by the download layer.
    let spec = Spec::new(url.to_string(), dest.clone());
    download::fetch(&spec, cancel, on_progress).map_err(|e| match e {
        download::Error::Cancelled => Error::Cancelled,
        other => Error::Download(other),
    })?;
    Ok(dest)
}

/// Copies a staged patch into the install. Returns where it landed.
pub fn apply(staged: &Path, root: &Path) -> Result<PathBuf, Error> {
    let bin = install::bin_dir(root);
    // Checked rather than created: making the folder would put the patch somewhere the game
    // will never look, and report success for it.
    if !install::exe_path(root).is_file() {
        return Err(Error::NoInstall(root.to_path_buf()));
    }
    let dest = bin.join(PATCH_FILE);
    std::fs::copy(staged, &dest)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::testserver::{payload, tmpdir, Opts, Server};
    use std::collections::HashMap;

    fn fake_install(dir: &Path) {
        let bin = install::bin_dir(dir);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("echovr.exe"), b"MZ").unwrap();
    }

    #[test]
    fn stages_then_applies_into_the_game_folder() {
        let body = payload(4096);
        let mut routes = HashMap::new();
        routes.insert("/patch.dll".to_string(), body.clone());
        let srv = Server::start(routes, Opts::ranged());

        let dir = tmpdir("patch_ok");
        fake_install(&dir);
        let staging = dir.join("staging");

        let staged = stage(&srv.url("/patch.dll"), &staging, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), body);
        // Staged only: nothing in the game folder yet.
        assert!(!install::bin_dir(&dir).join(PATCH_FILE).exists());

        let placed = apply(&staged, &dir).unwrap();
        assert_eq!(placed, install::bin_dir(&dir).join(PATCH_FILE));
        assert_eq!(std::fs::read(&placed).unwrap(), body);
        std::fs::remove_dir_all(dir).ok();
    }

    /// The point of staging: a wrong path costs a copy, not another link from the bot.
    #[test]
    fn a_wrong_path_leaves_the_download_reusable() {
        let body = payload(1024);
        let mut routes = HashMap::new();
        routes.insert("/patch.dll".to_string(), body.clone());
        let srv = Server::start(routes, Opts::ranged());

        let dir = tmpdir("patch_wrongpath");
        let staging = dir.join("staging");
        let staged = stage(&srv.url("/patch.dll"), &staging, &Cancel::new(), &mut |_| {}).unwrap();

        // Not an install: refused.
        let empty = dir.join("not_echo");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(matches!(apply(&staged, &empty), Err(Error::NoInstall(_))));
        assert!(staged.is_file(), "the staged patch must survive to be retried");

        // Fixed path, no second download.
        let real = dir.join("echo");
        fake_install(&real);
        let before = srv.requests.load(std::sync::atomic::Ordering::Relaxed);
        apply(&staged, &real).unwrap();
        assert_eq!(
            srv.requests.load(std::sync::atomic::Ordering::Relaxed),
            before,
            "applying must not talk to the network"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// A folder with no echovr.exe is refused rather than being created and filled.
    #[test]
    fn refuses_a_folder_that_is_not_an_install() {
        let dir = tmpdir("patch_noinstall");
        let staged = dir.join("pnsovr.dll");
        std::fs::write(&staged, b"x").unwrap();
        assert!(matches!(apply(&staged, &dir), Err(Error::NoInstall(_))));
        assert!(!install::bin_dir(&dir).exists(), "must not have created the folder");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A dead link is reported as needing a new one, not as a generic failure: retrying the
    /// same expired URL can only fail again.
    #[test]
    fn a_404_asks_for_a_fresh_link() {
        let srv = Server::start(HashMap::new(), Opts::ranged());
        let dir = tmpdir("patch_404");
        let err = stage(&srv.url("/gone.dll"), &dir, &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(err.needs_new_link(), "got {err:?}");
        assert!(err.to_string().contains("24 hours"));
        std::fs::remove_dir_all(dir).ok();
    }
}
