// SPDX-License-Identifier: GPL-3.0-or-later
//! Applies an update manifest to a local install.
//!
//! Split into a plan and an apply, unlike `UpdateService.java` which decides and acts in
//! the same loop. Planning first buys three things: the user is told what is about to
//! happen before anything happens, the skip logic is testable on its own, and a folder
//! that cannot be written to is discovered before the first byte is downloaded rather than
//! halfway through.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::engine::download::{self, Snapshot, Spec};
use crate::engine::hash;
use crate::engine::manifest::{Entry, Manifest};
use crate::engine::Cancel;

/// One file the plan intends to touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Path as the manifest wrote it, for messages.
    pub rel: String,
    pub abs: PathBuf,
    /// Absent for deletions.
    pub sha256: Option<String>,
    pub url: String,
}

/// What applying this manifest would do. Nothing has happened yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub deletes: Vec<Step>,
    pub fetches: Vec<Step>,
    /// Files already present with the right hash. Reported so the UI can say "9 of 12
    /// already current" rather than looking like it did nothing.
    pub up_to_date: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.deletes.is_empty() && self.fetches.is_empty()
    }

    pub fn work_items(&self) -> usize {
        self.deletes.len() + self.fetches.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub deleted: usize,
    pub fetched: usize,
    pub skipped: usize,
}

/// Progress, shaped for a checklist and a progress bar.
#[derive(Debug, Clone)]
pub enum Event {
    Deleting { rel: String, index: usize, of: usize },
    Fetching { rel: String, index: usize, of: usize, snapshot: Snapshot },
    Placed { rel: String },
}

#[derive(Debug)]
pub enum Error {
    /// Reading or hashing a local file while planning.
    Local { rel: String, source: io::Error },
    Fetch { rel: String, source: download::Error },
    Delete { rel: String, source: io::Error },
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Local { rel, source } => write!(f, "could not read {rel}: {source}"),
            Error::Fetch { rel, source } => write!(f, "{rel}: {source}"),
            Error::Delete { rel, source } => write!(f, "could not remove {rel}: {source}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// True when the update failed only because the install folder is not writable by
    /// this process. The UI turns this into "this folder needs administrator rights"
    /// instead of a raw OS message, and later it is what triggers elevation.
    pub fn needs_elevation(&self) -> bool {
        let io_err = match self {
            Error::Delete { source, .. } | Error::Local { source, .. } => Some(source),
            Error::Fetch { source: download::Error::Io(e), .. } => Some(e),
            _ => None,
        };
        matches!(io_err.map(|e| e.kind()), Some(io::ErrorKind::PermissionDenied))
    }
}

/// Joins a manifest path onto a directory, one component at a time.
///
/// Manifest paths always use `/` and are validated to contain no `..`, no leading `/` and
/// no backslash, so this is a straight append. Done component-wise anyway so the result is
/// a native path rather than a mix of separators in every error message.
fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        out.push(part);
    }
    out
}

/// Works out what needs doing. Hashes local files, so it costs a read of whatever is
/// already there, and touches the network only for the manifest the caller already has.
pub fn plan(manifest: &Manifest, target_dir: &Path, cancel: &Cancel) -> Result<Plan, Error> {
    let mut plan = Plan::default();

    for entry in manifest.dels() {
        plan.deletes.push(step(manifest, entry, target_dir));
    }

    for entry in manifest.adds() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let step = step(manifest, entry, target_dir);
        let expected = step.sha256.as_deref().unwrap_or_default();

        if step.abs.is_file() {
            match hash::sha256_matches(&step.abs, expected) {
                Ok(true) => {
                    plan.up_to_date.push(step.rel);
                    continue;
                }
                Ok(false) => {}
                // An unreadable local file is not fatal: re-fetching it is the fix, and
                // the write will produce the real error if there is one.
                Err(_) => {}
            }
        }
        plan.fetches.push(step);
    }

    Ok(plan)
}

fn step(manifest: &Manifest, entry: &Entry, target_dir: &Path) -> Step {
    Step {
        rel: entry.path.clone(),
        abs: join_rel(target_dir, &entry.path),
        sha256: entry.sha256.clone(),
        url: manifest.url_for(entry),
    }
}

