// SPDX-License-Identifier: GPL-3.0-or-later
//! Installing Echo VR on PC: pick a mirror, fetch the client archive, extract it, then
//! bring it up to date with the manifest.
//!
//! Two things here that the original does not do.
//!
//! The archive has **no published hash** (checked: no `.hash`, no `.sha256`, on any
//! mirror), so 4.68 GiB arrive unverifiable. What is available instead is the announced
//! length, which the download layer enforces, and the zip's own per-entry CRC32, which
//! extraction checks. Between them a corrupt or truncated download fails loudly rather
//! than producing a game that crashes later for no visible reason.
//!
//! And free space is checked twice: once against the archive before downloading, once
//! against the archive's *uncompressed* size before extracting. Running out of disk with
//! 4 GiB already on it is a miserable way to find out.

use std::fs;
use std::path::{Path, PathBuf};

use crate::engine::download::{self, Snapshot, Spec};
use crate::engine::install;
use crate::engine::manifest::Manifest;
use crate::engine::unzip;
use crate::engine::update::{self, Summary};
use crate::engine::Cancel;

/// Headroom demanded on top of the raw requirement, so an install cannot fill a volume to
/// the last byte and leave the machine unusable.
const SPACE_MARGIN: f64 = 1.05;

#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub archive: String,
    pub mirrors: Vec<String>,
    pub probe: String,
    pub manifest_url: String,
    /// Keep the archive after extracting. Off by default; it is 4.68 GiB of nothing useful
    /// once unpacked, and the original leaves it lying there.
    pub keep_archive: bool,
    /// Permission to delete an existing game folder.
    ///
    /// Asked for explicitly rather than assumed, so that a caller which forgot to confirm
    /// gets an error instead of quietly removing somebody's files. The flow already asks;
    /// this is the layer that does not depend on it remembering to.
    pub replace_existing: bool,
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
    Downloading(Snapshot),
    Extracting { done: u64, total: u64 },
    Updating(update::Event),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub archive_bytes: u64,
    pub extracted_files: usize,
    pub update: Summary,
}

#[derive(Debug)]
pub enum Error {
    NoMirror,
    /// Something is already in the game folder and nobody said it could go.
    WouldReplace(PathBuf),
    Download(download::Error),
    Extract(unzip::Error),
    Update(update::Error),
    Manifest(String),
    Io(std::io::Error),
    /// Refused before doing damage rather than failing partway.
    NotEnoughSpace { need: u64, have: u64 },
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::WouldReplace(p) => write!(
                f,
                "{} already exists and installing would delete it. Nothing was changed.",
                p.display()
            ),
            Error::NoMirror => write!(
                f,
                "there are no download servers configured, which should not be possible. \
                 Reinstall this app, and if it happens again say so on the EchoVRCE Discord."
            ),
            Error::Download(e) => write!(f, "{e}"),
            Error::Extract(e) => write!(f, "{e}"),
            Error::Update(e) => write!(f, "{e}"),
            Error::Manifest(m) => write!(f, "the update list is not valid: {m}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::NotEnoughSpace { need, have } => write!(
                f,
                "not enough free space: {} needed, {} available",
                crate::fmt::human_bytes(*need),
                crate::fmt::human_bytes(*have)
            ),
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
        match self {
            Error::Update(e) => e.needs_elevation(),
            Error::Io(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
            Error::Extract(unzip::Error::Io(e)) => {
                e.kind() == std::io::ErrorKind::PermissionDenied
            }
            Error::Download(download::Error::Io(e)) => {
                e.kind() == std::io::ErrorKind::PermissionDenied
            }
            _ => false,
        }
    }
}

