// SPDX-License-Identifier: GPL-3.0-or-later
//! Applying an update manifest to a Quest, over adb.
//!
//! The same plan-then-apply shape as the PC side, for the same reason: the user is told
//! what will happen before it happens, and the skip logic is testable on its own.
//!
//! What differs is where the comparison happens. On PC the local files are hashed directly;
//! here they live on the headset, so hashing means asking the device. That is done in
//! batches, because a round trip per file over adb is slow enough to be noticeable, and it
//! degrades to "push everything" when the device has no `sha256sum` rather than pulling
//! every file back to hash it, which would cost more than re-pushing.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::engine::download;
use crate::engine::manifest::{Entry, Manifest};
use crate::engine::quest::{self, Quest};
use crate::engine::Cancel;

/// Caps on one batched `sha256sum` call, in paths and in characters. A device's argument
/// list is not unlimited and a silently truncated command would look like "these files are
/// all missing".
const BATCH_PATHS: usize = 50;
const BATCH_CHARS: usize = 3000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub rel: String,
    pub remote: String,
    pub url: String,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub deletes: Vec<Step>,
    pub pushes: Vec<Step>,
    pub up_to_date: Vec<String>,
    /// True when the device could not hash its own files, so nothing could be skipped.
    pub hashing_unavailable: bool,
}

impl Plan {
    pub fn work_items(&self) -> usize {
        self.deletes.len() + self.pushes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.work_items() == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub deleted: usize,
    pub pushed: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone)]
pub enum Event {
    Hashing,
    Deleting { rel: String, index: usize, of: usize },
    Downloading { rel: String, index: usize, of: usize, done: u64, total: Option<u64> },
    Pushing { rel: String, index: usize, of: usize },
    Placed { rel: String },
}

