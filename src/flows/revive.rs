// SPDX-License-Identifier: GPL-3.0-or-later
//! Set up Revive, for playing Echo through SteamVR.
//!
//! Five steps. The actions are offered as a list to tick rather than done wholesale,
//! because they are independent, they fail for different reasons, and two of them write
//! into Program Files while one does not. Someone who only wants the shortcut should not
//! have to trigger an elevation prompt for it.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use egui::{RichText, Ui};

use crate::config;
use crate::endpoints;
use crate::engine::install::{self, Inspection};
use crate::engine::revive::{self, Outcome};
use crate::engine::Cancel;
use crate::fmt::human_bytes;
use crate::theme;
use crate::widgets::{self, RowState, Status};

/// Prefix on a result that failed only for want of rights. Stripped before display.
const ELEVATION_MARK: &str = "\u{1}";

const STEPS: &[&str] = &["Echo path", "Revive", "Actions", "Apply", "Done"];

/// The things this flow can do, in the order they run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Shortcut,
    Manifest,
}

impl Action {
    const ALL: [Action; 2] = [Action::Shortcut, Action::Manifest];

    fn label(self) -> &'static str {
        match self {
            Action::Shortcut => "Desktop shortcut",
            Action::Manifest => "Add Echo to Revive's app list",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Action::Shortcut => "Launches Echo through Revive's injector. No admin needed.",
            Action::Manifest => {
                "Lets Revive start Echo from SteamVR. Writes into Program Files, so this \
                 needs administrator rights."
            }
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Running,
    Succeeded,
    Failed,
}

enum Msg {
    Log(String),
    Progress { done: u64, total: Option<u64> },
    Installed,
    InstallFailed(String),
    Step { action: Action, result: Result<String, String> },
    Finished,
}

/// The folder chosen in Dependencies wins over anything found automatically, the same way
/// it does for adb. Read fresh each time rather than cached: the user may have chosen one
/// while this flow was open.
fn found_revive() -> Option<PathBuf> {
    crate::engine::revive::locate(crate::config::Settings::load().revive_path.as_deref())
        .map(|f| f.dir)
}

pub struct Revive {
    path: String,
    /// Where the prefilled or adopted folder came from. Shown under the field, because a
    /// suggestion whose reasoning is invisible is the app deciding.
    path_note: Option<&'static str>,
    inspection: Inspection,

    revive_dir: Option<PathBuf>,
    install_phase: Phase,
    install_error: Option<String>,
    progress: Option<(u64, Option<u64>)>,

    chosen: Vec<Action>,
    results: Vec<(Action, Result<String, String>)>,
    /// A step failed only for want of administrator rights, so the broker can offer to
    /// redo it rather than telling the user to relaunch the app themselves.
    needs_elevation: bool,
    elevated: crate::flows::elevated::Elevated,
    apply_phase: Phase,
    running: Option<Action>,

    cancel: Cancel,
    rx: Option<Receiver<Msg>>,
    log: crate::log::Ring,
    log_open: bool,

