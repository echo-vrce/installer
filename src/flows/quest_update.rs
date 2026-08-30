// SPDX-License-Identifier: GPL-3.0-or-later
//! Update Echo VR on a Quest.
//!
//! Four steps. The version check is a step of its own rather than something that happens
//! silently on entry, because when it refuses it needs to explain itself with the evidence
//! in view: what the headset holds, what the update expects, and why they do not match.

use std::sync::mpsc::{self, Receiver};
use std::thread;

use egui::{RichText, Ui};

use crate::config::{self, Settings};
use crate::endpoints;
use crate::engine::adb::{self, Adb, Device};
use crate::engine::watch::DeviceWatcher;
use crate::engine::download;
use crate::engine::manifest::Manifest;
use crate::engine::quest::{self, Marker, Quest, Verdict};
use crate::engine::quest_update::{self as engine, Plan, Summary};
use crate::engine::Cancel;
use crate::fmt::human_bytes;
use crate::theme;
use crate::widgets::{self, RowState, Status};

const STEPS: &[&str] = &["Connect", "Version check", "Update", "Done"];
#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Running,
    Succeeded,
    Failed,
}

/// What the version check concluded, with the evidence that led there.
struct Checked {
    verdict: Verdict,
    marker: Option<Marker>,
    installed_sha: Option<String>,
    manifest_base: Option<String>,
    self_heal: bool,
}

enum Msg {
    Log(String),
    Checked(Box<Checked>),
    CheckFailed(String),
    Planned(Plan),
    Progress { rel: String, index: usize, of: usize, note: String },
    Placed(String),
    Finished(Result<Summary, String>),
}

pub struct QuestUpdate {
    settings: Settings,
    adb: Option<adb::Located>,
    watcher: Option<DeviceWatcher>,
    /// Chosen by serial, not by position: the list is re-read constantly and an index
    /// silently means a different headset the moment one is unplugged.
    chosen: Option<String>,

    check_phase: Phase,
    checked: Option<Checked>,
    check_error: Option<String>,

    phase: Phase,
    cancel: Cancel,
    rx: Option<Receiver<Msg>>,
    log: crate::log::Ring,
    log_open: bool,
    plan: Option<Plan>,
    current: Option<(String, usize, usize, String)>,
    finished: Vec<String>,
    summary: Option<Summary>,
    error: Option<String>,
}

impl Default for QuestUpdate {
    fn default() -> Self {
        let settings = Settings::load();
        let mut f = QuestUpdate {
            adb: None,
            watcher: None,
            chosen: None,
            check_phase: Phase::Idle,
            checked: None,
            check_error: None,
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
            settings,
        };
        f.rescan();
        f
    }
}

impl QuestUpdate {
    fn rescan(&mut self) {
        self.adb = adb::locate(self.settings.adb_path.as_deref());
        // Dropping the previous watcher stops its thread, so a changed adb never leaves an
        // old one polling.
        self.watcher = self.adb.as_ref().map(|f| DeviceWatcher::start(f.path.clone()));
    }

    fn snapshot(&self) -> crate::engine::watch::Snapshot {
        self.watcher.as_ref().map(|w| w.snapshot()).unwrap_or_default()
    }

    fn ready_device(&self) -> Option<Device> {
        self.snapshot().pick(self.chosen.as_deref())
    }

    fn adb_path(&self) -> Option<std::path::PathBuf> {
        self.adb.as_ref().map(|a| a.path.clone())
    }

    fn start_check(&mut self) {
        let (Some(path), Some(device)) = (self.adb_path(), self.ready_device()) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.check_phase = Phase::Running;
        self.checked = None;
        self.check_error = None;
        thread::spawn(move || check(path, device, tx));
    }

    fn start_update(&mut self) {
        let (Some(path), Some(device)) = (self.adb_path(), self.ready_device()) else {
            return;
        };
        let self_heal = self.checked.as_ref().is_some_and(|c| c.self_heal);
        let marker_seed = self.checked.as_ref().map(|c| {
            (
                c.manifest_base.clone().unwrap_or_default(),
                c.installed_sha.clone().unwrap_or_default(),
            )
        });

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
        let cancel = self.cancel.clone();
        thread::spawn(move || run(path, device, cancel, self_heal, marker_seed, tx));
    }