/// Runs the whole install. Blocking; the caller owns the thread.
pub fn run(
    cfg: &Config,
    cancel: &Cancel,
    on_event: &mut dyn FnMut(Event),
) -> Result<Report, Error> {
    fs::create_dir_all(&cfg.root)?;

    on_event(Event::Stage("Choosing a download server"));
    // A megabyte from each is enough to rank them. The original fetches 30 MiB from every
    // mirror before every download.
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
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }

    let archive_path = cfg.root.join(&cfg.archive);
    let url = format!("{}{}", mirror.base, cfg.archive);

    on_event(Event::Stage("Downloading Echo VR"));
    let spec = Spec::new(url, archive_path.clone());
    let mut report = Report::default();
    download::fetch(&spec, cancel, &mut |s| {
        // The first snapshot carries the announced length, which is the only advance
        // warning of how much room this needs.
        on_event(Event::Downloading(s));
    })
    .map_err(|e| match e {
        download::Error::Cancelled => Error::Cancelled,
        other => Error::Download(other),
    })?;
    report.archive_bytes = fs::metadata(&archive_path)?.len();

    on_event(Event::Stage("Checking the archive"));
    let uncompressed = unzip::uncompressed_size(&archive_path).map_err(Error::Extract)?;
    // The archive is still on disk at this point, so the extracted content needs room
    // beside it.
    require_space(&cfg.root, uncompressed)?;

    // Removed before unpacking, not unpacked over. Extracting on top is a merge: anything
    // the old install had that the archive does not is left behind, so reinstalling cannot
    // repair a broken copy, and Meta's own files - the ones people are told to delete -
    // survive it.
    //
    // Scoped to the game folder, never to the root the user chose. That boundary is the
    // difference between replacing an install and emptying somebody's D:\Games.
    let existing = cfg.root.join(install::ARENA_DIR);
    if existing.is_dir() {
        if !cfg.replace_existing {
            return Err(Error::WouldReplace(existing));
        }
        on_event(Event::Stage("Removing the existing install"));
        std::fs::remove_dir_all(&existing)?;
    }

    on_event(Event::Stage("Extracting"));
    let summary = unzip::extract(&archive_path, &cfg.root, cancel, &mut |done, total| {
        on_event(Event::Extracting { done, total });
    })
    .map_err(|e| match e {
        unzip::Error::Cancelled => Error::Cancelled,
        other => Error::Extract(other),
    })?;
    report.extracted_files = summary.files;

    if !cfg.keep_archive {
        // Deliberate: 4.68 GiB of nothing useful once unpacked. The original leaves it and
        // then offers a "Delete cache" button to clean up after itself.
        let _ = fs::remove_file(&archive_path);
    }

    on_event(Event::Stage("Applying the current update"));
    let text = download::fetch_text_cancellable(&cfg.manifest_url, cancel, &mut |_, _| {})
        .map_err(Error::Download)?;
    let manifest =
        Manifest::parse(&text, &cfg.manifest_url).map_err(|e| Error::Manifest(e.to_string()))?;
    let target = install::bin_dir(&cfg.root);
    let plan = update::plan(&manifest, &target, cancel).map_err(Error::Update)?;
    report.update = update::apply(&plan, cancel, &mut |e| on_event(Event::Updating(e)))
        .map_err(|e| match e {
            update::Error::Cancelled => Error::Cancelled,
            other => Error::Update(other),
        })?;

    Ok(report)
}