    /// Which Meta library the chosen Echo sits in, according to the registry, together with
    /// the path that answer was worked out for. `Some((_, None))` means the registry does
    /// not place it in any library, which is the case that produces a SteamVR entry that
    /// cannot launch. Memoised because reading it starts a PowerShell.
    library: Option<(String, Option<String>)>,
    pending: Option<crate::flows::Confirm>,
}

/// Only used the first time, before there is anything to remember or detect.
fn guessed_path() -> String {
    if cfg!(windows) {
        "C:\\EchoVR".to_string()
    } else {
        format!("{}/EchoVR", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
    }
}

impl Default for Revive {
    fn default() -> Self {
        // Acts on an existing install, so it wants the same suggestion the update flow
        // wants: where Echo actually is, not where it would go.
        let (path, path_note) = crate::config::suggested_update_path(guessed_path);
        let inspection = install::inspect(std::path::Path::new(&path));
        Revive {
            path,
            path_note,
            inspection,
            revive_dir: found_revive(),
            install_phase: Phase::Idle,
            install_error: None,
            progress: None,
            // Both on by default: someone who opened this flow wants both. The artwork
            // action the original ticks by default is absent; see the Actions step.
            chosen: Action::ALL.to_vec(),
            results: Vec::new(),
            needs_elevation: false,
            elevated: Default::default(),
            apply_phase: Phase::Idle,
            running: None,
            cancel: Cancel::new(),
            rx: None,
            library: None,
            pending: None,
            log: crate::log::Ring::default(),
            log_open: false,
        }
    }
}

impl Revive {
    fn reinspect(&mut self) {
        self.inspection = install::inspect(std::path::Path::new(&self.path));
    }

    fn start_revive_install(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.install_phase = Phase::Running;
        self.install_error = None;
        self.progress = None;
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        thread::spawn(move || {
            let tx2 = tx.clone();
            let url = revive::installer_url();
            let _ = tx2.send(Msg::Log(format!("installer {url}")));
            let dest = config::dir().join("staging").join("ReviveInstaller.exe");
            let spec = crate::engine::download::Spec::new(url, dest.clone());
            if let Err(e) = crate::engine::download::fetch(&spec, &cancel, &mut |s| {
                let _ = tx2.send(Msg::Progress { done: s.done, total: s.total });
            }) {
                let _ = tx.send(Msg::InstallFailed(e.to_string()));
                return;
            }
            let _ = tx.send(Msg::Log("running the installer, expect a Windows prompt".into()));
            match revive::run_installer(&dest) {
                Ok(()) => {
                    let _ = tx.send(Msg::Installed);
                }
                Err(e) => {
                    let _ = tx.send(Msg::InstallFailed(e.to_string()));
                }
            }
        });
    }

    /// Hands the same setup to an elevated copy, as the PC flows do.
    ///
    /// Deliberately the command anyone could type, so an elevated run is not a special path
    /// through the code.
    /// What is actually there now, for the actions that were asked for.
    fn verify_on_disk(&self) -> Vec<(Action, Result<String, String>)> {
        self.chosen
            .iter()
            .map(|action| {
                let outcome = match action {
                    Action::Shortcut => match revive::shortcut_path() {
                        Some(p) if p.is_file() => Ok(format!("created at {}", p.display())),
                        Some(p) => Err(format!("not found at {}", p.display())),
                        None => Err("no Desktop folder to put it in".to_string()),
                    },
                    Action::Manifest => match self.revive_dir.as_ref() {
                        Some(dir) if revive::has_entry(dir) => {
                            Ok("entry present in Revive's app list".to_string())
                        }
                        Some(_) => Err("the entry is not in Revive's app list".to_string()),
                        None => Err("Revive could not be found".to_string()),
                    },
                };
                (*action, outcome)
            })
            .collect()
    }

    fn start_elevated(&mut self) {
        self.results.clear();
        self.needs_elevation = false;
        self.apply_phase = Phase::Running;
        self.log.clear();
        self.log.push("asking Windows for administrator rights".into());
        self.elevated.start(vec![
            "revive".into(),
            "setup".into(),
            "--path".into(),
            self.path.clone(),
        ]);
    }

    fn start_apply(&mut self) {
        let Some(dir) = self.revive_dir.clone() else { return };
        let exe = install::exe_path(std::path::Path::new(&self.path));
        let actions = self.chosen.clone();

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.apply_phase = Phase::Running;
        self.results.clear();
        self.cancel = Cancel::new();

        thread::spawn(move || {
            for action in actions {
                let result = match action {
                    Action::Shortcut => revive::create_shortcut(&dir, &exe)
                        .map(|p| format!("created at {}", p.display()))
                        .map_err(|e| e.to_string()),
                    Action::Manifest => revive::patch_manifest(&dir, &exe)
                        .map(|o| match o {
                            Outcome::Added => "entry added".to_string(),
                            Outcome::Updated => "entry refreshed".to_string(),
                        })
                        .map_err(|e| {
                            // Marked rather than worded: the flow turns this into an offer
                            // to do it elevated, and matching on a sentence to decide that
                            // would break the first time the sentence changed.
                            if e.needs_elevation() {
                                format!("{ELEVATION_MARK}{e}")
                            } else {
                                e.to_string()
                            }
                        }),
                };
                let _ = tx.send(Msg::Step { action, result });
            }
            let _ = tx.send(Msg::Finished);
        });
    }

    fn pump(&mut self) {
        for update in self.elevated.poll() {
            match update {
                crate::flows::elevated::Update::Line(l) => self.log.push(l),
                // Revive setup has no bytes to report, so its events are just lines.
                crate::flows::elevated::Update::Event(e) => {
                    if let crate::cli::Event::Stage(s) = e {
                        self.log.push(s);
                    }
                }
                crate::flows::elevated::Update::Finished => {
                    self.needs_elevation = false;
                    self.log.push("the elevated run finished".into());
                    // Its results cannot cross a process boundary as data, and a summary
                    // built from "it exited zero" would be a claim rather than a finding.
                    // So what it achieved is read back off disk.
                    self.revive_dir = found_revive();
                    self.results = self.verify_on_disk();
                    self.apply_phase = if self.results.iter().all(|(_, r)| r.is_ok()) {
                        Phase::Succeeded
                    } else {
                        Phase::Failed
                    };
                }
                crate::flows::elevated::Update::Failed(e) => {
                    self.apply_phase = Phase::Failed;
                    self.log.push(e);
                }
            }
        }

        let (inbox, _) = crate::channel::drain(&self.rx);
        for msg in inbox {
            match msg {
                Msg::Log(l) => self.log.push(l),
                Msg::Progress { done, total } => self.progress = Some((done, total)),
                Msg::Installed => {
                    self.install_phase = Phase::Succeeded;
                    self.progress = None;
                    // The installer runs detached and elevated, so the files appear a
                    // moment after it returns. Re-checked rather than assumed.
                    self.revive_dir = found_revive();
                }
                Msg::InstallFailed(e) => {
                    // Asked for, not gone wrong.
                    self.install_error = (!self.cancel.is_cancelled()).then_some(e);
                    self.install_phase = Phase::Failed;
                    self.progress = None;
                }
                Msg::Step { action, result } => {
                    self.running = None;
                    if let Err(e) = &result {
                        if e.starts_with(ELEVATION_MARK) {
                            self.needs_elevation = true;
                        }
                    }
                    self.results.push((action, result));
                }
                Msg::Finished => {
                    self.apply_phase = if self.results.iter().all(|(_, r)| r.is_ok()) {
                        Phase::Succeeded
                    } else {
                        Phase::Failed
                    };
                }
            }
        }
    }
}

impl crate::flows::Flow for Revive {
    /// Going back voids the install attempt and every applied action. What the user ticked
    /// is kept; what those ticks produced is not.
    fn reset_after(&mut self, _step: usize) {
        self.cancel.cancel();
        self.rx = None;
        self.install_phase = Phase::Idle;
        self.install_error = None;
        self.progress = None;
        self.results.clear();
        self.apply_phase = Phase::Idle;
        self.needs_elevation = false;
        self.elevated.forget();
        self.running = None;
        self.log.clear();
        self.revive_dir = found_revive();
    }

    fn steps(&self) -> &'static [&'static str] {
        STEPS
    }

    fn status_note(&self) -> Option<(bool, String)> {
        Some(match &self.revive_dir {
            Some(_) => (true, "Revive found".into()),
            None => (false, "Revive not found".into()),
        })
    }

    fn blocked_reason(&self, step: usize) -> Option<String> {
        match step {
            0 if !self.inspection.has_echo => {
                Some("Point at a folder containing echovr.exe".into())
            }
            1 if self.revive_dir.is_none() => Some("Revive has to be installed first".into()),
            2 if self.chosen.is_empty() => Some("Tick at least one thing to do".into()),
            3 => match self.apply_phase {
                Phase::Running => Some("Working".into()),
                Phase::Succeeded => None,
                _ => Some("Run the setup first".into()),
            },
            _ => None,
        }
    }

    fn on_exit(&mut self) {
        self.cancel.cancel();
    }

    fn content(&mut self, ui: &mut Ui, step: usize, signals: &mut crate::flows::Signals) {
        self.pump();
        if self.install_phase == Phase::Running || self.apply_phase == Phase::Running {
            signals.keep_repainting = true;
        }
        match step {
            0 => self.step_path(ui),
            1 => self.step_revive(ui),
            2 => self.step_actions(ui),
            3 => self.step_apply(ui),
            _ => self.step_done(ui),
        }
    }
}

impl Revive {
    /// The Meta library this Echo lives in, read from the registry.
    ///
    /// This is the whole question behind the warning on the Actions step. Revive's app list
    /// entry launches Echo through the Oculus runtime, so its path is stored relative to a
    /// library id and resolved against that library. An Echo that is not inside one has no
    /// id that resolves, and the entry then points at a path that does not exist.
    ///
    /// `patch_manifest` will fall back to borrowing an id from another app's entry, which is
    /// what the original installer does. That is what makes the entry appear to succeed and
    /// then fail to launch, so a `None` here is worth stopping for even though the write
    /// itself would go through.
    ///
    /// Memoised against the path it was computed for, because it shells out.
    fn meta_library(&mut self) -> Option<String> {
        if self.library.as_ref().map(|(p, _)| p.as_str()) != Some(self.path.as_str()) {
            let exe = install::exe_path(std::path::Path::new(&self.path));
            let found = crate::engine::meta::library_id_for(&exe);
            self.library = Some((self.path.clone(), found));
        }
        self.library.as_ref().and_then(|(_, id)| id.clone())
    }

