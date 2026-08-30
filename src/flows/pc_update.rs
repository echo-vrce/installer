// SPDX-License-Identifier: GPL-3.0-or-later
//! Update Echo VR on PC: the first flow that does real work.
//!
//! Three steps. The path is typed by the user and only inspected, never corrected. The
//! update runs on a worker thread and reports through a channel, because egui's frame loop
//! must not block: a 4.68 GiB archive or a stalled mirror would otherwise freeze the
//! window.
//!
//! Note that mirror selection does not apply here. Manifest entries resolve against the
//! manifest's own location, so an update run always talks to the host the manifest came
//! from.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use egui::{RichText, Ui};

use crate::endpoints;
use crate::engine::download::Snapshot;
use crate::engine::install::{self, Inspection};
use crate::engine::manifest::Manifest;
use crate::engine::update::{self, Plan, Summary};
use crate::engine::Cancel;
use crate::flows::{Flow, Signals};
use crate::fmt::human_bytes;
use crate::theme;
use crate::widgets::{self, RowState, Status};

const STEPS: &[&str] = &["Install path", "Update", "Done"];
/// Sent from the worker thread to the UI.
enum Msg {
    Log(String),
    Planned(Plan),
    Progress { rel: String, index: usize, of: usize, snapshot: Option<Snapshot> },
    ItemDone(String),
    Finished(Result<Summary, String>),
    /// The error was a permission failure, so the message can name the real cause.
    NeedsElevation,
}

#[derive(Debug, PartialEq)]
enum Phase {
    Idle,
    Running,
    Succeeded,
    Failed,
}

pub struct PcUpdate {
    path: String,
    /// Where the prefilled folder came from, or None when it is just the fallback. Shown
    /// under the field: a suggestion whose reasoning is invisible is the app deciding.
    path_note: Option<&'static str>,
    inspection: Inspection,
    phase: Phase,
    cancel: Cancel,
    rx: Option<Receiver<Msg>>,
    log: crate::log::Ring,
    log_open: bool,
    plan: Option<Plan>,
    current: Option<(String, usize, usize, Option<Snapshot>)>,
    finished: Vec<String>,
    summary: Option<Summary>,
    error: Option<String>,
    needs_elevation: bool,
    /// Stopped on purpose, so the idle screen says so instead of looking untouched.
    stopped: bool,
    elevated: crate::flows::elevated::Elevated,
}

impl Default for PcUpdate {
    fn default() -> Self {
        // A default that is honest about being a guess: it is prefilled so the field is
        // not empty, and immediately inspected so the user sees what is actually there.
        let (path, path_note) = default_path();
        let inspection = install::inspect(std::path::Path::new(&path));
        PcUpdate {
            path,
            path_note,
            inspection,
            phase: Phase::Idle,
            cancel: Cancel::new(),
            rx: None,
            log: crate::log::Ring::default(),
            log_open: false,
            plan: None,
            current: None,
            finished: Vec::new(),
            summary: None,
            error: None,
            needs_elevation: false,
            stopped: false,
            elevated: Default::default(),
        }
    }
}

fn default_path() -> (String, Option<&'static str>) {
    crate::config::suggested_update_path(guessed_path)
}