    fn pump(&mut self) {
        let (inbox, disconnected) = crate::channel::drain(&self.rx);
        for msg in inbox {
            match msg {
                Msg::Log(l) => self.log.push(l),
                Msg::Checked(c) => {
                    self.checked = Some(*c);
                    self.check_phase = Phase::Succeeded;
                }
                Msg::CheckFailed(e) => {
                    self.check_error = Some(e);
                    self.check_phase = Phase::Failed;
                }
                Msg::Planned(p) => self.plan = Some(p),
                Msg::Progress { rel, index, of, note } => {
                    self.current = Some((rel, index, of, note))
                }
                Msg::Placed(rel) => {
                    self.finished.push(rel);
                    self.current = None;
                }
                Msg::Finished(Ok(s)) => {
                    self.summary = Some(s);
                    self.phase = Phase::Succeeded;
                }
                Msg::Finished(Err(e)) => {
                    self.error = Some(e);
                    self.phase = Phase::Failed;
                }
            }
        }
        if disconnected {
            self.rx = None;
            if self.phase == Phase::Running {
                self.error = Some("The update stopped unexpectedly.".into());
                self.phase = Phase::Failed;
            }
            if self.check_phase == Phase::Running {
                self.check_error = Some("The version check stopped unexpectedly.".into());
                self.check_phase = Phase::Failed;
            }
        }
    }
}

/// Reads the headset's state and compares it against the manifest.
fn check(adb_path: std::path::PathBuf, device: Device, tx: mpsc::Sender<Msg>) {
    let adb = Adb::at(&adb_path);
    let q = Quest::new(&adb, Some(&device));

    // Plain, not cancellable: this is the short check, and there is no Cancel offered for
    // it to honour.
    let text = match download::fetch_text(endpoints::QUEST_MANIFEST) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Msg::CheckFailed(format!("Could not download the update list: {e}")));
            return;
        }
    };
    let manifest = match Manifest::parse(&text, endpoints::QUEST_MANIFEST) {
        Ok(m) => m,
        Err(e) => {
            let _ = tx.send(Msg::CheckFailed(format!("The update list is not valid: {e}")));
            return;
        }
    };

    let installed = q.is_installed();
    let installed_sha = if installed { q.installed_sha() } else { None };
    let marker = if installed { q.read_marker() } else { None };
    let base = manifest.base_apk().map(|b| b.sha256.clone());

    let decision = quest::decide(base.as_deref(), marker.as_ref(), installed, installed_sha.as_deref());
    let _ = tx.send(Msg::Checked(Box::new(Checked {
        verdict: decision.verdict,
        marker,
        installed_sha,
        manifest_base: base,
        self_heal: decision.self_heal,
    })));
}

