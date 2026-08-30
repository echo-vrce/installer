// SPDX-License-Identifier: GPL-3.0-or-later
//! The dependency panel.
//!
//! A settings screen rather than a wizard, because it is not a sequence: it is a statement
//! of what the app can find and an offer to fix what it cannot. It lives outside the flows
//! for the same reason adb is needed by two of them and Revive by one, so making it a step
//! would mean asking the same question twice.
//!
//! Everything here follows the same rule as the rest of the app: it reports what it sees,
//! it never rewrites a path the user chose, and a manual choice always wins over anything
//! found automatically.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use egui::{RichText, Ui};

use crate::config::{self, Settings};
use crate::engine::adb::{self, InstallStage, Located, Source, State};
use crate::engine::watch::DeviceWatcher;
use crate::engine::Cancel;
use crate::fmt::human_bytes;
use crate::theme;
use crate::widgets::{self, Status};

// Device polling lives in a background watcher, so a slow or hung adb cannot freeze this
// window. See engine::watch for why that matters more than it sounds.

enum Msg {
    Progress { stage: InstallStage, done: u64, total: Option<u64> },
    Finished(Result<PathBuf, String>),
}

/// Revive's own install: a download, then an elevation prompt the user has to answer.
enum ReviveMsg {
    Progress { done: u64, total: Option<u64> },
    Finished(Result<(), String>),
}

pub struct Dependencies {
    settings: Settings,
    adb: Option<Located>,
    watcher: Option<DeviceWatcher>,
    pending: Option<crate::flows::Confirm>,
    installing: Option<Receiver<Msg>>,
    install_progress: Option<(InstallStage, u64, Option<u64>)>,
    install_error: Option<String>,
    cancel: Cancel,

    revive: Option<crate::engine::revive::Located>,
    revive_installing: Option<Receiver<ReviveMsg>>,
    revive_progress: Option<(u64, Option<u64>)>,
    revive_error: Option<String>,
}

impl Default for Dependencies {
    fn default() -> Self {
        let settings = Settings::load();
        let mut d = Dependencies {
            adb: None,
            watcher: None,
            pending: None,
            installing: None,
            install_progress: None,
            install_error: None,
            cancel: Cancel::new(),
            settings,
            revive: None,
            revive_installing: None,
            revive_progress: None,
            revive_error: None,
        };
        d.rescan();
        d
    }
}

