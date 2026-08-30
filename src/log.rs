// SPDX-License-Identifier: GPL-3.0-or-later
//! The log that outlives the window.
//!
//! Everything a flow prints into its log pane also lands in a file, because the pane is
//! gone the moment someone closes the app - which is exactly when they go looking for it.
//! The original writes `log.log` into the current working directory, which for a portable
//! executable means a Downloads folder, a synced drive, or somewhere read-only.
//!
//! Deliberately not a logging framework. One file, one writer, flushed per line.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::fmt;

/// Runs to keep. Ten is enough to cover "it worked yesterday" without turning the folder
/// into an archive nobody asked for.
const KEEP_RUNS: usize = 10;
/// Ceiling for the whole folder. A single wedged run cannot fill a disk.
const CAP_BYTES: u64 = 8 * 1024 * 1024;
/// Ceiling for one run's file, past which lines are dropped and counted.
const CAP_ONE: u64 = 2 * 1024 * 1024;

const PREFIX: &str = "installer-";
const SUFFIX: &str = ".log";

struct Sink {
    file: File,
    path: PathBuf,
    written: u64,
    dropped: u64,
    /// Mirror to stderr. Off for the GUI, on for the CLI, where the terminal *is* the log.
    echo: bool,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Opens this run's file and prunes older ones. Returns the path, or `None` if the folder
/// could not be written - in which case every later call is a silent no-op, because a
/// failure to log is not a reason to refuse to install.
pub fn init(dir: &Path, echo: bool) -> Option<PathBuf> {
    fs::create_dir_all(dir).ok()?;
    prune(dir, KEEP_RUNS, CAP_BYTES);
    open_at(&dir.join(format!("{PREFIX}{}{SUFFIX}", fmt::utc_stamp(fmt::now_secs()))), echo)
}

/// Logs to a file somebody else chose, replacing whatever was there.
///
/// This is how an elevated run reports back: the parent names the file before the child
/// exists, then reads it as it fills. Truncated rather than appended, so the parent is not
/// reading the previous attempt's output and calling it progress.
pub fn init_at(path: &Path, echo: bool) -> Option<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok()?;
    }
    let _ = fs::remove_file(path);
    open_at(path, echo)
}

fn open_at(path: &Path, echo: bool) -> Option<PathBuf> {
    let path = path.to_path_buf();
    let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
    let mut guard = SINK.lock().ok()?;
    *guard = Some(Sink { file, path: path.clone(), written: 0, dropped: 0, echo });
    drop(guard);

    line(&format!(
        "{} {} on {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    ));
    Some(path)
}

/// Appends one line, timestamped. Flushed immediately: a log that is still in a buffer when
/// the process dies is a log that was never worth keeping.
pub fn line(msg: &str) {
    let Ok(mut guard) = SINK.lock() else { return };
    let Some(sink) = guard.as_mut() else { return };

    if sink.echo {
        eprintln!("{msg}");
    }
    if sink.written >= CAP_ONE {
        sink.dropped += 1;
        return;
    }
    let stamped = format!("{}  {msg}\n", fmt::utc_clock(fmt::now_secs()));
    if sink.file.write_all(stamped.as_bytes()).is_ok() {
        sink.written += stamped.len() as u64;
        let _ = sink.file.flush();
        if sink.written >= CAP_ONE {
            let _ = writeln!(sink.file, "-- size cap reached, further lines dropped --");
            let _ = sink.file.flush();
        }
    }
}

/// This run's file, for the button that opens it.
pub fn path() -> Option<PathBuf> {
    SINK.lock().ok()?.as_ref().map(|s| s.path.clone())
}

/// Turns a panic into a line in the log instead of a message on a console nobody sees.
///
/// A GUI build on Windows has no console at all, so without this a panic is completely
/// silent: the window vanishes and there is nothing to send anyone.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".into());
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown payload".into());
        line(&format!("PANIC at {where_}: {what}"));
        previous(info);
    }));
}

/// Keeps the newest `keep` runs, then drops more until the folder fits `cap`.
///
/// Split out and taking its limits as arguments so the policy can be tested without
/// writing eight megabytes.
fn prune(dir: &Path, keep: usize, cap: u64) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut runs: Vec<(PathBuf, u64)> = entries
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with(PREFIX) && n.ends_with(SUFFIX)
        })
        .filter_map(|e| e.metadata().ok().map(|m| (e.path(), m.len())))
        .collect();

    // The names carry a sortable UTC stamp, so this is newest-last without asking the
    // filesystem for times it may not keep accurately.
    runs.sort_by(|a, b| a.0.cmp(&b.0));

    while runs.len() > keep {
        let (path, _) = runs.remove(0);
        let _ = fs::remove_file(path);
    }
    let mut total: u64 = runs.iter().map(|(_, n)| *n).sum();
    while total > cap && !runs.is_empty() {
        let (path, size) = runs.remove(0);
        let _ = fs::remove_file(path);
        total -= size;
    }
}

/// The in-memory tail a flow shows in its log pane, teed to the file on the way in.
///
/// One type instead of the identical `push_log` each flow used to carry: they had already
/// been copied five times, which is five chances for them to disagree later.
#[derive(Debug, Clone)]
pub struct Ring {
    lines: Vec<String>,
    limit: usize,
}

impl Default for Ring {
    fn default() -> Self {
        Ring { lines: Vec::new(), limit: 400 }
    }
}

impl Ring {
    pub fn push(&mut self, text: String) {
        line(&text);
        if self.lines.len() >= self.limit {
            self.lines.remove(0);
        }
        self.lines.push(text);
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str, bytes: usize) {
        fs::write(dir.join(name), vec![b'x'; bytes]).unwrap();
    }

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("evrce-log-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn prune_keeps_the_newest_runs() {
        let dir = scratch("keep");
        for d in 1..=5 {
            touch(&dir, &format!("installer-2026010{d}-000000Z.log"), 10);
        }
        prune(&dir, 2, u64::MAX);
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec![
            "installer-20260104-000000Z.log".to_string(),
            "installer-20260105-000000Z.log".to_string(),
        ]);
    }

    #[test]
    fn prune_enforces_the_folder_cap_after_the_count() {
        let dir = scratch("cap");
        for d in 1..=4 {
            touch(&dir, &format!("installer-2026010{d}-000000Z.log"), 100);
        }
        // Count allows all four; the cap only leaves room for two.
        prune(&dir, 10, 250);
        let left = fs::read_dir(&dir).unwrap().flatten().count();
        assert_eq!(left, 2);
    }

    #[test]
    fn prune_ignores_files_it_did_not_write() {
        let dir = scratch("other");
        touch(&dir, "installer-20260101-000000Z.log", 10);
        touch(&dir, "echo-logs-20260101-0000Z.zip", 10);
        touch(&dir, "notes.txt", 10);
        prune(&dir, 0, 0);
        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["echo-logs-20260101-0000Z.zip".to_string(), "notes.txt".to_string()]);
    }

    #[test]
    fn ring_keeps_the_tail_and_drops_the_head() {
        let mut r = Ring { lines: Vec::new(), limit: 3 };
        for i in 0..5 {
            r.push(format!("line {i}"));
        }
        assert_eq!(r.len(), 3);
        assert_eq!(r.lines(), ["line 2", "line 3", "line 4"]);
        r.clear();
        assert!(r.is_empty());
    }

    #[test]
    fn writing_without_init_is_a_no_op_not_a_panic() {
        // The sink is process-global and other tests may have opened it; the point here is
        // only that this call cannot bring the process down.
        line("a line from a test");
    }
}