    /// Would the app list entry be written without a library that resolves it?
    fn manifest_would_dangle(&mut self) -> bool {
        self.chosen.contains(&Action::Manifest) && self.meta_library().is_none()
    }

    fn step_path(&mut self, ui: &mut Ui) {
        if !cfg!(windows) {
            widgets::status(ui, Status::Warn, "Revive is a Windows-only SteamVR shim");
            ui.label(
                RichText::new("The steps will render, but nothing here can run on this system.")
                    .font(theme::font_ui(11.5))
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(theme::UNIT);
        }
        widgets::field_label(ui, "Echo VR folder");
        if let Some(note) = self.path_note {
            ui.label(
                egui::RichText::new(note).font(theme::font_ui(10.5)).color(theme::TEXT_FAINT),
            );
            ui.add_space(2.0);
        }
        let mut edited = false;
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 96.0).clamp(180.0, 380.0);
            if widgets::path_field(ui, &mut self.path, w).changed() {
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
            self.reinspect();
        }
        ui.add_space(theme::UNIT * 1.5);
        if self.inspection.has_echo {
            widgets::status(ui, Status::Ok, "echovr.exe found at this path");
        } else {
            widgets::status(ui, Status::Err, "no echovr.exe here");
            ui.label(
                RichText::new("Revive needs the exact path to the game to launch it.")
                    .font(theme::font_ui(11.0))
                    .color(theme::TEXT_MUTED),
            );
        }
    }