#[derive(Debug)]
pub enum Error {
    NoTarget,
    Device(quest::Error),
    Download { rel: String, source: download::Error },
    Io(std::io::Error),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoTarget => write!(f, "the update list does not say where files go on the device"),
            Error::Device(e) => write!(f, "{e}"),
            Error::Download { rel, source } => write!(f, "{rel}: {source}"),
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

/// Works out what needs pushing, asking the device to hash what it already has.
pub fn plan(
    manifest: &Manifest,
    quest: &Quest<'_>,
    cancel: &Cancel,
    on_event: &mut dyn FnMut(Event),
) -> Result<Plan, Error> {
    let root = manifest.target_root().ok_or(Error::NoTarget)?;
    let mut plan = Plan::default();

    for entry in manifest.dels() {
        plan.deletes.push(step(manifest, entry, root));
    }

    let adds: Vec<Step> = manifest.adds().map(|e| step(manifest, e, root)).collect();
    if adds.is_empty() {
        return Ok(plan);
    }

    on_event(Event::Hashing);
    let remote = match remote_hashes(quest, root, &adds, cancel) {
        Some(map) => map,
        None => {
            // No sha256sum on the device: nothing can be skipped, so everything is pushed.
            plan.hashing_unavailable = true;
            HashMap::new()
        }
    };

    for step in adds {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let matches = step
            .sha256
            .as_deref()
            .zip(remote.get(&step.rel))
            .is_some_and(|(want, have)| want.eq_ignore_ascii_case(have));
        if matches {
            plan.up_to_date.push(step.rel);
        } else {
            plan.pushes.push(step);
        }
    }
    Ok(plan)
}

fn step(manifest: &Manifest, entry: &Entry, root: &str) -> Step {
    Step {
        rel: entry.path.clone(),
        remote: format!("{root}/{}", entry.path),
        url: manifest.url_for(entry),
        sha256: entry.sha256.clone(),
    }
}

/// Hashes the manifest's targets on the device, in as few round trips as possible.
/// Returns None when the device cannot hash at all.
fn remote_hashes(
    quest: &Quest<'_>,
    root: &str,
    adds: &[Step],
    cancel: &Cancel,
) -> Option<HashMap<String, String>> {
    // Probe with a file that exists on every Android build, so "no output" means the tool
    // is missing rather than the file being absent.
    let probe = quest.shell("sha256sum /system/build.prop").ok()?;
    quest::first_hash(&probe)?;

    let mut hashes = HashMap::new();
    let mut batch: Vec<&str> = Vec::new();
    let mut chars = 0usize;

    let flush = |batch: &mut Vec<&str>, hashes: &mut HashMap<String, String>| {
        if batch.is_empty() {
            return;
        }
        // One argv element for the whole script: the host shell never sees it, and the
        // manifest's path validation guarantees the device's shell sees no metacharacters.
        let script = format!("cd {root} && sha256sum {} 2>/dev/null", batch.join(" "));
        if let Ok(out) = quest.shell(&script) {
            hashes.extend(quest::parse_hash_listing(&out));
        }
        batch.clear();
    };

    let mut seen = std::collections::HashSet::new();
    for step in adds {
        if cancel.is_cancelled() {
            break;
        }
        if !seen.insert(step.rel.as_str()) {
            continue;
        }
        if !batch.is_empty() && (batch.len() >= BATCH_PATHS || chars + step.rel.len() > BATCH_CHARS)
        {
            flush(&mut batch, &mut hashes);
            chars = 0;
        }
        chars += step.rel.len() + 1;
        batch.push(&step.rel);
    }
    flush(&mut batch, &mut hashes);
    Some(hashes)
}

/// Carries out a plan. Deletions first, then pushes, which is the manifest's own ordering.
pub fn apply(
    plan: &Plan,
    quest: &Quest<'_>,
    root: &str,
    staging: &PathBuf,
    cancel: &Cancel,
    on_event: &mut dyn FnMut(Event),
) -> Result<Summary, Error> {
    std::fs::create_dir_all(staging)?;
    let total = plan.work_items();
    let mut summary = Summary { skipped: plan.up_to_date.len(), ..Default::default() };
    let mut index = 0;

    for step in &plan.deletes {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        index += 1;
        on_event(Event::Deleting { rel: step.rel.clone(), index, of: total });
        // rm -f exits 0 for something already gone, and a leftover file is not worth
        // aborting an update over.
        let _ = quest.shell(&format!("rm -rf {}", step.remote));
        summary.deleted += 1;
        on_event(Event::Placed { rel: step.rel.clone() });
    }

    let mut last_parent: Option<String> = None;
    for step in &plan.pushes {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        index += 1;

        let local = staging.join(step.rel.replace('/', "_"));
        let mut spec = download::Spec::new(step.url.clone(), local.clone());
        if let Some(sha) = &step.sha256 {
            spec = spec.with_sha256(sha.clone());
        }
        let rel = step.rel.clone();
        download::fetch(&spec, cancel, &mut |s| {
            on_event(Event::Downloading {
                rel: rel.clone(),
                index,
                of: total,
                done: s.done,
                total: s.total,
            })
        })
        .map_err(|e| match e {
            download::Error::Cancelled => Error::Cancelled,
            source => Error::Download { rel: step.rel.clone(), source },
        })?;

        // The live manifest groups entries by directory, so this collapses many mkdirs
        // into one per directory.
        let parent = step
            .remote
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| root.to_string());
        if last_parent.as_deref() != Some(parent.as_str()) {
            let _ = quest.shell(&format!("mkdir -p {parent}"));
            last_parent = Some(parent);
        }

        on_event(Event::Pushing { rel: step.rel.clone(), index, of: total });
        quest.push(&local, &step.remote)?;
        let _ = std::fs::remove_file(&local);
        summary.pushed += 1;
        on_event(Event::Placed { rel: step.rel.clone() });
    }

    // /sdcard is a synthesised FUSE mount, so chmod may be a no-op or fail outright
    // depending on the build. Never fatal.
    let _ = quest.shell(&format!("chmod -R 777 {root}"));
    Ok(summary)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::engine::adb::Adb;
    use crate::engine::testserver::{payload, sha_of, tmpdir, Opts, Server};
    use std::collections::HashMap as Map;
    use std::os::unix::fs::PermissionsExt;

