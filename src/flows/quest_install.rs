// SPDX-License-Identifier: GPL-3.0-or-later
//! Install Echo VR on a Quest.
//!
//! The step list changes with the answer to the first question, because a new player needs
//! a personalised APK and an owner does not. Showing an authorisation step to someone who
//! will never use it, or hiding one that is required, are both worse than a step list that
//! reflects the actual path.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use egui::{RichText, Ui};

use crate::config::{self, Settings};
use crate::endpoints;
use crate::engine::adb::{self, Adb, Device};
use crate::engine::watch::DeviceWatcher;
use crate::engine::download;
use crate::engine::manifest::Manifest;
use crate::engine::patch::{self, Kind};
use crate::engine::quest::Quest;
use crate::engine::quest_install::{self as engine, Config, Report};
use crate::engine::Cancel;
use crate::fmt::human_bytes;
use crate::theme;
use crate::widgets::{self, RowState, Status};

const STEPS_STOCK: &[&str] = &["Licence", "Headset", "Download", "Install", "Done"];
const STEPS_PATCHED: &[&str] =
    &["Licence", "Headset", "Authorise", "Download", "Install", "Done"];
/// Stages the install engine emits, so the checklist exists before any of them happen.
const INSTALL_STAGES: &[&str] = &[
    "Removing the previous install",
    "Installing Echo VR",
    "Verifying the install",
    "Copying game data",
    "Unpacking on the headset",
    "Granting permissions",
    "Recording the version",
    "Applying the current update",
];

#[derive(Clone, Copy, PartialEq)]
enum Licence {
    Owner,
    NewPlayer,
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
    Auth(patch::Progress),
    AuthDone(Result<String, String>),
    /// Carries the invite the server itself supplied.
    NotInGuild { message: String, invite: String },
    Stage(&'static str),
    Mirror(String),
    Probing { base: String, index: usize, of: usize },
    Download { what: String, done: u64, total: Option<u64> },
    Downloaded(PathBuf, PathBuf),
    Finished(Result<Report, String>),
}

pub struct QuestInstall {
    settings: Settings,
    licence: Option<Licence>,

    adb: Option<adb::Located>,
    watcher: Option<DeviceWatcher>,
    /// Chosen by serial, not by position: the list is re-read constantly and an index
    /// silently means a different headset the moment one is unplugged.
    chosen: Option<String>,

    auth_phase: Phase,
    auth_note: Option<String>,
    patch_url: Option<String>,
    manual_url: String,
    auth_error: Option<String>,
    guild_invite: Option<String>,

    dl_phase: Phase,
    files: Option<(PathBuf, PathBuf)>,
    mirror: Option<String>,
    progress: Option<(String, u64, Option<u64>)>,

    phase: Phase,
    stage: Option<&'static str>,
    /// One line under the current stage, for a stage that would otherwise sit silent.
    stage_detail: Option<String>,
    report: Option<Report>,
    error: Option<String>,

    cancel: Cancel,
    rx: Option<Receiver<Msg>>,
    log: crate::log::Ring,
    log_open: bool,
    launch_result: Option<String>,
    pending: Option<crate::flows::Confirm>,
}

impl Default for QuestInstall {
    fn default() -> Self {
        let settings = Settings::load();
        let mut f = QuestInstall {
            licence: None,
            adb: None,
            watcher: None,
            chosen: None,
            auth_phase: Phase::Idle,
            auth_note: None,
            patch_url: None,
            manual_url: String::new(),
            auth_error: None,
            guild_invite: None,
            dl_phase: Phase::Idle,
            files: None,
            mirror: None,
            progress: None,
            phase: Phase::Idle,
            stage: None,
            stage_detail: None,
            report: None,
            error: None,
            cancel: Cancel::new(),
            rx: None,
            log: crate::log::Ring::default(),
            log_open: false,
            launch_result: None,
            pending: None,
            settings,
        };
        f.rescan();
        f
    }
}

impl QuestInstall {
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

    fn needs_patch(&self) -> bool {
        self.licence == Some(Licence::NewPlayer)
    }