    fn step_revive(&mut self, ui: &mut Ui) {
        match &self.revive_dir {
            Some(dir) => {
                widgets::status(ui, Status::Ok, "Revive is installed");
                widgets::mono_color(ui, &dir.display().to_string(), 10.5, theme::TEXT_DIM);
            }
            None => {
                widgets::status(ui, Status::Warn, "Revive was not found");
                ui.label(
                    RichText::new(
                        "It can be fetched from its own releases. Windows will ask you to \
                         confirm, because its installer needs administrator rights.",
                    )
                    .font(theme::font_ui(11.5))
                    .color(theme::TEXT_MUTED),
                );
            }
        }

        if let Some((done, total)) = self.progress {
            ui.add_space(theme::UNIT * 0.75);
            let frac = match total {
                Some(t) if t > 0 => done as f32 / t as f32,
                _ => 0.0,
            };
            widgets::progress_row(
                ui,
                "ReviveInstaller.exe",
                frac,
                &match total {
                    Some(t) => format!("{} / {}", human_bytes(done), human_bytes(t)),
                    None => human_bytes(done),
                },
            );
        }
        if let Some(e) = &self.install_error {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(ui, Status::Err, e);
        }

        ui.add_space(theme::UNIT);
        ui.horizontal(|ui| {
            let busy = self.install_phase == Phase::Running;
            // The only flow without one, and it downloads 58 MB. Checked between chunks,
            // and the partial file is kept, so stopping costs nothing.
            if busy && widgets::secondary(ui, "Cancel", true) {
                self.cancel.cancel();
            }
            let label = if self.revive_dir.is_some() { "Reinstall" } else { "Install Revive" };
            if widgets::secondary(ui, label, !busy && cfg!(windows)) {
                self.start_revive_install();
            }
            if widgets::secondary(ui, "Re-check", !busy) {
                self.revive_dir = found_revive();
            }
        });
        if self.install_phase == Phase::Succeeded && self.revive_dir.is_none() {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(
                ui,
                Status::Warn,
                "The installer ran but Revive still is not where it should be. It may still \
                 be finishing; press Re-check.",
            );
        }

        ui.add_space(theme::UNIT);
        let mut open = self.log_open;
        widgets::log_pane(ui, &mut open, self.log.lines());
        self.log_open = open;
    }