/// Only used the first time, before there is anything to remember.
fn guessed_path() -> String {
    if cfg!(windows) {
        "C:\\EchoVR".to_string()
    } else {
        // Only so the flow is exercisable on a dev box; PC Echo is Windows only.
        format!("{}/EchoVR", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
    }
}

impl PcUpdate {
    fn reinspect(&mut self) {
        self.inspection = install::inspect(std::path::Path::new(&self.path));
    }

    /// Hands the same operation to an elevated copy of this executable.
    ///
    /// Deliberately the CLI command a person could type: the elevated run is not a special
    /// path through the code, which is what makes it debuggable when it goes wrong.
    /// Turns the elevated run's progress into the state an ordinary run would have left,
    /// so the same widgets draw it and nothing downstream knows the difference.
    fn absorb_elevated(&mut self, e: crate::cli::Event) {
        use crate::cli::Event;
        match e {
            Event::Item { name, index, of } => {
                if let Some((rel, ..)) = &self.current {
                    // The one before it finished, or it would not have moved on.
                    let done = rel.clone();
                    if !self.finished.contains(&done) {
                        self.finished.push(done);
                    }
                }
                self.current = Some((name, index, of, None));
            }
            Event::Progress { done, total, .. } => {
                if let Some((rel, index, of, _)) = &self.current {
                    let snap = Snapshot { done, total, bytes_per_sec: 0.0, attempt: 0 };
                    self.current = Some((rel.clone(), *index, *of, Some(snap)));
                }
            }
            Event::Stage(s) => self.log.push(s),
            Event::Done { .. } => self.current = None,
        }
    }

    fn start_elevated(&mut self) {
        // Recorded when the run starts, not when it succeeds. A failed install is exactly
        // the case that leaves a 4.68 GB archive behind, so that is the one the cache
        // cleaner most needs to know the folder of.
        crate::config::remember_install_path(&self.path);
        self.error = None;
        self.phase = Phase::Running;
        self.log.clear();
        // The failed attempt's plan and its half-ticked checklist belong to a run that is
        // over; leaving them on screen would show stale states beside live log lines.
        self.plan = None;
        self.current = None;
        self.finished.clear();
        self.summary = None;
        self.log.push("asking Windows for administrator rights".into());
        self.elevated.start(vec!["update".into(), "--path".into(), self.path.clone()]);
    }

    fn start(&mut self) {
        self.stopped = false;
        // Recorded when the run starts, not when it succeeds. A failed install is exactly
        // the case that leaves a 4.68 GB archive behind, so that is the one the cache
        // cleaner most needs to know the folder of.
        crate::config::remember_install_path(&self.path);
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.phase = Phase::Running;
        self.cancel = Cancel::new();
        self.log.clear();
        self.plan = None;
        self.current = None;
        self.finished.clear();
        self.summary = None;
        self.error = None;
        self.needs_elevation = false;

        let target = install::bin_dir(std::path::Path::new(&self.path));
        let cancel = self.cancel.clone();

        thread::spawn(move || run(target, cancel, tx));
    }

    /// Drains whatever the worker has sent since the last frame.
    ///
    /// Collected first and handled second: holding the receiver borrowed while dispatching
    /// would mean nothing in the handlers could touch `self`.
    fn pump(&mut self) {
        // The elevated run is a separate channel: it produces log lines and one verdict,
        // and nothing else in this flow knows or cares that it is a different process.
        for update in self.elevated.poll() {
            match update {
                crate::flows::elevated::Update::Line(l) => self.log.push(l),
                crate::flows::elevated::Update::Event(e) => self.absorb_elevated(e),
                crate::flows::elevated::Update::Finished => {
                    self.phase = Phase::Succeeded;
                    self.needs_elevation = false;
                    // The elevated copy did the work, so this side has no counts of its
                    // own. Re-inspecting is what makes the result trustworthy rather than
                    // reported.
                    self.reinspect();
                    self.log.push("the elevated run finished".into());
                }
                crate::flows::elevated::Update::Failed(e) => {
                    self.phase = Phase::Failed;
                    self.error = Some(e);
                }
            }
        }

        let (inbox, disconnected) = crate::channel::drain(&self.rx);

        for msg in inbox {
            match msg {
                Msg::Log(line) => self.log.push(line),
                Msg::Planned(plan) => self.plan = Some(plan),
                Msg::Progress { rel, index, of, snapshot } => {
                    self.current = Some((rel, index, of, snapshot))
                }
                Msg::ItemDone(rel) => {
                    self.finished.push(rel);
                    self.current = None;
                }
                Msg::NeedsElevation => self.needs_elevation = true,
                Msg::Finished(result) => match result {
                    Ok(summary) => {
                        self.summary = Some(summary);
                        self.phase = Phase::Succeeded;
                    }
                    Err(e) => {
                        self.error = Some(e);
                        self.phase = Phase::Failed;
                    }
                },
            }
        }

        if disconnected {
            self.rx = None;
            // The thread ended without a verdict, which should not happen. Treated as a
            // failure rather than leaving the step running forever.
            if self.phase == Phase::Running {
                self.error = Some("The update stopped unexpectedly.".into());
                self.phase = Phase::Failed;
            }
        }
    }
}

/// The worker. Fetch, plan, apply, and say what happened.
fn run(target: PathBuf, cancel: Cancel, tx: mpsc::Sender<Msg>) {
    let say = |line: String| {
        let _ = tx.send(Msg::Log(line));
    };

    say(format!("target {}", target.display()));
    say(format!("fetching {}", endpoints::PC_MANIFEST));

    let text = match crate::engine::download::fetch_text_cancellable(endpoints::PC_MANIFEST, &cancel, &mut |_, _| {}) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Msg::Finished(Err(format!(
                "Could not download the update list: {e}"
            ))));
            return;
        }
    };
    let manifest = match Manifest::parse(&text, endpoints::PC_MANIFEST) {
        Ok(m) => m,
        Err(e) => {
            // A manifest that fails validation is refused rather than partially applied.
            let _ = tx.send(Msg::Finished(Err(format!("The update list is not valid: {e}"))));
            return;
        }
    };
    say(format!("manifest ok, {} entries", manifest.entries().len()));

    let plan = match update::plan(&manifest, &target, &cancel) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Msg::Finished(Err(e.to_string())));
            return;
        }
    };
    say(format!(
        "plan: {} to fetch, {} to delete, {} already current",
        plan.fetches.len(),
        plan.deletes.len(),
        plan.up_to_date.len()
    ));
    let _ = tx.send(Msg::Planned(plan.clone()));

    let tx2 = tx.clone();
    let mut on_event = |event: update::Event| match event {
        update::Event::Deleting { rel, index, of } => {
            let _ = tx2.send(Msg::Progress { rel, index, of, snapshot: None });
        }
        update::Event::Fetching { rel, index, of, snapshot } => {
            let _ = tx2.send(Msg::Progress { rel, index, of, snapshot: Some(snapshot) });
        }
        update::Event::Placed { rel } => {
            let _ = tx2.send(Msg::ItemDone(rel));
        }
    };

    match update::apply(&plan, &cancel, &mut on_event) {
        Ok(summary) => {
            say(format!(
                "done: {} fetched, {} deleted, {} skipped",
                summary.fetched, summary.deleted, summary.skipped
            ));
            let _ = tx.send(Msg::Finished(Ok(summary)));
        }
        Err(e) => {
            if e.needs_elevation() {
                let _ = tx.send(Msg::NeedsElevation);
            }
            say(format!("failed: {e}"));
            let _ = tx.send(Msg::Finished(Err(e.to_string())));
        }
    }
}