    fn start_auth(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.auth_phase = Phase::Running;
        self.auth_error = None;
        self.guild_invite = None;
        self.auth_note = None;
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        thread::spawn(move || {
            let tx2 = tx.clone();
            let result = patch::obtain(Kind::Apk, &cancel, &mut |p| {
                let _ = tx2.send(Msg::Auth(p));
            });
            match result {
                Ok(url) => {
                    let _ = tx.send(Msg::AuthDone(Ok(url)));
                }
                Err(patch::Error::NotInGuild { message, invite }) => {
                    let _ = tx.send(Msg::NotInGuild { message, invite });
                }
                Err(e) => {
                    let _ = tx.send(Msg::AuthDone(Err(e.to_string())));
                }
            }
        });
    }

    fn start_download(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.dl_phase = Phase::Running;
        self.files = None;
        self.progress = None;
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        let patched = self.patch_url.clone();
        thread::spawn(move || download_all(patched, cancel, tx));
    }

    fn start_install(&mut self) {
        let (Some(path), Some(device), Some((apk, data))) = (
            self.adb.as_ref().map(|a| a.path.clone()),
            self.ready_device(),
            self.files.clone(),
        ) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.phase = Phase::Running;
        self.stage = None;
        self.error = None;
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        let patched = self.patch_url.clone();
        thread::spawn(move || install_all(path, device, apk, data, patched, cancel, tx));
    }

    fn pump(&mut self) {
        let (inbox, _) = crate::channel::drain(&self.rx);
        for msg in inbox {
            match msg {
                Msg::Log(l) => self.log.push(l),
                Msg::Auth(p) => {
                    self.auth_note = Some(
                        match p {
                            patch::Progress::WaitingForBrowser => {
                                "Waiting for you to authorise in the browser..."
                            }
                            patch::Progress::Generating => {
                                "Authorised. The bot is building your file, about ten seconds..."
                            }
                        }
                        .into(),
                    );
                }
                Msg::AuthDone(Ok(url)) => {
                    self.patch_url = Some(url);
                    self.auth_phase = Phase::Succeeded;
                }
                Msg::AuthDone(Err(e)) => {
                    self.auth_error = Some(e);
                    self.auth_phase = Phase::Failed;
                }
                Msg::NotInGuild { message, invite } => {
                    self.auth_error = Some(message);
                    self.guild_invite = Some(invite);
                    self.auth_phase = Phase::Failed;
                }
                Msg::Stage(s) => self.stage = Some(s),
                Msg::Mirror(m) => {
                    self.mirror = Some(m);
                    self.stage_detail = None;
                }
                // The server probe is the one stage with nothing to show while it runs.
                Msg::Probing { base, index, of } => {
                    self.stage_detail = Some(format!("trying {base}  ({index} of {of})"));
                }
                Msg::Download { what, done, total } => self.progress = Some((what, done, total)),
                Msg::Downloaded(apk, data) => {
                    self.files = Some((apk, data));
                    self.dl_phase = Phase::Succeeded;
                    self.progress = None;
                }
                Msg::Finished(Ok(r)) => {
                    self.report = Some(r);
                    self.phase = Phase::Succeeded;
                }
                Msg::Finished(Err(e)) => {
                    // The same channel carries both jobs, so the failure belongs to
                    // whichever is running.
                    if self.dl_phase == Phase::Running {
                        self.dl_phase = Phase::Failed;
                    } else {
                        self.phase = Phase::Failed;
                    }
                    self.error = Some(e);
                }
            }
        }
    }
}

/// Fetches the manifest for the APK name, then the APK and the data archive.
fn download_all(patched: Option<String>, cancel: Cancel, tx: mpsc::Sender<Msg>) {
    let say = |l: String| {
        let _ = tx.send(Msg::Log(l));
    };

    let text = match download::fetch_text_cancellable(endpoints::QUEST_MANIFEST, &cancel, &mut |_, _| {}) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Msg::Finished(Err(format!("Could not read the file list: {e}"))));
            return;
        }
    };
    let manifest = match Manifest::parse(&text, endpoints::QUEST_MANIFEST) {
        Ok(m) => m,
        Err(e) => {
            let _ = tx.send(Msg::Finished(Err(format!("The file list is not valid: {e}"))));
            return;
        }
    };
    // No built-in fallback on purpose: the original's stale one installs a six-week-old
    // build that its own version gate then refuses forever.
    let Some(base) = manifest.base_apk() else {
        let _ = tx.send(Msg::Finished(Err(
            "The file list does not say which build to install. Try again later.".into(),
        )));
        return;
    };
    say(format!("build {}", base.name));

    let cfg = Config {
        apk_name: base.name.clone(),
        base_sha256: base.sha256.clone(),
        patched_url: patched,
        mirrors: endpoints::MIRRORS.iter().map(|s| s.to_string()).collect(),
        probe: endpoints::MIRROR_PROBE.into(),
        staging: config::dir().join("staging"),
        installer_version: crate::app::VERSION.to_string(),
    };

    let tx2 = tx.clone();
    match engine::download(&cfg, &cancel, &mut |e| match e {
        engine::Event::Stage(s) => {
            let _ = tx2.send(Msg::Stage(s));
        }
        engine::Event::Probing { base, index, of } => {
            let _ = tx2.send(Msg::Probing { base, index, of });
        }
        engine::Event::Mirror(m) => {
            let _ = tx2.send(Msg::Log(format!("mirror {m}")));
            let _ = tx2.send(Msg::Mirror(m));
        }
        // Into the log rather than the headline: it is not a failure, and the download is
        // still going. It is there so a later failure has a history behind it.
        engine::Event::MirrorProblem(m) => {
            let _ = tx2.send(Msg::Log(m));
        }
        engine::Event::Downloading { what, done, total } => {
            let _ = tx2.send(Msg::Download { what, done, total });
        }
    }) {
        Ok((apk, data)) => {
            say("downloads complete".into());
            let _ = tx.send(Msg::Downloaded(apk, data));
        }
        Err(e) => {
            let _ = tx.send(Msg::Finished(Err(e.to_string())));
        }
    }
}