    const ROOT: &str = "/sdcard/Android/media/com.readyatdawn.r15";

    /// A stand-in adb, recording argv and answering from a case statement.
    struct Fake {
        dir: PathBuf,
        adb: Adb,
    }

    impl Fake {
        fn new(tag: &str, body: &str) -> Fake {
            let dir = tmpdir(&format!("qu_{tag}"));
            let path = dir.join("adb");
            let log = dir.join("argv.log");
            std::fs::write(
                &path,
                format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n{body}\n", log.display()),
            )
            .unwrap();
            let mut p = std::fs::metadata(&path).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&path, p).unwrap();
            Fake { adb: Adb::at(&path), dir }
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("argv.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    /// Builds a manifest whose entries are served by `server`, targeting the media root.
    fn manifest_for(server: &Server, files: &[(&str, &[u8])], dels: &[&str]) -> Manifest {
        let mut body = format!("# Target: {ROOT}\n");
        for (rel, bytes) in files {
            body.push_str(&format!("add  {rel}  {}\n", sha_of(bytes)));
        }
        for rel in dels {
            body.push_str(&format!("del  {rel}\n"));
        }
        Manifest::parse(&body, &server.url("/updates/quest/update.manifest")).unwrap()
    }

    fn routes(files: &[(&str, &[u8])]) -> Map<String, Vec<u8>> {
        let mut r = Map::new();
        for (rel, bytes) in files {
            r.insert(format!("/updates/quest/{rel}"), bytes.to_vec());
        }
        r
    }

    /// Answers the sha256sum probe, then reports one known hash for `a.bin`.
    fn hashing_adb(tag: &str, known: &str) -> Fake {
        let body = [
            "case \"$*\" in",
            "*build.prop*) echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  /system/build.prop';;",
            &format!("*sha256sum*) echo '{known}  a.bin';;"),
            "*push*) echo '1 file pushed. 1.0 MB/s (10 bytes in 0.001s)';;",
            "*) echo '';;",
            "esac",
        ]
        .join("\n");
        Fake::new(tag, &body)
    }

    #[test]
    fn skips_files_the_device_already_has() {
        let a = payload(64);
        let b = payload(128);
        let srv = Server::start(routes(&[("a.bin", &a), ("b.bin", &b)]), Opts::ranged());
        let m = manifest_for(&srv, &[("a.bin", &a), ("b.bin", &b)], &[]);

        let f = hashing_adb("skip", &sha_of(&a));
        let q = Quest::new(&f.adb, None);
        let plan = plan(&m, &q, &Cancel::new(), &mut |_| {}).unwrap();

        assert_eq!(plan.up_to_date, vec!["a.bin".to_string()]);
        assert_eq!(plan.pushes.len(), 1);
        assert_eq!(plan.pushes[0].rel, "b.bin");
        assert!(!plan.hashing_unavailable);
        std::fs::remove_dir_all(&f.dir).ok();
    }

    /// A device with no sha256sum must still update: it just cannot skip anything.
    #[test]
    fn pushes_everything_when_the_device_cannot_hash() {
        let a = payload(64);
        let srv = Server::start(routes(&[("a.bin", &a)]), Opts::ranged());
        let m = manifest_for(&srv, &[("a.bin", &a)], &[]);

        let f = Fake::new("nohash", "echo 'sha256sum: not found'");
        let q = Quest::new(&f.adb, None);
        let plan = plan(&m, &q, &Cancel::new(), &mut |_| {}).unwrap();

        assert!(plan.hashing_unavailable);
        assert_eq!(plan.pushes.len(), 1);
        assert!(plan.up_to_date.is_empty());
        std::fs::remove_dir_all(&f.dir).ok();
    }

    /// A manifest with no Target header cannot be applied: nothing should be guessed about
    /// where files go on someone's headset.
    #[test]
    fn refuses_a_manifest_with_no_target() {
        let srv = Server::start(Map::new(), Opts::ranged());
        let m = Manifest::parse("add a.bin ".to_owned().as_str(), &srv.url("/m")).unwrap_or_else(
            |_| Manifest::parse("# no target\n", &srv.url("/m")).unwrap(),
        );
        let f = Fake::new("notarget", "echo ''");
        let q = Quest::new(&f.adb, None);
        assert!(matches!(
            plan(&m, &q, &Cancel::new(), &mut |_| {}),
            Err(Error::NoTarget)
        ));
        std::fs::remove_dir_all(&f.dir).ok();
    }

    /// Hashing many files must not become one adb round trip per file.
    #[test]
    fn hashes_in_batches() {
        let body = payload(8);
        let files: Vec<(String, Vec<u8>)> =
            (0..120).map(|i| (format!("f{i:03}.bin"), body.clone())).collect();
        let refs: Vec<(&str, &[u8])> =
            files.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
        let srv = Server::start(routes(&refs), Opts::ranged());
        let m = manifest_for(&srv, &refs, &[]);

        let f = hashing_adb("batch", "0000000000000000000000000000000000000000000000000000000000000000");
        let q = Quest::new(&f.adb, None);
        plan(&m, &q, &Cancel::new(), &mut |_| {}).unwrap();

        let sha_calls = f.calls().iter().filter(|c| c.contains("sha256sum")).count();
        // One probe plus a handful of batches, not 120 calls.
        assert!(sha_calls > 1, "nothing was batched");
        assert!(sha_calls <= 6, "expected a few batches, got {sha_calls} calls");
        std::fs::remove_dir_all(&f.dir).ok();
    }

    #[test]
    fn applies_deletes_then_pushes_and_finishes_with_chmod() {
        let a = payload(200);
        let srv = Server::start(routes(&[("sub/a.bin", &a)]), Opts::ranged());
        let m = manifest_for(&srv, &[("sub/a.bin", &a)], &["old.bin"]);

        let f = hashing_adb("apply", "1111111111111111111111111111111111111111111111111111111111111111");
        let q = Quest::new(&f.adb, None);
        let plan = plan(&m, &q, &Cancel::new(), &mut |_| {}).unwrap();

        let staging = tmpdir("qu_staging");
        let summary = apply(&plan, &q, ROOT, &staging, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(summary, Summary { deleted: 1, pushed: 1, skipped: 0 });

        let calls = f.calls();
        let rm = calls.iter().position(|c| c.contains("rm -rf")).expect("delete ran");
        let mkdir = calls.iter().position(|c| c.contains("mkdir -p")).expect("parent created");
        let push = calls.iter().position(|c| c.starts_with("push ")).expect("push ran");
        assert!(rm < push, "deletions come before pushes");
        assert!(mkdir < push, "the parent directory is created before pushing into it");
        assert!(
            calls.last().unwrap().contains("chmod -R 777"),
            "permissions are fixed at the end: {calls:?}"
        );
        // The staged copy is not left behind.
        assert!(std::fs::read_dir(&staging).unwrap().next().is_none());

        std::fs::remove_dir_all(&f.dir).ok();
        std::fs::remove_dir_all(staging).ok();
    }

    #[test]
    fn cancelling_stops_the_run() {
        let f = hashing_adb("cancel", "0".repeat(64).as_str());
        let q = Quest::new(&f.adb, None);
        let cancel = Cancel::new();
        cancel.cancel();
        let staging = tmpdir("qu_cancel");
        let plan = Plan {
            deletes: vec![Step {
                rel: "x".into(),
                remote: format!("{ROOT}/x"),
                url: "http://127.0.0.1:1/x".into(),
                sha256: None,
            }],
            ..Default::default()
        };
        assert!(matches!(
            apply(&plan, &q, ROOT, &staging, &cancel, &mut |_| {}),
            Err(Error::Cancelled)
        ));
        std::fs::remove_dir_all(&f.dir).ok();
        std::fs::remove_dir_all(staging).ok();
    }
}