impl Flow for PcUpdate {
    /// Going back voids the run. Inputs are kept - the folder is what the user typed, and
    /// clearing it while they are correcting it would be its own bug.
    fn reset_after(&mut self, _step: usize) {
        self.cancel.cancel();
        self.rx = None;
        self.elevated.forget();
        self.phase = Phase::Idle;
        self.plan = None;
        self.current = None;
        self.finished.clear();
        self.summary = None;
        self.error = None;
        self.needs_elevation = false;
        self.log.clear();
        self.reinspect();
    }

    fn steps(&self) -> &'static [&'static str] {
        STEPS
    }

    /// The folder step is the commit: past it, files get written. Only asked when the
    /// folder is not what an update expects, which is when the answer is not obvious.
    fn confirm_advance(&self, step: usize) -> Option<crate::flows::Confirm> {
        if step != 0 {
            return None;
        }
        if !self.inspection.root_exists {
            return Some(crate::flows::Confirm {
                title: "That folder does not exist".into(),
                consequence: format!(
                    "Continuing will create {} and download the update into it.\n\n\
                     An update is not an install: the result will be the patched files \
                     with no game around them. To install Echo VR from nothing, go back \
                     and use Install Echo VR (PC) instead.",
                    self.path
                ),
                proceed: "Create it and update".into(),
            });
        }
        if !self.inspection.has_echo {
            return Some(crate::flows::Confirm {
                title: "No Echo VR found in that folder".into(),
                consequence: format!(
                    "There is no echovr.exe in {}.\n\n\
                     Continuing will write the update's files there anyway. If this is the \
                     wrong folder, nothing will warn you again.",
                    self.path
                ),
                proceed: "Update it anyway".into(),
            });
        }
        None
    }

    fn blocked_reason(&self, step: usize) -> Option<String> {
        match step {
            0 if self.path.trim().is_empty() => Some("Enter your Echo VR folder".into()),
            1 => match self.phase {
                Phase::Running => Some("Update in progress".into()),
                Phase::Succeeded => None,
                _ => Some("Run the update first".into()),
            },
            _ => None,
        }
    }

    fn on_exit(&mut self) {
        self.cancel.cancel();
    }

    fn content(&mut self, ui: &mut Ui, step: usize, signals: &mut Signals) {
        match step {
            0 => self.step_path(ui),
            1 => self.step_update(ui, signals),
            _ => self.step_done(ui, signals),
        }
    }
}