impl Dependencies {
    /// What replacing adb actually costs, which is not "a headset is connected".
    fn replacement_warning(&self) -> crate::flows::Confirm {
        let attached = self
            .watcher
            .as_ref()
            .map(|w| w.snapshot())
            .map(|s| {
                s.devices()
                    .iter()
                    .map(|d| d.model.clone().unwrap_or_else(|| d.serial.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut consequence = String::from(
            "Replacing adb stops the adb server first, because a running copy cannot be \
             overwritten. That drops the connection to any headset, and anything in \
             progress on one is interrupted where it stands.\n\n",
        );
        if !attached.is_empty() {
            consequence.push_str(&format!(
                "Connected right now: {}. It will reconnect on its own afterwards.\n\n",
                attached.join(", ")
            ));
        }
        consequence.push_str(
            "The copy you have is kept until the new one is unpacked and in place, so if \
             this fails you keep the adb that was working.",
        );

        crate::flows::Confirm {
            title: "Replace the installed adb".into(),
            consequence,
            proceed: "Replace it".into(),
        }
    }

    /// Re-locates adb and restarts the watcher on whatever was found.
    /// Called when the screen is opened again, rather than built.
    ///
    /// The panel is kept alive between visits, so what it found the first time stays on
    /// show until something asks it to look again. Anything can have moved in between: an
    /// adb chosen from the command line, a Revive installed by a flow.
    ///
    /// Unlike [`rescan`](Self::rescan), the watcher survives an unchanged path. Restarting
    /// it on every visit would drop a working connection to a headset and send it hunting
    /// for the device again, which the user sees as the list blinking out for no reason.
    pub fn reenter(&mut self) {
        self.settings = Settings::load();
        let found = adb::locate(self.settings.adb_path.as_deref());
        if found.as_ref().map(|f| &f.path) != self.adb.as_ref().map(|f| &f.path) {
            self.watcher = found.as_ref().map(|f| DeviceWatcher::start(f.path.clone()));
        }
        self.adb = found;
        self.revive = crate::engine::revive::locate(self.settings.revive_path.as_deref());
    }

    pub fn rescan(&mut self) {
        self.adb = adb::locate(self.settings.adb_path.as_deref());
        // Dropping the old watcher stops its thread, so a changed adb path never leaves a
        // second one polling the previous binary.
        self.watcher = self.adb.as_ref().map(|f| DeviceWatcher::start(f.path.clone()));
        self.revive = crate::engine::revive::locate(self.settings.revive_path.as_deref());
    }

    fn start_install(&mut self) {
        // The watcher runs adb every couple of seconds. On Windows each of those holds the
        // executable open, which is the other half of why a reinstall could not replace it.
        self.watcher = None;

        let (tx, rx) = mpsc::channel();
        self.installing = Some(rx);
        self.install_error = None;
        self.install_progress = None;
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();

        thread::spawn(move || {
            let tx2 = tx.clone();
            let result = adb::install(&cancel, &mut |stage, done, total| {
                let _ = tx2.send(Msg::Progress { stage, done, total });
            });
            let _ = tx.send(Msg::Finished(result));
        });
    }

    fn pump(&mut self) -> bool {
        let (revive_msgs, _) = crate::channel::drain(&self.revive_installing);
        for msg in revive_msgs {
            match msg {
                ReviveMsg::Progress { done, total } => self.revive_progress = Some((done, total)),
                ReviveMsg::Finished(r) => {
                    self.revive_installing = None;
                    self.revive_progress = None;
                    if let Err(e) = r {
                        // Asked for, not gone wrong. Told apart by what we requested rather
                        // than by matching the error text, which would break the first time
                        // the wording changed.
                        self.revive_error =
                            (!self.cancel.is_cancelled()).then_some(e);
                    }
                    // The installer runs detached and elevated, so its files appear a
                    // moment after it returns. Re-read rather than assumed.
                    self.revive = crate::engine::revive::locate(
                        self.settings.revive_path.as_deref(),
                    );
                }
            }
        }

        let (inbox, done) = crate::channel::drain(&self.installing);
        let mut finished = false;
        for msg in inbox {
            match msg {
                Msg::Progress { stage, done, total } => {
                    self.install_progress = Some((stage, done, total))
                }
                Msg::Finished(Ok(_)) => {
                    finished = true;
                }
                Msg::Finished(Err(e)) => {
                    // Same rule as Revive's: a cancel is an answer, not a fault.
                    self.install_error = (!self.cancel.is_cancelled()).then_some(e);
                    finished = true;
                }
            }
        }
        if finished || done {
            self.installing = None;
            self.install_progress = None;
            // A freshly unpacked adb should show up immediately, not after a manual poke.
            self.rescan();
        }
        self.installing.is_some()
    }

    /// Draws the panel. Returns true while something is running, so the shell keeps
    /// repainting.
    pub fn show(&mut self, ui: &mut Ui) -> bool {
        let busy = self.pump();

        // Drawn before the sections so the dialog sits over the panel it is asking about.
        if widgets::confirm_modal(ui, &mut self.pending) == Some(true) {
            self.start_install();
        }

        self.section_adb(ui, busy);
        ui.add_space(theme::UNIT * 2.0);
        self.section_devices(ui);
        ui.add_space(theme::UNIT * 2.0);
        self.section_revive(ui);
        ui.add_space(theme::UNIT * 2.0);
        self.section_appdata(ui);

        // Keep the clock running so the device poll actually fires.
        busy || true
    }

    fn section_adb(&mut self, ui: &mut Ui, busy: bool) {
        widgets::section_label(ui, "ADB");
        ui.label(
            RichText::new(
                "Needed to talk to a Quest over USB. Not needed for anything on PC.",
            )
            .font(theme::font_ui(11.5))
            .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::UNIT * 0.75);

        match &self.adb {
            Some(found) => {
                // A green tick for something that does not run is worse than no tick: it
                // says "this is fine" about the thing that will fail at the next step.
                match found.version.as_deref() {
                    Some(v) => widgets::status(ui, Status::Ok, v.trim()),
                    None => widgets::status(
                        ui,
                        Status::Warn,
                        "this file is there but does not run as adb",
                    ),
                }
                widgets::mono_color(ui, &found.path.display().to_string(), 10.5, theme::TEXT_DIM);
                ui.label(
                    RichText::new(match found.source {
                        Source::Configured => "using the path you chose",
                        Source::Managed => "downloaded by this installer",
                        Source::OnPath => "found on your PATH",
                    })
                    .font(theme::font_ui(10.5))
                    .color(theme::TEXT_FAINT),
                );
            }
            None => {
                widgets::status(ui, Status::Warn, "not found");
                ui.label(
                    RichText::new(
                        "Choose an existing adb, or let the installer fetch Google's \
                         platform-tools into its own folder.",
                    )
                    .font(theme::font_ui(11.0))
                    .color(theme::TEXT_FAINT),
                );
            }
        }

        if let Some((stage, done, total)) = self.install_progress {
            ui.add_space(theme::UNIT * 0.75);
            let label = match stage {
                InstallStage::Downloading => "downloading platform-tools",
                InstallStage::Extracting => "unpacking",
            };
            let frac = match total {
                Some(t) if t > 0 => done as f32 / t as f32,
                _ => 0.0,
            };
            widgets::progress_row(
                ui,
                label,
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

        ui.add_space(theme::UNIT * 0.75);
        ui.horizontal(|ui| {
            if widgets::secondary(ui, "Choose...", !busy) {
                if let Some(file) = rfd::FileDialog::new().pick_file() {
                    self.settings.adb_path = Some(file);
                    self.settings.save();
                    self.rescan();
                }
            }
            if busy && widgets::secondary(ui, "Cancel", true) {
                // The download checks this between chunks; the partial file is kept, so a
                // cancel here costs nothing but the wait.
                self.cancel.cancel();
            }
            let label = if self.adb.is_some() { "Reinstall" } else { "Install" };
            if widgets::secondary(ui, label, !busy) {
                // Only a replacement is worth stopping for. A first install replaces
                // nothing and interrupts nothing.
                match self.adb.is_some() {
                    true => self.pending = Some(self.replacement_warning()),
                    false => self.start_install(),
                }
            }
            if widgets::secondary(ui, "Re-check", !busy) {
                self.rescan();
                if let Some(w) = &self.watcher {
                    w.poke();
                }
            }
            if self.settings.adb_path.is_some() && widgets::secondary(ui, "Clear choice", !busy) {
                // Back to automatic discovery, rather than silently keeping a stale path.
                self.settings.adb_path = None;
                self.settings.save();
                self.rescan();
            }
        });
    }

    fn section_devices(&mut self, ui: &mut Ui) {
        widgets::section_label(ui, "DEVICES");

        // A file that does not run as adb cannot answer this question either, and letting
        // the attempt fail here prints the OS message for it verbatim: "This version of %1
        // is not compatible with the version of Windows you're running", placeholder and
        // all. It is alarming, it is not true - the file is not a program at all, not a
        // mismatched one - and it repeats what the section above already said, further from
        // the buttons that would fix it.
        if self.adb.as_ref().is_some_and(|a| a.version.is_none()) {
            widgets::status(ui, Status::Warn, "nothing to look with until adb works");
            ui.label(
                RichText::new(
                    "The file chosen above does not run as adb. Choose another one, or \
                     clear the choice and let the installer fetch its own.",
                )
                .font(theme::font_ui(10.5))
                .color(theme::TEXT_FAINT),
            );
            return;
        }

        let Some(watcher) = &self.watcher else {
            ui.label(
                RichText::new("Nothing to look with until adb is set up.")
                    .font(theme::font_ui(11.5))
                    .color(theme::TEXT_FAINT),
            );
            return;
        };
        let snap = watcher.snapshot();

        if snap.still_looking() {
            widgets::status(ui, Status::Info, "looking for a headset...");
            return;
        }
        // A cable that keeps dropping is worth naming, rather than blinking the list on
        // and off and leaving the user to guess.
        if snap.unstable() {
            let ago = snap
                .since_seen()
                .map(|d| format!(", last seen {}s ago", d.as_secs()))
                .unwrap_or_default();
            widgets::status(
                ui,
                Status::Warn,
                &format!("the connection keeps dropping{ago}. Check the cable and the port."),
            );
            ui.add_space(theme::UNIT * 0.5);
        }
        if snap.devices().is_empty() {
            widgets::status(ui, Status::Info, "no headset connected");
            ui.label(
                RichText::new(
                    "Connect it by USB and make sure Developer Mode is on. This list \
                     refreshes on its own.",
                )
                .font(theme::font_ui(10.5))
                .color(theme::TEXT_FAINT),
            );
            if let Some(e) = snap.last_error() {
                ui.add_space(theme::UNIT * 0.5);
                widgets::status(ui, Status::Err, e);
            }
            return;
        }
        for d in snap.devices() {
            let kind = match d.state {
                State::Ready => Status::Ok,
                State::Unauthorized => Status::Warn,
                _ => Status::Err,
            };
            let name = d.model.clone().unwrap_or_else(|| d.serial.clone());
            widgets::status(ui, kind, &format!("{name}: {}", d.state.describe()));
            widgets::mono_color(ui, &d.serial, 10.5, theme::TEXT_FAINT);
            if d.state == State::Unauthorized {
                ui.label(
                    RichText::new(
                        "Put the headset on and tap Allow on the USB debugging prompt. \
                         Replug the cable if it does not appear.",
                    )
                    .font(theme::font_ui(10.5))
                    .color(theme::TEXT_DIM),
                );
            }
        }
    }

    fn section_revive(&mut self, ui: &mut Ui) {
        widgets::section_label(ui, "REVIVE");
        if !cfg!(windows) {
            ui.label(
                RichText::new("Windows only. Nothing to check here.")
                    .font(theme::font_ui(11.5))
                    .color(theme::TEXT_FAINT),
            );
            return;
        }
        ui.label(
            RichText::new(
                "Needed only to play through SteamVR. Not needed for a headset over USB, \
                 and not installed unless you ask: it is someone else's installer and it \
                 asks for administrator rights.",
            )
            .font(theme::font_ui(11.5))
            .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::UNIT * 0.75);

        let busy = self.revive_installing.is_some();
        match &self.revive {
            Some(found) => {
                widgets::status(ui, Status::Ok, "Revive is installed");
                widgets::mono_color(ui, &found.dir.display().to_string(), 10.5, theme::TEXT_DIM);
                ui.label(
                    RichText::new(found.source.describe())
                        .font(theme::font_ui(10.5))
                        .color(theme::TEXT_FAINT),
                );
            }
            // Bound in the pattern rather than tested in a guard and unwrapped after: the
            // unwrap was only safe because of the arm above it, which is the kind of thing
            // that stops being true when someone reorders a match.
            None if self.settings.revive_path.is_some() => {
                if let Some(chosen) = &self.settings.revive_path {
                    // A choice that stopped being valid is worth saying out loud rather
                    // than quietly falling back to a different copy.
                    widgets::status(
                        ui,
                        Status::Warn,
                        "the folder you chose has no ReviveInjector.exe",
                    );
                    widgets::mono_color(ui, &chosen.display().to_string(), 10.5, theme::TEXT_DIM);
                }
            }
            None => widgets::status(ui, Status::Info, "not installed"),
        }

        if let Some((done, total)) = self.revive_progress {
            ui.add_space(theme::UNIT * 0.5);
            widgets::progress_row(
                ui,
                "ReviveInstaller.exe",
                total.map(|t| done as f32 / t.max(1) as f32).unwrap_or(0.0),
                &match total {
                    Some(t) => format!("{} / {}", human_bytes(done), human_bytes(t)),
                    None => human_bytes(done),
                },
            );
        }
        if let Some(e) = &self.revive_error {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(ui, Status::Err, e);
        }

        ui.add_space(theme::UNIT * 0.75);
        ui.horizontal(|ui| {
            if widgets::secondary(ui, "Choose folder...", !busy) {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.settings.revive_path = Some(dir);
                    self.settings.save();
                    self.rescan();
                }
            }
            if busy && widgets::secondary(ui, "Cancel", true) {
                self.cancel.cancel();
            }
            let label = if self.revive.is_some() { "Reinstall" } else { "Install" };
            if widgets::secondary(ui, label, !busy) {
                self.start_revive_install();
            }
            if self.settings.revive_path.is_some() && widgets::secondary(ui, "Clear choice", !busy)
            {
                self.settings.revive_path = None;
                self.settings.save();
                self.rescan();
            }
        });
        if busy {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(
                ui,
                Status::Info,
                "Revive's installer asks for administrator rights. Answer the Windows prompt.",
            );
        }
    }

    fn start_revive_install(&mut self) {
        // Fresh, not reused: a cancel left set by an earlier run would stop this one before
        // it began.
        self.cancel = Cancel::new();
        let (tx, rx) = mpsc::channel();
        self.revive_installing = Some(rx);
        self.revive_error = None;
        self.revive_progress = Some((0, None));
        let cancel = self.cancel.clone();
        thread::spawn(move || {
            let tx2 = tx.clone();
            let result = crate::engine::revive::install(&cancel, &mut |done, total| {
                let _ = tx2.send(ReviveMsg::Progress { done, total });
            });
            let _ = tx.send(ReviveMsg::Finished(result.map(|_| ()).map_err(|e| e.to_string())));
        });
    }

    fn section_appdata(&mut self, ui: &mut Ui) {
        widgets::section_label(ui, "APP DATA");
        ui.label(
            RichText::new("Settings, logs and any adb this installer downloaded.")
                .font(theme::font_ui(11.5))
                .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::UNIT * 0.5);
        widgets::mono_color(ui, &config::dir().display().to_string(), 10.5, theme::TEXT_MUTED);
        ui.add_space(theme::UNIT * 0.75);
        if widgets::secondary(ui, "Open folder", true) {
            let _ = std::fs::create_dir_all(config::dir());
            let _ = widgets::open_path(&config::dir());
        }
    }
}