fn run(
    adb_path: std::path::PathBuf,
    device: Device,
    cancel: Cancel,
    self_heal: bool,
    marker_seed: Option<(String, String)>,
    tx: mpsc::Sender<Msg>,
) {
    let adb = Adb::at(&adb_path);
    let q = Quest::new(&adb, Some(&device));
    let say = |l: String| {
        let _ = tx.send(Msg::Log(l));
    };

    let text = match download::fetch_text_cancellable(endpoints::QUEST_MANIFEST, &cancel, &mut |_, _| {}) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Msg::Finished(Err(format!("Could not download the update list: {e}"))));
            return;
        }
    };
    let manifest = match Manifest::parse(&text, endpoints::QUEST_MANIFEST) {
        Ok(m) => m,
        Err(e) => {
            let _ = tx.send(Msg::Finished(Err(format!("The update list is not valid: {e}"))));
            return;
        }
    };

    // A stock install with no marker gets one before anything is touched, so a failure
    // partway leaves the headset describable rather than anonymous.
    if self_heal {
        if let (Some(base_apk), Some((base_sha, installed_sha))) =
            (manifest.base_apk().map(|b| b.name.clone()), marker_seed)
        {
            say("no marker found, writing one from the stock build".into());
            let _ = q.write_marker(&Marker {
                base_apk,
                base_sha256: base_sha,
                installed_sha256: installed_sha,
                patched: false,
                installed_at: String::new(),
                installer_version: crate::app::VERSION.to_string(),
            });
        }
    }

    let tx2 = tx.clone();
    let plan = match engine::plan(&manifest, &q, &cancel, &mut |e| {
        if matches!(e, engine::Event::Hashing) {
            let _ = tx2.send(Msg::Log("asking the headset what it already has".into()));
        }
    }) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Msg::Finished(Err(e.to_string())));
            return;
        }
    };
    if plan.hashing_unavailable {
        say("this headset cannot hash its own files, so everything will be sent".into());
    }
    say(format!(
        "plan: {} to send, {} to delete, {} already current",
        plan.pushes.len(),
        plan.deletes.len(),
        plan.up_to_date.len()
    ));
    let _ = tx.send(Msg::Planned(plan.clone()));

    let root = manifest.target_root().unwrap_or(quest::MEDIA_ROOT).to_string();
    let staging = config::dir().join("staging");
    let tx3 = tx.clone();
    let result = engine::apply(&plan, &q, &root, &staging, &cancel, &mut |event| match event {
        engine::Event::Deleting { rel, index, of } => {
            let _ = tx3.send(Msg::Progress { rel, index, of, note: "removing".into() });
        }
        engine::Event::Downloading { rel, index, of, done, total } => {
            let note = match total {
                Some(t) => format!("{} / {}", human_bytes(done), human_bytes(t)),
                None => human_bytes(done),
            };
            let _ = tx3.send(Msg::Progress { rel, index, of, note });
        }
        engine::Event::Pushing { rel, index, of } => {
            let _ = tx3.send(Msg::Progress { rel, index, of, note: "sending to headset".into() });
        }
        engine::Event::Placed { rel } => {
            let _ = tx3.send(Msg::Placed(rel));
        }
        // Asking the headset to hash its own files is the slow, silent part of a Quest
        // update, and it was the one event thrown away.
        engine::Event::Hashing => {
            let _ = tx2.send(Msg::Log("asking the headset what it already has".into()));
        }
    });

    match result {
        Ok(s) => {
            say(format!("done: {} sent, {} removed, {} skipped", s.pushed, s.deleted, s.skipped));
            let _ = tx.send(Msg::Finished(Ok(s)));
        }
        Err(e) => {
            say(format!("failed: {e}"));
            let _ = tx.send(Msg::Finished(Err(e.to_string())));
        }
    }
}

impl crate::flows::Flow for QuestUpdate {
    /// Going back voids the check and the run. The chosen headset is kept: it is a choice,
    /// and re-picking it every time would be the annoying kind of correct.
    fn reset_after(&mut self, _step: usize) {
        self.cancel.cancel();
        self.rx = None;
        self.check_phase = Phase::Idle;
        self.checked = None;
        self.check_error = None;
        self.phase = Phase::Idle;
        self.plan = None;
        self.current = None;
        self.finished.clear();
        self.summary = None;
        self.error = None;
        self.log.clear();
    }

    fn steps(&self) -> &'static [&'static str] {
        STEPS
    }

    fn blocked_reason(&self, step: usize) -> Option<String> {
        match step {
            0 if self.adb.is_none() => Some("adb is not set up".into()),
            0 if self.ready_device().is_none() => {
                Some("Connect a headset and allow this computer".into())
            }
            1 => match (&self.checked, self.check_phase) {
                (_, Phase::Running) => Some("Checking".into()),
                (Some(c), _) if c.verdict == Verdict::Ok => None,
                (Some(_), _) => Some("This headset cannot take this update".into()),
                _ => Some("Run the check first".into()),
            },
            2 => match self.phase {
                Phase::Running => Some("Update in progress".into()),
                Phase::Succeeded => None,
                _ => Some("Run the update first".into()),
            },
            _ => None,
        }
    }

    fn on_enter(&mut self, step: usize) {
        if step == 1 && self.check_phase == Phase::Idle {
            self.start_check();
        }
    }

    fn on_exit(&mut self) {
        self.cancel.cancel();
    }

    fn status_note(&self) -> Option<(bool, String)> {
        Some(match (&self.adb, self.ready_device()) {
            (None, _) => (false, "adb is not set up".into()),
            (Some(_), None) => (false, "adb ready, waiting for a headset".into()),
            (Some(_), Some(d)) => (
                true,
                format!("adb ready, {}", d.model.clone().unwrap_or_else(|| d.serial.clone())),
            ),
        })
    }

    fn content(&mut self, ui: &mut Ui, step: usize, signals: &mut crate::flows::Signals) {
        self.pump();
        if self.phase == Phase::Running || self.check_phase == Phase::Running {
            signals.keep_repainting = true;
        }
        match step {
            0 => self.step_connect(ui, signals),
            1 => self.step_check(ui),
            2 => self.step_update(ui),
            _ => self.step_done(ui),
        }
    }
}