impl PcUpdate {
    fn step_path(&mut self, ui: &mut Ui) {
        widgets::field_label(ui, "Echo VR folder");
        if let Some(note) = self.path_note {
            ui.label(
                egui::RichText::new(note).font(theme::font_ui(10.5)).color(theme::TEXT_FAINT),
            );
            ui.add_space(2.0);
        }
        let mut edited = false;
        ui.horizontal(|ui| {
            let field_w = (ui.available_width() - 96.0).clamp(180.0, 380.0);
            if widgets::path_field(ui, &mut self.path, field_w).changed() {
                edited = true;
            }
            ui.add_space(theme::UNIT * 0.75);
            if widgets::secondary(ui, "Browse", true) {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.path = dir.to_string_lossy().into_owned();
                    edited = true;
                }
            }
        });
        if edited {
            if let Some(note) = crate::flows::adopt_install_root(&mut self.path) {
                self.path_note = Some(note);
            }
            // Once they have typed, the note is about a path that is no longer in the box.
            self.path_note = None;
            self.reinspect();
        }
        ui.add_space(theme::UNIT * 1.5);

        // Informative, never blocking: the user may continue past every one of these.
        // The folder they typed is the folder we use.
        if self.inspection.has_echo {
            widgets::status(ui, Status::Ok, "echovr.exe found at this path");
        } else if self.inspection.root_exists {
            widgets::status(ui, Status::Warn, "no echovr.exe here, so there may be nothing to update");
        } else {
            // Short here on purpose. This is the glance that tells you something is off;
            // what continuing would actually do belongs at the moment you commit to it, in
            // the confirmation, not on the page you are still reading.
            widgets::status(ui, Status::Err, "this folder does not exist");
        }

        if self.inspection.root_exists && !self.inspection.writable {
            widgets::status(ui, Status::Warn, "this folder needs administrator rights to write to");
        }
        if let Some(free) = self.inspection.free_bytes {
            widgets::status(ui, Status::Info, &format!("{} free", human_bytes(free)));
        }

