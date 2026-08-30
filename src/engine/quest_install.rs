// SPDX-License-Identifier: GPL-3.0-or-later
//! Installing Echo VR on a Quest.
//!
//! Order is not arbitrary. Android wipes `/sdcard/Android/media/<package>` when a package
//! is uninstalled, so the game data has to be staged **after** the APK is installed or it
//! is deleted the moment the old build goes. The original learned this the hard way; its
//! source carries the comment.
//!
//! One check the original does not make: after installing, the APK on the device is hashed
//! and compared against the file that was sent. `adb install` reporting Success is not the
//! same as the device now holding the build we handed it, and every later version decision
//! rests on that being true.

use std::path::{Path, PathBuf};

use crate::engine::download::{self, Spec};
use crate::engine::hash;
use crate::engine::manifest::Manifest;
use crate::engine::quest::{self, Marker, Quest};
use crate::engine::quest_update;
use crate::engine::Cancel;

/// Game data lives in the app-owned external media directory. Unlike the
/// `/sdcard/readyatdawn` the original used to use, this needs no storage permission, so it
/// works on a secondary Quest account.
const DATA_DIR: &str = "/sdcard/Android/media/com.readyatdawn.r15/files";
/// Pushed here first, then moved. Writing a large archive straight onto the synthesised
/// /sdcard mount is markedly slower.
const STAGE_REMOTE: &str = "/data/local/tmp/_data.zip";
const DATA_ARCHIVE: &str = "_data.zip";
/// Where an install before mid-2025 put its data. Removed so it cannot shadow the new one.
const LEGACY_DIR: &str = "/sdcard/readyatdawn";

#[derive(Debug, Clone)]
pub struct Config {
    /// APK filename, which is also its name on the mirrors. Comes from the manifest's
    /// BASE_APK header; there is deliberately no built-in fallback, because the original's
    /// stale one installs a six-week-old build that the version gate then refuses forever.
    pub apk_name: String,
    pub base_sha256: String,
    /// Set when the APK came from the patch bot rather than the mirrors.
    pub patched_url: Option<String>,
    pub mirrors: Vec<String>,
    pub probe: String,
    pub staging: PathBuf,
    pub installer_version: String,
}