fn install_all(
    adb_path: PathBuf,
    device: Device,
    apk: PathBuf,
    data: PathBuf,
    patched: Option<String>,
    cancel: Cancel,
    tx: mpsc::Sender<Msg>,
) {
    let adb = Adb::at(&adb_path);
    let q = Quest::new(&adb, Some(&device));

    // Fetched again rather than carried across: the install may run minutes after the
    // download, and this is what decides both the version record and what gets updated.
    let text = download::fetch_text_cancellable(endpoints::QUEST_MANIFEST, &cancel, &mut |_, _| {}).unwrap_or_default();
    let manifest = Manifest::parse(&text, endpoints::QUEST_MANIFEST).ok();
    let base = manifest.as_ref().and_then(|m| m.base_apk().cloned());

    let cfg = Config {
        apk_name: base.as_ref().map(|b| b.name.clone()).unwrap_or_default(),
        base_sha256: base.as_ref().map(|b| b.sha256.clone()).unwrap_or_default(),
        patched_url: patched,
        mirrors: Vec::new(),
        probe: String::new(),
        staging: config::dir().join("staging"),
        installer_version: crate::app::VERSION.to_string(),
    };

    let tx2 = tx.clone();
    let result = engine::install(&cfg, &apk, &data, manifest.as_ref(), &q, &cancel, &mut |e| {
        if let engine::Event::Stage(s) = e {
            let _ = tx2.send(Msg::Log(format!("stage: {s}")));
            let _ = tx2.send(Msg::Stage(s));
        }
    });
    let _ = tx.send(Msg::Finished(result.map_err(|e| e.to_string())));
}

impl crate::flows::Flow for QuestInstall {
    /// Going back voids the authorisation, the download and the install. The headset and
    /// the licence answer are kept; everything derived from them is not.
    fn reset_after(&mut self, _step: usize) {
        self.cancel.cancel();
        self.rx = None;
        self.auth_phase = Phase::Idle;
        self.auth_note = None;
        self.auth_error = None;
        self.patch_url = None;
        self.dl_phase = Phase::Idle;
        self.files = None;
        self.mirror = None;
        self.progress = None;
        self.phase = Phase::Idle;
        self.stage = None;
        self.report = None;
        self.error = None;
        self.launch_result = None;
        self.log.clear();
    }