        ui.add_space(theme::UNIT * 1.5);
        widgets::mono_color(
            ui,
            &install::bin_dir(std::path::Path::new(&self.path)).display().to_string(),
            10.5,
            theme::TEXT_FAINT,
        );
        ui.label(
            RichText::new("Update files are placed here")
                .font(theme::font_ui(10.5))
                .color(theme::TEXT_FAINT),
        );
    }

    fn step_update(&mut self, ui: &mut Ui, signals: &mut Signals) {
        self.pump();
        if self.phase == Phase::Running {
            signals.keep_repainting = true;
        }

        match self.phase {
            Phase::Idle => {
                if self.stopped {
                    widgets::status(
                        ui,
                        Status::Info,
                        "Stopped. What downloaded is kept, so starting again carries on \
                         from where it left off.",
                    );
                    ui.add_space(theme::UNIT);
                }
                ui.label(
                    RichText::new(
                        "Checks the current update list, then downloads only the files that \
                         are missing or out of date.",
                    )
                    .font(theme::font_ui(12.0))
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(theme::UNIT * 1.5);
                if widgets::primary(ui, "Start update", true) {
                    self.start();
                }
            }
            _ => self.render_run(ui, signals),
        }
    }

    fn render_run(&mut self, ui: &mut Ui, _signals: &mut Signals) {
        let plan = self.plan.clone();
        let finished = self.finished.clone();
        let current = self.current.clone();

        let elevated = self.elevated.running();
        if elevated {
            // The checklist below is driven by events the elevated run sends, so it fills
            // in exactly as it would otherwise. This line is the only difference.
            widgets::status(ui, Status::Info, "Running with administrator rights");
            ui.add_space(theme::UNIT * 0.5);
        }
        widgets::card(ui, |ui| {
            match &plan {
                // The plan belongs to whichever process made it. An elevated run made its
                // own and this side never sees it, so the count from its progress is what
                // there is - and saying "reading the update list" while files are arriving
                // would be worse than saying less.
                None => match &current {
                    Some((_, index, of, _)) => widgets::check_row(
                        ui,
                        RowState::Working,
                        &format!("File {index} of {of}"),
                    ),
                    None => widgets::check_row(ui, RowState::Working, "Reading the update list"),
                },
                Some(plan) => {
                    if plan.is_empty() {
                        widgets::check_row(ui, RowState::Done, "Everything is already up to date");
                    }
                    for step in plan.deletes.iter().chain(plan.fetches.iter()) {
                        let state = if finished.contains(&step.rel) {
                            RowState::Done
                        } else if current.as_ref().is_some_and(|(rel, ..)| rel == &step.rel) {
                            if self.phase == Phase::Failed {
                                RowState::Failed
                            } else {
                                RowState::Working
                            }
                        } else {
                            RowState::Pending
                        };
                        widgets::check_row(ui, state, &step.rel);
                    }
                    if !plan.up_to_date.is_empty() {
                        ui.add_space(theme::UNIT * 0.5);
                        ui.label(
                            RichText::new(format!(
                                "{} file{} already current",
                                plan.up_to_date.len(),
                                if plan.up_to_date.len() == 1 { "" } else { "s" }
                            ))
                            .font(theme::font_ui(11.0))
                            .color(theme::TEXT_DIM),
                        );
                    }
                }
            };
        });

        ui.add_space(theme::UNIT);

        if let Some((rel, index, of, Some(snapshot))) = &current {
            widgets::progress_row(
                ui,
                rel,
                snapshot.fraction().unwrap_or(0.0),
                &describe(*index, *of, snapshot),
            );
            ui.add_space(theme::UNIT * 0.5);
        }

        match self.phase {
            Phase::Succeeded => {
                let s = self.summary.unwrap_or_default();
                widgets::status(
                    ui,
                    Status::Ok,
                    &format!(
                        "Update applied: {} downloaded, {} removed, {} already current",
                        s.fetched, s.deleted, s.skipped
                    ),
                );
            }
            Phase::Failed => {
                let msg = self.error.clone().unwrap_or_else(|| "Update failed".into());
                widgets::status(ui, Status::Err, &msg);
                if self.needs_elevation {
                    ui.add_space(theme::UNIT * 0.5);
                    if crate::flows::elevated::Elevated::available() {
                        widgets::status(
                            ui,
                            Status::Warn,
                            "This folder needs administrator rights. Windows will ask.",
                        );
                    } else {
                        // Already elevated, or not Windows: offering the button would be
                        // a dead end, so say the other thing that could work.
                        widgets::status(
                            ui,
                            Status::Warn,
                            "This folder cannot be written to. Try installing somewhere you own.",
                        );
                    }
                }
                ui.add_space(theme::UNIT * 0.5);
                ui.horizontal(|ui| {
                    if widgets::secondary(ui, "Retry", true) {
                        self.start();
                    }
                    if self.needs_elevation && crate::flows::elevated::Elevated::available() {
                        if widgets::primary(ui, "Run as administrator", true) {
                            self.start_elevated();
                        }
                    }
                    widgets::external_link(ui, "Ask for help on Discord", endpoints::DISCORD_LOUNGE);
                });
            }
            Phase::Running => {
                ui.add_space(theme::UNIT * 0.5);
                if widgets::secondary(ui, "Cancel", true) {
                    // Both, because only one of them is doing the work and this side does
                    // not need to care which. The elevated one is asked through a file; the
                    // local one is a flag its own thread is watching.
                    self.cancel.cancel();
                    self.elevated.cancel();
                }
            }
            Phase::Idle => {}
        }

        ui.add_space(theme::UNIT);
        let mut open = self.log_open;
        widgets::log_pane(ui, &mut open, self.log.lines());
        self.log_open = open;
    }

    fn step_done(&mut self, ui: &mut Ui, _signals: &mut Signals) {
        widgets::status(ui, Status::Ok, "Echo VR is up to date");
        ui.add_space(theme::UNIT);
        let s = self.summary.unwrap_or_default();
        widgets::card(ui, |ui| {
            widgets::kv(ui, "Folder     ", &self.path);
            widgets::kv(ui, "Downloaded ", &s.fetched.to_string());
            widgets::kv(ui, "Removed    ", &s.deleted.to_string());
            widgets::kv(ui, "Unchanged  ", &s.skipped.to_string());
        });
        ui.add_space(theme::UNIT * 1.5);
        if widgets::secondary(ui, "Open folder", true) {
            let _ = widgets::open_path(&install::bin_dir(std::path::Path::new(&self.path)));
        }
    }
}