    fn step_actions(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("Pick what to set up. Each one is independent.")
                .font(theme::font_ui(12.0))
                .color(theme::TEXT_MUTED),
        );
        ui.add_space(theme::UNIT * 1.25);

        for action in Action::ALL {
            let on = self.chosen.contains(&action);
            if widgets::option_row(ui, on, action.label(), action.detail()) {
                if on {
                    self.chosen.retain(|a| *a != action);
                } else {
                    self.chosen.push(action);
                }
            }
            ui.add_space(theme::UNIT * 0.5);
        }

        ui.add_space(theme::UNIT);
        // Stated either way, not only when it is bad news. Someone who has been told once
        // that this entry can fail wants to know it is fine the next time, and silence does
        // not tell them that.
        match self.meta_library() {
            Some(_) => widgets::status(
                ui,
                Status::Ok,
                "This Echo is inside your Meta library, so the app list entry will work",
            ),
            None => {
                widgets::status(
                    ui,
                    Status::Warn,
                    "The app list entry will not work for this folder",
                );
                ui.label(
                    RichText::new(
                        "Revive resolves that entry against a Meta library, and this Echo is \
                         not inside one. SteamVR would show Echo VR and fail to start it. The \
                         desktop shortcut is unaffected: it launches Echo wherever it is.",
                    )
                    .font(theme::font_ui(10.5))
                    .color(theme::TEXT_FAINT),
                );
            }
        }

        ui.add_space(theme::UNIT);
        // Present but unavailable, rather than quietly missing: the original offers this
        // and ticks it by default, so its absence is worth accounting for.
        widgets::status(ui, Status::Info, "Game artwork is not offered");
        ui.label(
            RichText::new(
                "The artwork pack is no longer published on any mirror. Ticking it would only \
                 ever fail.",
            )
            .font(theme::font_ui(10.5))
            .color(theme::TEXT_FAINT),
        );
    }