    fn steps(&self) -> &'static [&'static str] {
        if self.needs_patch() {
            STEPS_PATCHED
        } else {
            STEPS_STOCK
        }
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

    fn blocked_reason(&self, step: usize) -> Option<String> {
        let names = self.steps();
        match names.get(step).copied() {
            Some("Licence") if self.licence.is_none() => {
                Some("Choose whether you own Echo VR".into())
            }
            Some("Headset") if self.adb.is_none() => Some("adb is not set up".into()),
            Some("Headset") if self.ready_device().is_none() => {
                Some("Connect a headset and allow this computer".into())
            }
            Some("Authorise") if self.patch_url.is_none() => {
                Some("Get your patched build first".into())
            }
            Some("Download") => match self.dl_phase {
                Phase::Running => Some("Downloading".into()),
                Phase::Succeeded => None,
                _ => Some("Download the files first".into()),
            },
            Some("Install") => match self.phase {
                Phase::Running => Some("Install in progress".into()),
                Phase::Succeeded => None,
                _ => Some("Run the install first".into()),
            },
            _ => None,
        }
    }

    fn on_exit(&mut self) {
        self.cancel.cancel();
    }

    fn content(&mut self, ui: &mut Ui, step: usize, signals: &mut crate::flows::Signals) {
        self.pump();
        if self.phase == Phase::Running
            || self.dl_phase == Phase::Running
            || self.auth_phase == Phase::Running
        {
            signals.keep_repainting = true;
        }
        match self.steps().get(step).copied() {
            Some("Licence") => self.step_licence(ui),
            Some("Headset") => self.step_headset(ui, signals),
            Some("Authorise") => self.step_authorise(ui),
            Some("Download") => self.step_download(ui),
            Some("Install") => self.step_install(ui),
            _ => self.step_done(ui),
        }
    }
}