fn describe(index: usize, of: usize, s: &Snapshot) -> String {
    format!("{index}/{of}  ·  {}", crate::fmt::transfer(s))
}

#[cfg(test)]
mod tests {

    #[test]
    fn an_elevated_run_drives_the_same_progress_row() {
        // The elevated child is another process; all this side gets is events. They have to
        // land in the state the widgets already read, or the window shows nothing while
        // several gigabytes go past.
        use crate::cli::Event;
        let mut f = PcUpdate::default();

        f.absorb_elevated(Event::Item { name: "a.dll".into(), index: 1, of: 3 });
        let (rel, index, of, snap) = f.current.clone().expect("an item should become current");
        assert_eq!((rel.as_str(), index, of), ("a.dll", 1, 3));
        assert!(snap.is_none(), "nothing has moved yet");

        f.absorb_elevated(Event::Progress { what: "a.dll".into(), done: 50, total: Some(100) });
        let (_, _, _, snap) = f.current.clone().unwrap();
        let snap = snap.expect("progress should attach to the current item");
        assert_eq!((snap.done, snap.total), (50, Some(100)));

        // Moving on marks the one before as finished, which is what ticks the checklist.
        f.absorb_elevated(Event::Item { name: "b.dll".into(), index: 2, of: 3 });
        assert_eq!(f.finished, vec!["a.dll".to_string()]);
        assert_eq!(f.current.clone().unwrap().0, "b.dll");

        f.absorb_elevated(Event::Done { ok: true, summary: "2 fetched".into() });
        assert!(f.current.is_none(), "nothing is in progress once it is over");
    }

    #[test]
    fn going_back_forgets_the_previous_run() {
        // The bug this exists for: update one folder, step back, point at another, and the
        // finished screen still showed the first run's result against the second folder.
        let mut f = PcUpdate::default();
        f.phase = Phase::Succeeded;
        f.summary = Some(Summary::default());
        f.finished.push("a.dll".into());
        f.error = Some("stale".into());
        f.needs_elevation = true;
        f.log.push("from the old run".into());

        f.reset_after(0);

        assert_eq!(f.phase, Phase::Idle);
        assert!(f.summary.is_none(), "a result from another folder must not survive");
        assert!(f.finished.is_empty());
        assert!(f.error.is_none());
        assert!(!f.needs_elevation);
        assert!(f.log.is_empty());
        assert!(f.plan.is_none());
    }

    use super::*;

    #[test]
    fn blocks_continue_until_the_update_has_actually_run() {
        let mut f = PcUpdate::default();
        assert!(f.blocked_reason(1).is_some(), "idle must not let the user past");
        f.phase = Phase::Running;
        assert!(f.blocked_reason(1).is_some());
        f.phase = Phase::Failed;
        assert!(f.blocked_reason(1).is_some());
        f.phase = Phase::Succeeded;
        assert!(f.blocked_reason(1).is_none());
    }

    #[test]
    fn blocks_continue_on_an_empty_path_but_not_on_a_wrong_one() {
        let mut f = PcUpdate::default();
        f.path = "   ".into();
        assert!(f.blocked_reason(0).is_some());
        // A path that does not exist is reported, not forbidden: the user owns the path.
        f.path = "/definitely/not/here".into();
        assert!(f.blocked_reason(0).is_none());
    }

}