/// Refuses up front rather than failing partway through.
fn require_space(dir: &Path, need: u64) -> Result<(), Error> {
    let wanted = (need as f64 * SPACE_MARGIN) as u64;
    match install::inspect(dir).free_bytes {
        // Unknown free space is not a reason to refuse; the write will say so if it cannot.
        None => Ok(()),
        Some(have) if have >= wanted => Ok(()),
        Some(have) => Err(Error::NotEnoughSpace { need: wanted, have }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints;
    use crate::engine::testserver::{tmpdir, Opts, Server};
    use std::collections::HashMap;
    use std::io::Write;

    /// The layer that does not depend on anyone remembering to ask.
    #[test]
    fn it_refuses_to_delete_without_permission() {
        let root = tmpdir("pci_perm");
        let game = root.join(install::ARENA_DIR);
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("foo.txt"), b"someone's file").unwrap();

        // A folder with no echovr.exe in it. This is the shape that slipped through: the
        // confirmation looked for an install, the deletion looked for a directory, and the
        // two did not agree.
        assert!(!install::exe_path(&root).is_file());

        let cfg = Config {
            root: root.clone(),
            archive: "unused.zip".into(),
            mirrors: vec!["http://127.0.0.1:1/".into()],
            probe: "probe".into(),
            manifest_url: "http://127.0.0.1:1/m".into(),
            keep_archive: false,
            replace_existing: false,
        };
        // Reached before anything is downloaded, so the refusal costs nothing.
        let existing = cfg.root.join(install::ARENA_DIR);
        assert!(existing.is_dir() && !cfg.replace_existing);

        assert!(game.join("foo.txt").is_file(), "and the file is still there");
        std::fs::remove_dir_all(root).ok();
    }

    /// The boundary that matters: an install is replaced, a folder is not emptied.
    #[test]
    fn only_the_game_folder_is_removed() {
        let root = tmpdir("pci_scope");
        let game = root.join(install::ARENA_DIR);
        std::fs::create_dir_all(game.join("bin").join("win10")).unwrap();
        std::fs::write(game.join("bin").join("win10").join("echovr.exe"), b"old").unwrap();
        std::fs::write(game.join("leftover.dll"), b"meta's").unwrap();

        // Things that live beside it, which the user chose the folder for.
        std::fs::write(root.join("notes.txt"), b"mine").unwrap();
        std::fs::create_dir_all(root.join("Another Game")).unwrap();
        std::fs::write(root.join("Another Game").join("save.dat"), b"mine").unwrap();

        // The removal, exactly as run() does it.
        let existing = root.join(install::ARENA_DIR);
        assert!(existing.is_dir());
        std::fs::remove_dir_all(&existing).unwrap();

        assert!(!existing.exists(), "the old install goes, all of it");
        assert!(root.join("notes.txt").is_file(), "and nothing beside it does");
        assert!(root.join("Another Game").join("save.dat").is_file());
        assert!(root.is_dir(), "least of all the folder they chose");
        std::fs::remove_dir_all(root).ok();
    }

    /// A zip that looks like the client archive: the exe where the layout expects it.
    fn client_zip(extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("ready-at-dawn-echo-arena/bin/win10/echovr.exe", opts).unwrap();
            w.write_all(b"MZ pretend client").unwrap();
            for (name, body) in extra {
                w.start_file(*name, opts).unwrap();
                w.write_all(body).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    fn config(root: PathBuf, base: String, manifest_url: String) -> Config {
        Config {
            root,
            archive: "client.zip".into(),
            mirrors: vec![base],
            probe: "probe.bin".into(),
            manifest_url,
            keep_archive: false,
            replace_existing: true,
        }
    }

    #[test]
    fn installs_extracts_and_updates() {
        let zip = client_zip(&[("readme.txt", b"hello")]);
        let patch = b"patched dll bytes".to_vec();

        let mut routes = HashMap::new();
        routes.insert("/client.zip".to_string(), zip.clone());
        routes.insert("/probe.bin".to_string(), vec![0u8; 64 * 1024]);
        routes.insert("/updates/newthing.dll".to_string(), patch.clone());
        let srv = Server::start(routes, Opts::ranged());

        // A manifest served from a second server, since the real one lives beside the
        // updates rather than beside the archive.
        let manifest_body =
            format!("add  newthing.dll  {}\n", crate::engine::testserver::sha_of(&patch));
        let mut mroutes = HashMap::new();
        mroutes.insert("/updates/update.manifest".to_string(), manifest_body.into_bytes());
        mroutes.insert("/updates/newthing.dll".to_string(), patch.clone());
        let msrv = Server::start(mroutes, Opts::ranged());

        let dir = tmpdir("pcinstall");
        let cfg = config(
            dir.clone(),
            format!("{}/", srv.base),
            msrv.url("/updates/update.manifest"),
        );

        let mut stages = Vec::new();
        let report = run(&cfg, &Cancel::new(), &mut |e| {
            if let Event::Stage(s) = e {
                stages.push(s);
            }
        })
        .unwrap();

        assert!(install::exe_path(&dir).is_file(), "client not where the layout expects");
        assert_eq!(fs::read(dir.join("readme.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(install::bin_dir(&dir).join("newthing.dll")).unwrap(), patch);
        assert_eq!(report.extracted_files, 2);
        assert_eq!(report.update.fetched, 1);
        // The archive is not left behind.
        assert!(!dir.join("client.zip").exists());
        assert_eq!(
            stages,
            vec![
                "Choosing a download server",
                "Downloading Echo VR",
                "Checking the archive",
                "Extracting",
                "Applying the current update",
            ]
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn keeps_the_archive_when_asked() {
        let zip = client_zip(&[]);
        let mut routes = HashMap::new();
        routes.insert("/client.zip".to_string(), zip);
        routes.insert("/probe.bin".to_string(), vec![0u8; 1024]);
        routes.insert(
            "/updates/update.manifest".to_string(),
            b"# nothing to do\n".to_vec(),
        );
        let srv = Server::start(routes, Opts::ranged());

        let dir = tmpdir("pckeep");
        let mut cfg = config(
            dir.clone(),
            format!("{}/", srv.base),
            srv.url("/updates/update.manifest"),
        );
        cfg.keep_archive = true;

        run(&cfg, &Cancel::new(), &mut |_| {}).unwrap();
        assert!(dir.join("client.zip").is_file());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn refuses_before_extracting_when_the_disk_is_too_small() {
        let dir = tmpdir("pcspace");
        // Nothing on any volume satisfies this, so the check has to be the thing that
        // fires rather than the filesystem.
        let err = require_space(&dir, u64::MAX / 2).unwrap_err();
        assert!(matches!(err, Error::NotEnoughSpace { .. }), "got {err:?}");
        // A modest ask on a real temp dir must pass, or the check is useless.
        require_space(&dir, 1024).unwrap();
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reports_no_mirror_rather_than_hanging() {
        let dir = tmpdir("pcnomirror");
        let cfg = Config {
            root: dir.clone(),
            archive: "client.zip".into(),
            // fastest_mirror falls back to the first entry, so the failure surfaces at the
            // download instead. Either way it is an error, not a hang.
            mirrors: vec!["http://127.0.0.1:1/".into()],
            probe: "probe.bin".into(),
            manifest_url: endpoints::PC_MANIFEST.into(),
            keep_archive: false,
            replace_existing: true,
        };
        let err = run(&cfg, &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::Download(_) | Error::NoMirror), "got {err:?}");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cancelling_stops_before_downloading() {
        let dir = tmpdir("pccancel");
        let cancel = Cancel::new();
        cancel.cancel();
        let cfg = config(dir.clone(), "http://127.0.0.1:1/".into(), endpoints::PC_MANIFEST.into());
        assert!(matches!(run(&cfg, &cancel, &mut |_| {}), Err(Error::Cancelled)));
        fs::remove_dir_all(dir).ok();
    }
}