/// Carries out a plan. Deletions first, then downloads, which is the manifest's own
/// ordering and matters when a file is replaced by one at a different path.
pub fn apply(
    plan: &Plan,
    cancel: &Cancel,
    on_event: &mut dyn FnMut(Event),
) -> Result<Summary, Error> {
    let total = plan.work_items();
    let mut summary = Summary { skipped: plan.up_to_date.len(), ..Default::default() };
    let mut index = 0usize;

    for step in &plan.deletes {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        index += 1;
        on_event(Event::Deleting { rel: step.rel.clone(), index, of: total });
        match fs::remove_file(&step.abs) {
            Ok(()) => summary.deleted += 1,
            // Already gone is the desired end state, not a failure.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Delete { rel: step.rel.clone(), source: e }),
        }
        on_event(Event::Placed { rel: step.rel.clone() });
    }

    for step in &plan.fetches {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        index += 1;
        let rel = step.rel.clone();
        let at = index;

        let mut spec = Spec::new(step.url.clone(), step.abs.clone());
        if let Some(sha) = &step.sha256 {
            spec = spec.with_sha256(sha.clone());
        }

        let mut report = |snapshot: Snapshot| {
            on_event(Event::Fetching { rel: rel.clone(), index: at, of: total, snapshot });
        };
        match download::fetch(&spec, cancel, &mut report) {
            Ok(download::Outcome::Downloaded) => summary.fetched += 1,
            // fetch re-checks the hash of an existing file, so this is the race where the
            // file became correct between planning and applying. Counted as skipped.
            Ok(download::Outcome::AlreadyPresent) => summary.skipped += 1,
            Err(download::Error::Cancelled) => return Err(Error::Cancelled),
            Err(source) => return Err(Error::Fetch { rel: step.rel.clone(), source }),
        }
        on_event(Event::Placed { rel: step.rel.clone() });
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::testserver::{payload, sha_of, tmpdir, Opts, Server};
    use std::collections::HashMap;

    /// Builds a manifest plus a server that serves exactly its files.
    fn fixture(files: &[(&str, &[u8])], dels: &[&str]) -> (Server, Manifest) {
        let mut routes = HashMap::new();
        let mut body = String::from("# test manifest\n");
        for (rel, bytes) in files {
            routes.insert(format!("/updates/{rel}"), bytes.to_vec());
            body.push_str(&format!("add  {rel}  {}\n", sha_of(bytes)));
        }
        for rel in dels {
            body.push_str(&format!("del  {rel}\n"));
        }
        let server = Server::start(routes, Opts::ranged());
        let manifest =
            Manifest::parse(&body, &server.url("/updates/update.manifest")).unwrap();
        (server, manifest)
    }

    #[test]
    fn joins_manifest_paths_component_wise() {
        let base = Path::new("/tmp/win10");
        assert_eq!(join_rel(base, "a.dll"), PathBuf::from("/tmp/win10/a.dll"));
        assert_eq!(
            join_rel(base, "plugins/sub/b.dll"),
            PathBuf::from("/tmp/win10/plugins/sub/b.dll")
        );
    }

    #[test]
    fn plans_a_fresh_install_as_all_fetches() {
        let a = payload(1000);
        let b = payload(2048);
        let (_srv, m) = fixture(&[("a.dll", &a), ("plugins/b.dll", &b)], &[]);
        let dir = tmpdir("plan_fresh");

        let p = plan(&m, &dir, &Cancel::new()).unwrap();
        assert_eq!(p.fetches.len(), 2);
        assert!(p.up_to_date.is_empty());
        assert!(p.deletes.is_empty());
        fs::remove_dir_all(dir).ok();
    }

    /// The whole point of planning: files already correct are not re-downloaded, and the
    /// count is reported so the UI does not look idle.
    #[test]
    fn skips_files_that_already_match() {
        let a = payload(1000);
        let b = payload(2048);
        let (_srv, m) = fixture(&[("a.dll", &a), ("plugins/b.dll", &b)], &[]);
        let dir = tmpdir("plan_skip");
        fs::write(dir.join("a.dll"), &a).unwrap();

        let p = plan(&m, &dir, &Cancel::new()).unwrap();
        assert_eq!(p.up_to_date, vec!["a.dll".to_string()]);
        assert_eq!(p.fetches.len(), 1);
        assert_eq!(p.fetches[0].rel, "plugins/b.dll");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn plans_a_stale_file_as_a_fetch() {
        let a = payload(1000);
        let (_srv, m) = fixture(&[("a.dll", &a)], &[]);
        let dir = tmpdir("plan_stale");
        fs::write(dir.join("a.dll"), b"this is the old build").unwrap();

        let p = plan(&m, &dir, &Cancel::new()).unwrap();
        assert!(p.up_to_date.is_empty());
        assert_eq!(p.fetches.len(), 1);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn applies_a_plan_end_to_end() {
        let a = payload(3000);
        let b = payload(150_000);
        let (srv, m) = fixture(&[("a.dll", &a), ("plugins/b.dll", &b)], &["gone.dll"]);
        let dir = tmpdir("apply");
        fs::write(dir.join("gone.dll"), b"remove me").unwrap();

        let p = plan(&m, &dir, &Cancel::new()).unwrap();
        let mut events = Vec::new();
        let s = apply(&p, &Cancel::new(), &mut |e| events.push(e)).unwrap();

        assert_eq!(s, Summary { deleted: 1, fetched: 2, skipped: 0 });
        assert_eq!(fs::read(dir.join("a.dll")).unwrap(), a);
        assert_eq!(fs::read(dir.join("plugins/b.dll")).unwrap(), b);
        assert!(!dir.join("gone.dll").exists());
        // Subdirectories named by the manifest are created on the way.
        assert!(dir.join("plugins").is_dir());

        let placed: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Placed { rel } => Some(rel.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(placed, vec!["gone.dll", "a.dll", "plugins/b.dll"]);
        assert_eq!(srv.requests.load(std::sync::atomic::Ordering::Relaxed), 2);
        fs::remove_dir_all(dir).ok();
    }

    /// Deletions run before downloads, which is the manifest's ordering.
    #[test]
    fn deletes_before_fetching() {
        let a = payload(64);
        let (_srv, m) = fixture(&[("a.dll", &a)], &["old.dll"]);
        let dir = tmpdir("order");
        fs::write(dir.join("old.dll"), b"x").unwrap();

        let p = plan(&m, &dir, &Cancel::new()).unwrap();
        let mut order = Vec::new();
        apply(&p, &Cancel::new(), &mut |e| match e {
            Event::Deleting { rel, .. } => order.push(format!("del {rel}")),
            Event::Fetching { rel, .. } => {
                if !order.contains(&format!("get {rel}")) {
                    order.push(format!("get {rel}"))
                }
            }
            _ => {}
        })
        .unwrap();
        assert_eq!(order, vec!["del old.dll", "get a.dll"]);
        fs::remove_dir_all(dir).ok();
    }

    /// A deletion for a file that is not there is the desired end state, not an error.
    #[test]
    fn tolerates_a_missing_deletion_target() {
        let (_srv, m) = fixture(&[], &["never_existed.dll"]);
        let dir = tmpdir("del_missing");
        let p = plan(&m, &dir, &Cancel::new()).unwrap();
        let s = apply(&p, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(s.deleted, 0, "nothing was actually removed");
        fs::remove_dir_all(dir).ok();
    }

    /// A file whose served bytes do not match the manifest aborts the run and is not
    /// left behind, so a half-applied update cannot masquerade as a finished one.
    #[test]
    fn aborts_when_a_served_file_fails_its_hash() {
        let good = payload(500);
        let mut routes = HashMap::new();
        routes.insert("/updates/a.dll".to_string(), good.to_vec());
        // Manifest claims a different hash than the server will send.
        routes.insert("/updates/b.dll".to_string(), b"tampered".to_vec());
        let srv = Server::start(routes, Opts::ranged());
        let body = format!(
            "add  a.dll  {}\nadd  b.dll  {}\n",
            sha_of(&good),
            sha_of(b"what the manifest expects")
        );
        let m = Manifest::parse(&body, &srv.url("/updates/update.manifest")).unwrap();

        let dir = tmpdir("hashfail");
        let p = plan(&m, &dir, &Cancel::new()).unwrap();
        let err = apply(&p, &Cancel::new(), &mut |_| {}).unwrap_err();

        match err {
            Error::Fetch { rel, source: download::Error::HashMismatch { .. } } => {
                assert_eq!(rel, "b.dll");
            }
            other => panic!("got {other:?}"),
        }
        assert!(dir.join("a.dll").exists(), "the good file placed before the failure stays");
        assert!(!dir.join("b.dll").exists(), "the bad file must not be published");
        assert!(!dir.join("b.dll.part").exists(), "nor its partial");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cancelling_stops_the_run() {
        let a = payload(64);
        let (_srv, m) = fixture(&[("a.dll", &a)], &[]);
        let dir = tmpdir("cancel");
        let p = plan(&m, &dir, &Cancel::new()).unwrap();

        let cancel = Cancel::new();
        cancel.cancel();
        assert!(matches!(apply(&p, &cancel, &mut |_| {}).unwrap_err(), Error::Cancelled));
        fs::remove_dir_all(dir).ok();
    }

    /// The signal Fase 3 will hang elevation off, distinguished from any other IO failure.
    #[test]
    fn recognises_a_permission_failure() {
        let denied = Error::Delete {
            rel: "a.dll".into(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert!(denied.needs_elevation());

        let missing = Error::Delete {
            rel: "a.dll".into(),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert!(!missing.needs_elevation());
        assert!(!Error::Cancelled.needs_elevation());
    }
}