#[derive(Debug, Clone)]
pub enum Event {
    Stage(&'static str),
    Mirror(String),
    /// A server about to be tried, and which of how many it is. Sent before the attempt:
    /// the probe is several seconds of nothing on screen otherwise.
    Probing { base: String, index: usize, of: usize },
    /// A server that did not answer, or the note that none of them did.
    MirrorProblem(String),
    Downloading { what: String, done: u64, total: Option<u64> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub apk_sha256: String,
    pub patched: bool,
}

#[derive(Debug)]
pub enum Error {
    NoMirror,
    Download { what: String, source: download::Error },
    Device(quest::Error),
    /// The device ended up holding something other than what was sent.
    WrongApkInstalled { sent: String, found: String },
    /// The install landed but bringing it up to date did not.
    Update(String),
    Io(std::io::Error),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoMirror => write!(
                f,
                "there are no download servers configured, which should not be possible. \
                 Reinstall this app, and if it happens again say so on the EchoVRCE Discord."
            ),
            Error::Download { what, source } => write!(f, "{what}: {source}"),
            Error::Device(e) => write!(f, "{e}"),
            Error::WrongApkInstalled { .. } => write!(
                f,
                "the app installed on the headset is not the one that was sent. Uninstall \
                 Echo VR on the headset and try again."
            ),
            Error::Update(m) => write!(
                f,
                "Echo VR was installed, but applying the current update failed: {m}. Run \
                 Update Echo VR (Quest) to finish."
            ),
            Error::Io(e) => write!(f, "{e}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<quest::Error> for Error {
    fn from(e: quest::Error) -> Self {
        Error::Device(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Fetches the APK and the data archive. Split from the install so the download can happen
/// while the headset is still in its box.
pub fn download(
    cfg: &Config,
    cancel: &Cancel,
    on_event: &mut dyn FnMut(Event),
) -> Result<(PathBuf, PathBuf), Error> {
    std::fs::create_dir_all(&cfg.staging)?;

    on_event(Event::Stage("Choosing a download server"));
    let mirror = download::fastest_mirror(
        &cfg.mirrors,
        &cfg.probe,
        1024 * 1024,
        cancel,
        &mut |base, i, of| on_event(Event::Probing { base: base.to_string(), index: i, of }),
    )
        .ok_or(Error::NoMirror)?;
    on_event(Event::Mirror(mirror.base.clone()));
    // Said out loud rather than kept: if none of them answered the speed test, a failure
    // later on should not be the first anyone hears of it.
    if !mirror.measured {
        for (base, why) in &mirror.failures {
            on_event(Event::MirrorProblem(format!("{base} did not answer: {why}")));
        }
        on_event(Event::MirrorProblem(
            "no download server passed the speed test; trying the first one anyway".into(),
        ));
    }

    let apk = cfg.staging.join(&cfg.apk_name);
    on_event(Event::Stage("Downloading Echo VR"));
    // A personalised APK is repacked, so its hash cannot be checked against the manifest's
    // base. A stock one can and is.
    let (apk_url, expected) = match &cfg.patched_url {
        Some(url) => (url.clone(), None),
        None => (format!("{}{}", mirror.base, cfg.apk_name), Some(cfg.base_sha256.clone())),
    };
    let mut spec = Spec::new(apk_url, apk.clone());
    if let Some(sha) = expected {
        spec = spec.with_sha256(sha);
    }
    let name = cfg.apk_name.clone();
    fetch(&spec, cancel, &name, on_event)?;

    on_event(Event::Stage("Downloading game data"));
    let data = cfg.staging.join(DATA_ARCHIVE);
    let spec = Spec::new(format!("{}{DATA_ARCHIVE}", mirror.base), data.clone());
    fetch(&spec, cancel, DATA_ARCHIVE, on_event)?;

    Ok((apk, data))
}

fn fetch(
    spec: &Spec,
    cancel: &Cancel,
    what: &str,
    on_event: &mut dyn FnMut(Event),
) -> Result<(), Error> {
    download::fetch(spec, cancel, &mut |s| {
        on_event(Event::Downloading {
            what: what.to_string(),
            done: s.done,
            total: s.total,
        })
    })
    .map_err(|e| match e {
        download::Error::Cancelled => Error::Cancelled,
        source => Error::Download { what: what.to_string(), source },
    })?;
    Ok(())
}

/// Puts a downloaded APK and data archive onto a headset, then brings it up to date.
///
/// The update is part of installing, not a thing to remember afterwards. A fresh install
/// that is already behind the current manifest is a trap: the game is there, it is subtly
/// wrong, and nothing says so. The PC side has always done this; leaving it out here was an
/// omission, caught by a real headset ending up without any of the manifest's asset
/// patches.
pub fn install(
    cfg: &Config,
    apk: &Path,
    data: &Path,
    manifest: Option<&Manifest>,
    quest: &Quest<'_>,
    cancel: &Cancel,
    on_event: &mut dyn FnMut(Event),
) -> Result<Report, Error> {
    let check = |cancel: &Cancel| -> Result<(), Error> {
        if cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    };

    // Hashed before anything is sent, so the comparison after installing is against a
    // known quantity rather than against the file as it is at that later moment.
    let sent_sha = hash::sha256_file(apk)?;

    check(cancel)?;
    on_event(Event::Stage("Removing the previous install"));
    quest.uninstall()?;
    // An install from before the data directory moved would otherwise sit there shadowing
    // the new one.
    let _ = quest.shell(&format!("rm -rf {LEGACY_DIR}"));

    check(cancel)?;
    on_event(Event::Stage("Installing Echo VR"));
    quest.install_apk(apk)?;

    // adb saying Success is not the same as the device holding what was sent.
    on_event(Event::Stage("Verifying the install"));
    match quest.installed_sha() {
        Some(found) if found.eq_ignore_ascii_case(&sent_sha) => {}
        Some(found) => {
            return Err(Error::WrongApkInstalled { sent: sent_sha, found });
        }
        // The device could not hash it. Not grounds to fail an otherwise clean install,
        // but the marker records what was sent, which is still the honest answer.
        None => {}
    }

    check(cancel)?;
    on_event(Event::Stage("Copying game data"));
    // After the install, never before: uninstalling wipes this directory.
    let _ = quest.shell(&format!("mkdir -p {DATA_DIR}/_local"));
    let _ = quest.shell(&format!("chmod -R 777 {DATA_DIR}"));
    quest.push(data, STAGE_REMOTE)?;

    check(cancel)?;
    on_event(Event::Stage("Unpacking on the headset"));
    quest.shell(&format!("mv {STAGE_REMOTE} {DATA_DIR}/"))?;
    quest.shell(&format!("cd {DATA_DIR}/ && unzip -o {DATA_ARCHIVE}"))?;
    let _ = quest.shell(&format!("cd {DATA_DIR}/ && rm {DATA_ARCHIVE}"));
    let _ = quest.shell(&format!("chmod -R 777 {DATA_DIR}"));

    on_event(Event::Stage("Granting permissions"));
    // `install -g` already granted the manifest's permissions; these are the ones that
    // need asking for separately, and none of them is worth failing the install over.
    for args in [
        vec!["shell", "appops", "set", quest::PACKAGE, "MANAGE_EXTERNAL_STORAGE", "allow"],
        vec!["shell", "pm", "grant", quest::PACKAGE, "android.permission.RECORD_AUDIO"],
    ] {
        let _ = quest.exec(&args);
    }

    on_event(Event::Stage("Recording the version"));
    let marker = Marker {
        base_apk: cfg.apk_name.clone(),
        base_sha256: cfg.base_sha256.clone(),
        installed_sha256: sent_sha.clone(),
        patched: cfg.patched_url.is_some(),
        installed_at: now_iso8601(),
        installer_version: cfg.installer_version.clone(),
    };
    // Written before the update, once everything *it* describes is true. That ordering is
    // deliberate: a failed update then leaves a correct marker behind, so the standalone
    // update flow can pick up cleanly instead of refusing an install it cannot identify.
    quest.write_marker(&marker)?;

    if let Some(manifest) = manifest {
        on_event(Event::Stage("Applying the current update"));
        let root = manifest.target_root().unwrap_or(quest::MEDIA_ROOT).to_string();
        let plan = quest_update::plan(manifest, quest, cancel, &mut |_| {})
            .map_err(|e| Error::Update(e.to_string()))?;
        quest_update::apply(&plan, quest, &root, &cfg.staging, cancel, &mut |_| {})
            .map_err(|e| Error::Update(e.to_string()))?;
    }

    Ok(Report { apk_sha256: sent_sha, patched: cfg.patched_url.is_some() })
}

/// UTC, to the second, in the shape the other installers write.
fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days, so no date library is needed for one timestamp.
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = crate::fmt::civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_match_the_interop_shape() {
        let t = now_iso8601();
        assert_eq!(t.len(), 20, "got {t}");
        assert!(t.ends_with('Z'));
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[10..11], "T");
        // Sanity: this app did not ship before 2026.
        let year: i64 = t[..4].parse().unwrap();
        assert!((2026..2100).contains(&year), "got {t}");
    }

}