    fn step_apply(&mut self, ui: &mut Ui) {
        if self.apply_phase == Phase::Idle {
            ui.label(
                RichText::new(
                    "Runs the ticked items in order. Adding Echo to Revive's app list writes \
                     into Program Files.",
                )
                .font(theme::font_ui(12.0))
                .color(theme::TEXT_MUTED),
            );
            ui.add_space(theme::UNIT * 1.5);
            if widgets::primary(ui, "Set up Revive", true) {
                // Only when it would produce something that does not work. An Echo the
                // registry places in a library needs no dialog, and one on every run is one
                // nobody reads by the third time.
                match self.manifest_would_dangle() {
                    true => {
                        self.pending = Some(crate::flows::Confirm {
                            title: "The app list entry will not work".into(),
                            consequence: format!(
                                "Revive resolves its app list entry against a Meta library, \
                                 and the registry does not place this Echo in one:\n\n{}\n\n\
                                 The entry will be written and SteamVR will show Echo VR, \
                                 but starting it from there will fail, because the path the \
                                 entry resolves to does not exist.\n\n\
                                 The desktop shortcut is not affected. It launches Revive's \
                                 injector with the real path and works wherever Echo is \
                                 installed, so untick the app list and keep the shortcut if \
                                 you only want something that works.",
                                crate::fmt::windows_path(std::path::Path::new(&self.path)),
                            ),
                            proceed: "Add it anyway".into(),
                        })
                    }
                    false => self.start_apply(),
                }
            }
            if widgets::confirm_modal(ui, &mut self.pending) == Some(true) {
                self.start_apply();
            }
            return;
        }

        widgets::card(ui, |ui| {
            for action in &self.chosen {
                let done = self.results.iter().find(|(a, _)| a == action);
                let state = match done {
                    Some((_, Ok(_))) => RowState::Done,
                    Some((_, Err(_))) => RowState::Failed,
                    None if self.apply_phase == Phase::Running => RowState::Working,
                    None => RowState::Pending,
                };
                widgets::check_row(ui, state, action.label());
                if let Some((_, result)) = done {
                    let (kind, text) = match result {
                        Ok(msg) => (Status::Ok, msg.clone()),
                        Err(msg) => {
                            (Status::Err, msg.trim_start_matches(ELEVATION_MARK).to_string())
                        }
                    };
                    ui.indent(action.label(), |ui| widgets::status(ui, kind, &text));
                }
            }
        });

        ui.add_space(theme::UNIT * 0.75);
        match self.apply_phase {
            Phase::Succeeded => widgets::status(ui, Status::Ok, "Revive is set up"),
            Phase::Failed => {
                // Partial success is the common case here, so it is named as such rather
                // than reported as a flat failure.
                let ok = self.results.iter().filter(|(_, r)| r.is_ok()).count();
                widgets::status(
                    ui,
                    Status::Warn,
                    &format!("{ok} of {} finished. The rest are listed above.", self.results.len()),
                );
                if self.needs_elevation {
                    ui.add_space(theme::UNIT * 0.5);
                    widgets::status(
                        ui,
                        Status::Warn,
                        match crate::flows::elevated::Elevated::available() {
                            true => "Revive's folder needs administrator rights. Windows will ask.",
                            false => "Revive's folder cannot be written to from here.",
                        },
                    );
                }
                ui.add_space(theme::UNIT * 0.5);
                ui.horizontal(|ui| {
                    if widgets::secondary(ui, "Try again", true) {
                        self.start_apply();
                    }
                    // The broker existed and this flow was the one that did not use it,
                    // so it told people to relaunch the app themselves - which is the dead
                    // end the broker was built to remove.
                    if self.needs_elevation && crate::flows::elevated::Elevated::available() {
                        if widgets::primary(ui, "Run as administrator", true) {
                            self.start_elevated();
                        }
                    }
                    widgets::external_link(ui, "Ask for help on Discord", endpoints::DISCORD_LOUNGE);
                });
            }
            _ => {}
        }
    }

    fn step_done(&mut self, ui: &mut Ui) {
        widgets::status(ui, Status::Ok, "Revive setup finished");
        // An empty card is a box with nothing in it, which reads as something failing to
        // load rather than as nothing to say.
        if self.results.is_empty() {
            ui.add_space(theme::UNIT * 1.5);
            ui.label(
                RichText::new(
                    "Start SteamVR, then launch Echo from the desktop shortcut or from \
                     Revive's dashboard.",
                )
                .font(theme::font_ui(12.0))
                .color(theme::TEXT_MUTED),
            );
            return;
        }
        ui.add_space(theme::UNIT);
        widgets::card(ui, |ui| {
            for (action, result) in &self.results {
                widgets::kv(
                    ui,
                    action.label(),
                    match result {
                        Ok(msg) => msg,
                        Err(msg) => msg,
                    },
                );
            }
        });
        ui.add_space(theme::UNIT * 1.5);
        ui.label(
            RichText::new(
                "Start SteamVR, then launch Echo from the desktop shortcut or from Revive's \
                 dashboard.",
            )
            .font(theme::font_ui(12.0))
            .color(theme::TEXT_MUTED),
        );
    }
}