impl QuestUpdate {
    fn step_connect(&mut self, ui: &mut Ui, signals: &mut crate::flows::Signals) {
        // The watcher polls on its own thread; this only has to keep frames coming so the
        // result is seen.
        signals.keep_repainting = true;

        if self.adb.is_none() {
            widgets::status(ui, Status::Err, "adb is not set up");
            ui.label(
                RichText::new("Open Dependencies from the home screen to point at one or fetch it.")
                    .font(theme::font_ui(11.5))
                    .color(theme::TEXT_MUTED),
            );
            ui.add_space(theme::UNIT);
            if widgets::secondary(ui, "Re-check", true) {
                self.rescan();
            }
            return;
        }

        let snap = self.snapshot();
        if let Some(serial) = crate::flows::device_picker(ui, &snap, &self.chosen) {
            self.chosen = Some(serial);
        }
    }

    fn step_check(&mut self, ui: &mut Ui) {
        match self.check_phase {
            Phase::Running | Phase::Idle => {
                widgets::status(ui, Status::Info, "Reading the headset...");
            }
            Phase::Failed => {
                widgets::status(
                    ui,
                    Status::Err,
                    self.check_error.as_deref().unwrap_or("The check failed"),
                );
                ui.add_space(theme::UNIT);
                if widgets::secondary(ui, "Try again", true) {
                    self.check_phase = Phase::Idle;
                    self.start_check();
                }
            }
            Phase::Succeeded => {
                let Some(c) = &self.checked else { return };
                match &c.verdict {
                    Verdict::Ok => {
                        widgets::status(ui, Status::Ok, "This headset can take the update");
                        if c.self_heal {
                            widgets::status(
                                ui,
                                Status::Info,
                                "No version record found. One will be written from the build \
                                 that is installed.",
                            );
                        }
                    }
                    Verdict::NotInstalled => {
                        widgets::status(ui, Status::Warn, "Echo VR is not installed on this headset");
                        ui.label(
                            RichText::new("Use Install Echo VR (Quest) instead.")
                                .font(theme::font_ui(11.5))
                                .color(theme::TEXT_MUTED),
                        );
                    }
                    Verdict::Mismatch(why) => {
                        widgets::status(ui, Status::Err, why);
                        ui.label(
                            RichText::new(
                                "An update can only be applied on top of the exact build it was \
                                 made for. Reinstalling is the way forward.",
                            )
                            .font(theme::font_ui(11.5))
                            .color(theme::TEXT_MUTED),
                        );
                    }
                }

                // The evidence, so a refusal is not a black box.
                ui.add_space(theme::UNIT * 1.25);
                widgets::card(ui, |ui| {
                    match &c.marker {
                        Some(m) => {
                            widgets::kv(ui, "Installed from ", &m.base_apk);
                            widgets::kv(ui, "Patched        ", if m.patched { "yes" } else { "no" });
                            if !m.installed_at.is_empty() {
                                widgets::kv(ui, "Installed at   ", &m.installed_at);
                            }
                            if !m.installer_version.is_empty() {
                                widgets::kv(ui, "By             ", &m.installer_version);
                            }
                        }
                        None => {
                            widgets::kv(ui, "Version record ", "none on this headset");
                        }
                    }
                    widgets::kv(
                        ui,
                        "Update expects ",
                        &crate::fmt::short_hash(c.manifest_base.as_deref().unwrap_or("(not stated)")),
                    );
                    widgets::kv(
                        ui,
                        "Headset holds  ",
                        &crate::fmt::short_hash(c.installed_sha.as_deref().unwrap_or("(could not read)")),
                    );
                });
            }
        }
    }

