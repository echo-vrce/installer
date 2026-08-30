// SPDX-License-Identifier: GPL-3.0-or-later
//! Driving an elevated re-run from the window, and showing what it is doing.
//!
//! The hard part is not starting the elevated process; it is that the child has its own
//! console, so from here it is a black box that exits with a number. Waiting on that with
//! nothing on screen is the sort of freeze people kill the app during.
//!
//! So the child is told to log to a file this side names, and this side reads that file as
//! it fills. The lines it produces are the same lines the flow would have logged itself,
//! which means they land in the existing log pane and nothing new had to be designed.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::config;
use crate::engine::elevate;

/// How often the parent looks for new lines. Fast enough to feel live, slow enough that it
/// is not a spin loop on a file that mostly is not changing.
const POLL: Duration = Duration::from_millis(250);

enum Msg {
    Line(String),
    Done(Result<(), String>),
}

#[derive(Default)]
pub struct Elevated {
    rx: Option<Receiver<Msg>>,
    running: bool,
}

/// What came back this frame.
pub enum Update {
    /// A line the child wrote. Push it into the flow's log.
    Line(String),
    /// Progress the child reported in a form worth drawing. The same shapes an ordinary
    /// run produces, so a flow can feed them into the widgets it already has.
    Event(crate::cli::Event),
    Finished,
    Failed(String),
}

impl Elevated {
    pub fn running(&self) -> bool {
        self.running
    }

    /// Only worth offering when it could actually help.
    pub fn available() -> bool {
        cfg!(windows) && !elevate::is_elevated()
    }

    /// Starts the elevated run. `command` is the CLI command and its arguments, without
    /// the log file, which [`elevate::args_for`] adds.
    pub fn start(&mut self, command: Vec<String>) {
        if self.running {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.running = true;

        let log_file: PathBuf = elevate::log_path(&config::logs_dir());
        thread::spawn(move || {
            let borrowed: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
            let args = elevate::args_for(&borrowed, &log_file);

            // Start clean, so what gets read back is this run and not the last one - and
            // so a cancel left over from a previous run does not stop this one immediately.
            let _ = std::fs::remove_file(&log_file);
            let _ = std::fs::remove_file(elevate::cancel_path(&log_file));

            let stop = Arc::new(AtomicBool::new(false));
            let tail_stop = stop.clone();
            let tail_tx = tx.clone();
            let tail_path = log_file.clone();
            let tailer = thread::spawn(move || {
                let mut offset = 0u64;
                loop {
                    let done = tail_stop.load(Ordering::Relaxed);
                    offset = drain(&tail_path, offset, &tail_tx);
                    if done {
                        // One last pass after the child exited, so the final lines are not
                        // lost to the race between it writing and us noticing.
                        break;
                    }
                    thread::sleep(POLL);
                }
            });

            let outcome = elevate::run_elevated(&args);
            stop.store(true, Ordering::Relaxed);
            let _ = tailer.join();

            let result = match outcome {
                Ok(0) => Ok(()),
                Ok(code) => Err(elevate::Error::Failed { code }.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(Msg::Done(result));
        });
    }

    /// Asks the elevated run to stop.
    pub fn cancel(&self) {
        if !self.running {
            return;
        }
        let path = elevate::cancel_path(&elevate::log_path(&config::logs_dir()));
        let _ = std::fs::write(path, b"stop");
    }

    /// Stops listening to an elevated run.
    ///
    /// The child is not killed: it holds administrator rights and may be halfway through
    /// writing a file, and killing it there is how a half-installed folder happens. It runs
    /// to completion and its log is still on disk; this side simply stops reporting it.
    pub fn forget(&mut self) {
        self.rx = None;
        self.running = false;
    }

    /// Drains what arrived since the last frame.
    pub fn poll(&mut self) -> Vec<Update> {
        let mut out = Vec::new();
        let (inbox, _) = crate::channel::drain(&self.rx);
        for msg in inbox {
            match msg {
                // Two kinds share the file: objects for the window to draw, sentences for
                // whoever reads the log afterwards. Sorted here so no flow has to know.
                Msg::Line(l) => match crate::cli::Event::parse(&l) {
                    Some(e) => out.push(Update::Event(e)),
                    None => out.push(Update::Line(l)),
                },
                Msg::Done(Ok(())) => {
                    self.running = false;
                    self.rx = None;
                    out.push(Update::Finished);
                }
                Msg::Done(Err(e)) => {
                    self.running = false;
                    self.rx = None;
                    out.push(Update::Failed(e));
                }
            }
        }
        out
    }
}

/// Reads whole lines added since `offset`, returning the new offset.
///
/// Stops at the last newline rather than the end of the file: a line still being written is
/// read next time, not shown half finished.
fn drain(path: &PathBuf, offset: u64, tx: &mpsc::Sender<Msg>) -> u64 {
    let Ok(mut file) = std::fs::File::open(path) else { return offset };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return offset;
    }
    let Some(last_newline) = buf.rfind('\n') else { return offset };
    let complete = &buf[..=last_newline];
    for line in complete.lines() {
        if !line.trim().is_empty() {
            let _ = tx.send(Msg::Line(line.to_string()));
        }
    }
    offset + complete.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("evrce-elev-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn reads_only_whole_lines_and_resumes_where_it_stopped() {
        let dir = scratch("tail");
        let path = dir.join("elevated.log");
        let (tx, rx) = mpsc::channel();

        std::fs::write(&path, "one\ntwo\nthr").unwrap();
        let offset = drain(&path, 0, &tx);
        let first: Vec<String> = rx.try_iter().filter_map(|m| match m {
            Msg::Line(l) => Some(l),
            _ => None,
        }).collect();
        // "thr" has no newline yet: it is a line in progress, not a line.
        assert_eq!(first, vec!["one", "two"]);

        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        drain(&path, offset, &tx);
        let second: Vec<String> = rx.try_iter().filter_map(|m| match m {
            Msg::Line(l) => Some(l),
            _ => None,
        }).collect();
        assert_eq!(second, vec!["three"], "the earlier lines must not repeat");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        // The child may not have created it yet when the first poll lands.
        let (tx, _rx) = mpsc::channel();
        assert_eq!(drain(&PathBuf::from("/nonexistent/elevated.log"), 0, &tx), 0);
    }
}