impl QuestInstall {
    fn step_licence(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("Do you own Echo VR on your Meta account?")
                .font(theme::font_ui(12.5))
                .color(theme::TEXT_MUTED),
        );
        ui.add_space(theme::UNIT * 1.5);
        if widgets::option_row(
            ui,
            self.licence == Some(Licence::Owner),
            "I own Echo VR on Meta",
            "Installs the original build straight from the mirrors.",
        ) {
            self.licence = Some(Licence::Owner);
        }
        ui.add_space(theme::UNIT * 0.75);
        if widgets::option_row(
            ui,
            self.licence == Some(Licence::NewPlayer),
            "I'm a new player",
            "Adds a step: Discord builds a copy tied to your account.",
        ) {
            self.licence = Some(Licence::NewPlayer);
        }
    }

    fn step_headset(&mut self, ui: &mut Ui, signals: &mut crate::flows::Signals) {
        signals.keep_repainting = true;

        if self.adb.is_none() {
            widgets::status(ui, Status::Err, "adb is not set up");
            ui.label(
                RichText::new("Open Dependencies from the home screen to set it up.")
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

        ui.add_space(theme::UNIT * 1.5);
        widgets::status(
            ui,
            Status::Warn,
            "Installing replaces any Echo VR already on this headset, and its saved data.",
        );
    }

    fn step_authorise(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new(
                "Discord builds a copy of Echo VR tied to your account. You will be asked to \
                 authorise access to your profile and server list, nothing else.",
            )
            .font(theme::font_ui(12.0))
            .color(theme::TEXT_MUTED),
        );
        ui.add_space(theme::UNIT * 1.25);

        match self.auth_phase {
            Phase::Succeeded => {
                widgets::status(ui, Status::Ok, "Your build is ready to download");
                ui.add_space(theme::UNIT * 0.5);
                ui.label(
                    RichText::new(
                        "The link is personal to your account and stops working after 24 hours.",
                    )
                    .font(theme::font_ui(10.5))
                    .color(theme::TEXT_FAINT),
                );
            }
            Phase::Running => {
                widgets::status(
                    ui,
                    Status::Info,
                    self.auth_note.as_deref().unwrap_or("Opening your browser..."),
                );
                ui.add_space(theme::UNIT * 0.75);
                if widgets::secondary(ui, "Cancel", true) {
                    self.cancel.cancel();
                    self.auth_phase = Phase::Idle;
                }
            }
            _ => {
                if let Some(e) = &self.auth_error {
                    widgets::status(ui, Status::Err, e);
                    if let Some(invite) = self.guild_invite.clone() {
                        ui.add_space(theme::UNIT * 0.5);
                        widgets::external_link(ui, "Join the patcher server", &invite);
                    }
                    ui.add_space(theme::UNIT * 0.75);
                }
                if widgets::primary(ui, "Authorise with Discord", true) {
                    self.start_auth();
                }
                ui.add_space(theme::UNIT * 1.5);
                widgets::field_label(ui, "Or paste a link you already have");
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 90.0).clamp(180.0, 380.0);
                    widgets::path_field(ui, &mut self.manual_url, w);
                    ui.add_space(theme::UNIT * 0.5);
                    let usable = looks_like_patch_url(&self.manual_url);
                    if widgets::secondary(ui, "Use this", usable) {
                        self.patch_url = Some(self.manual_url.trim().to_string());
                        self.auth_phase = Phase::Succeeded;
                    }
                });
            }
        }
    }

    fn step_download(&mut self, ui: &mut Ui) {
        if self.dl_phase == Phase::Idle {
            ui.label(
                RichText::new(
                    "Fetches the build and its game data from the fastest mirror. The data \
                     archive is a little under a gigabyte.",
                )
                .font(theme::font_ui(12.0))
                .color(theme::TEXT_MUTED),
            );
            ui.add_space(theme::UNIT * 1.5);
            if widgets::primary(ui, "Start download", true) {
                self.start_download();
            }
            return;
        }

        if let Some(m) = &self.mirror {
            widgets::mono_color(ui, m, 10.5, theme::TEXT_FAINT);
            ui.add_space(theme::UNIT * 0.5);
        }
        if let Some((what, done, total)) = &self.progress {
            let frac = match total {
                Some(t) if *t > 0 => *done as f32 / *t as f32,
                _ => 0.0,
            };
            widgets::progress_row(
                ui,
                what,
                frac,
                &match total {
                    Some(t) => format!("{} / {}", human_bytes(*done), human_bytes(*t)),
                    None => human_bytes(*done),
                },
            );
        }

        ui.add_space(theme::UNIT * 0.75);
        match self.dl_phase {
            Phase::Succeeded => widgets::status(ui, Status::Ok, "Files ready"),
            Phase::Failed => {
                widgets::status(ui, Status::Err, self.error.as_deref().unwrap_or("Download failed"));
                ui.add_space(theme::UNIT * 0.5);
                if widgets::secondary(ui, "Retry", true) {
                    self.start_download();
                }
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

    fn step_install(&mut self, ui: &mut Ui) {
        if self.phase == Phase::Idle {
            widgets::status(
                ui,
                Status::Warn,
                "This removes any Echo VR already on the headset before installing.",
            );
            ui.add_space(theme::UNIT);
            ui.label(
                RichText::new("Keep the cable connected. Sending the game data takes a few minutes.")
                    .font(theme::font_ui(12.0))
                    .color(theme::TEXT_MUTED),
            );
            ui.add_space(theme::UNIT * 1.25);
            if widgets::primary(ui, "Install to headset", true) {
                self.pending = Some(crate::flows::Confirm {
                    title: "This replaces what is on the headset".into(),
                    consequence: "The existing Echo VR package and its game data are \
                                  removed and written again from what was downloaded.\n\n\
                                  Anything the game has stored on the headset goes with \
                                  it. The headset must stay connected for the whole \
                                  install; unplugging partway leaves it with neither the \
                                  old copy nor the new one."
                        .into(),
                    proceed: "Replace it".into(),
                });
            }
            if widgets::confirm_modal(ui, &mut self.pending) == Some(true) {
                self.start_install();
            }
            return;
        }

        let at = self
            .stage
            .and_then(|s| INSTALL_STAGES.iter().position(|x| *x == s))
            .unwrap_or(0);
        let failed = self.phase == Phase::Failed;
        let done = self.phase == Phase::Succeeded;
        widgets::card(ui, |ui| {
            for (i, name) in INSTALL_STAGES.iter().enumerate() {
                let state = if done || i < at {
                    RowState::Done
                } else if i == at {
                    if failed {
                        RowState::Failed
                    } else {
                        RowState::Working
                    }
                } else {
                    RowState::Pending
                };
                widgets::check_row(ui, state, name);
            }
        });

        ui.add_space(theme::UNIT * 0.75);
        match self.phase {
            Phase::Succeeded => widgets::status(ui, Status::Ok, "Installed"),
            Phase::Failed => {
                widgets::status(ui, Status::Err, self.error.as_deref().unwrap_or("Install failed"));
                ui.add_space(theme::UNIT * 0.5);
                ui.horizontal(|ui| {
                    if widgets::secondary(ui, "Retry", true) {
                        self.start_install();
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
        widgets::status(ui, Status::Ok, "Echo VR is installed on your headset");
        ui.add_space(theme::UNIT);
        if let Some(r) = &self.report {
            widgets::card(ui, |ui| {
                widgets::kv(ui, "Build     ", if r.patched { "personalised" } else { "original" });
                widgets::kv(ui, "Recorded  ", &crate::fmt::short_hash(&r.apk_sha256));
            });
        }
        ui.add_space(theme::UNIT * 1.5);
        self.launch_button(ui);
        ui.add_space(theme::UNIT);
        ui.label(
            RichText::new(
                "Launch it from your headset's library as usual. If it ever offers to restore \
                 data, decline: that would undo the install.",
            )
            .font(theme::font_ui(11.5))
            .color(theme::TEXT_MUTED),
        );
    }

    /// Starts the game on the headset, for when it is easier to press a button here than to
    /// put the headset on and find it.
    ///
    /// Kept on the UI thread deliberately: `am start` returns as soon as the intent is
    /// delivered, so there is nothing to wait on.
    fn launch_button(&mut self, ui: &mut Ui) {
        let device = self.ready_device();
        let adb_path = self.adb.as_ref().map(|a| a.path.clone());
        ui.horizontal(|ui| {
            let can = device.is_some() && adb_path.is_some();
            if widgets::secondary(ui, "Launch on headset", can) {
                if let (Some(path), Some(d)) = (adb_path, device) {
                    let adb = Adb::at(&path);
                    let q = Quest::new(&adb, Some(&d));
                    self.launch_result = Some(match q.launch() {
                        Ok(()) => "Started. Put the headset on.".to_string(),
                        Err(e) => format!("Could not start it: {e}"),
                    });
                }
            }
            if !can {
                ui.label(
                    RichText::new("connect the headset to use this")
                        .font(theme::font_ui(10.5))
                        .color(theme::TEXT_FAINT),
                );
            }
        });
        if let Some(msg) = self.launch_result.clone() {
            ui.add_space(theme::UNIT * 0.5);
            let ok = msg.starts_with("Started");
            widgets::status(ui, if ok { Status::Ok } else { Status::Err }, &msg);
        }
    }
}

/// Accepts the two shapes a patch link actually takes: the Discord CDN link the bot hands
/// out, and a direct link on the project's own host. Deliberately not anchored to `.dll` or
/// `.apk` at the end, because the CDN appends a signature query string.
pub fn looks_like_patch_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://cdn.discordapp.com/attachments/")
        || url.starts_with("https://files.echovr.de/")
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::flows::Flow;

    #[test]
    fn the_step_list_follows_the_licence_answer() {
        let mut f = QuestInstall::default();
        assert_eq!(f.steps(), STEPS_STOCK);
        f.licence = Some(Licence::NewPlayer);
        assert_eq!(f.steps(), STEPS_PATCHED, "a new player needs the authorise step");
        assert_eq!(f.steps().len(), STEPS_STOCK.len() + 1);
        f.licence = Some(Licence::Owner);
        assert_eq!(f.steps(), STEPS_STOCK, "an owner should never see it");
    }

    /// Blocking is keyed off step names rather than indices, precisely because the indices
    /// move when the licence answer changes.
    #[test]
    fn blocking_follows_the_step_name_not_its_index() {
        let mut f = QuestInstall { licence: Some(Licence::NewPlayer), ..Default::default() };
        let authorise = f.steps().iter().position(|s| *s == "Authorise").unwrap();
        assert!(f.blocked_reason(authorise).is_some());
        f.patch_url = Some("https://files.echovr.de/x".into());
        assert!(f.blocked_reason(authorise).is_none());

        // The same index on the stock path is a different step entirely.
        f.licence = Some(Licence::Owner);
        assert_eq!(f.steps()[authorise], "Download");
    }

    #[test]
    fn accepts_the_two_real_link_shapes() {
        assert!(looks_like_patch_url(
            "https://cdn.discordapp.com/attachments/1/2/pnsovr.dll?ex=a&is=b&hm=c"
        ));
        assert!(looks_like_patch_url("https://files.echovr.de/whatever.apk"));
        // Not anchored to an extension, because the CDN appends a signature.
        assert!(looks_like_patch_url("https://cdn.discordapp.com/attachments/1/2/x.apk?ex=1"));
        assert!(!looks_like_patch_url("http://cdn.discordapp.com/attachments/1/2/x"));
        assert!(!looks_like_patch_url("https://evil.example.com/x.apk"));
        assert!(!looks_like_patch_url(""));
    }

    #[test]
    fn install_stays_blocked_until_the_files_are_downloaded() {
        let mut f = QuestInstall { licence: Some(Licence::Owner), ..Default::default() };
        let install = f.steps().iter().position(|s| *s == "Install").unwrap();
        assert!(f.blocked_reason(install).is_some());
        f.phase = Phase::Succeeded;
        assert!(f.blocked_reason(install).is_none());
    }
}