    fn step_update(&mut self, ui: &mut Ui) {
        if self.phase == Phase::Idle {
            ui.label(
                RichText::new(
                    "Asks the headset which files it already has, then sends only what \
                     changed.",
                )
                .font(theme::font_ui(12.0))
                .color(theme::TEXT_MUTED),
            );
            ui.add_space(theme::UNIT * 1.5);
            if widgets::primary(ui, "Start update", true) {
                self.start_update();
            }
            return;
        }

        let plan = self.plan.clone();
        let finished = self.finished.clone();
        let current = self.current.clone();
        let failed = self.phase == Phase::Failed;

        widgets::card(ui, |ui| match &plan {
            None => widgets::check_row(ui, RowState::Working, "Working out what has changed"),
            Some(p) => {
                if p.is_empty() {
                    widgets::check_row(ui, RowState::Done, "Everything is already up to date");
                }
                for step in p.deletes.iter().chain(p.pushes.iter()) {
                    let state = if finished.contains(&step.rel) {
                        RowState::Done
                    } else if current.as_ref().is_some_and(|(rel, ..)| rel == &step.rel) {
                        if failed {
                            RowState::Failed
                        } else {
                            RowState::Working
                        }
                    } else {
                        RowState::Pending
                    };
                    widgets::check_row(ui, state, &step.rel);
                }
                if !p.up_to_date.is_empty() {
                    ui.add_space(theme::UNIT * 0.5);
                    ui.label(
                        RichText::new(format!("{} already current", p.up_to_date.len()))
                            .font(theme::font_ui(11.0))
                            .color(theme::TEXT_DIM),
                    );
                }
            }
        });

        if let Some((rel, index, of, note)) = &current {
            ui.add_space(theme::UNIT * 0.5);
            // `rel` is a manifest path and can be long enough to leave the window.
            widgets::breaking_label(
                ui,
                &format!("{index}/{of}  {rel}  {note}"),
                theme::font_mono(10.5),
                theme::TEXT_DIM,
            );
        }

        ui.add_space(theme::UNIT * 0.75);
        match self.phase {
            Phase::Succeeded => {
                let s = self.summary.unwrap_or_default();
                widgets::status(
                    ui,
                    Status::Ok,
                    &format!(
                        "Update applied: {} sent, {} removed, {} already current",
                        s.pushed, s.deleted, s.skipped
                    ),
                );
            }
            Phase::Failed => {
                widgets::status(
                    ui,
                    Status::Err,
                    self.error.as_deref().unwrap_or("The update failed"),
                );
                ui.add_space(theme::UNIT * 0.5);
                ui.horizontal(|ui| {
                    if widgets::secondary(ui, "Retry", true) {
                        self.start_update();
                    }
                    widgets::external_link(ui, "Ask for help on Discord", endpoints::DISCORD_LOUNGE);
                });
            }
            Phase::Running => {
                if widgets::secondary(ui, "Cancel", true) {
                    self.cancel.cancel();
                }
            }
            Phase::Idle => {}
        }

        ui.add_space(theme::UNIT);
        let mut open = self.log_open;
        widgets::log_pane(ui, &mut open, self.log.lines());
        self.log_open = open;
    }

    fn step_done(&mut self, ui: &mut Ui) {
        widgets::status(ui, Status::Ok, "Echo VR on your headset is up to date");
        ui.add_space(theme::UNIT);
        let s = self.summary.unwrap_or_default();
        widgets::card(ui, |ui| {
            widgets::kv(ui, "Sent        ", &s.pushed.to_string());
            widgets::kv(ui, "Removed     ", &s.deleted.to_string());
            widgets::kv(ui, "Unchanged   ", &s.skipped.to_string());
        });
    }
}


#[cfg(test)]
mod tests {

    #[test]
    fn shortens_only_real_hashes() {
        let h = "0a7fa5f9cfc173013e152a75fac2ded7ca4f66b8d8530f598c0c2530b5cf0973";
        assert_eq!(crate::fmt::short_hash(h), "0a7fa5f9...b5cf0973");
        assert_eq!(crate::fmt::short_hash("(not stated)"), "(not stated)");
    }
}
