// SPDX-License-Identifier: GPL-3.0-or-later
//! Apply the licence patch to a PC install.
//!
//! Four steps. Getting the patch and applying it are separate because they fail for
//! unrelated reasons and have unrelated fixes: a link can expire, a path can be wrong, and
//! conflating them costs someone another trip through Discord for a mistake they could
//! have corrected locally.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use egui::{RichText, Ui};

use crate::config;
use crate::endpoints;
use crate::engine::download::Snapshot;
use crate::engine::install::{self, Inspection};
use crate::engine::patch::{self, Kind};
use crate::engine::pc_patch;
use crate::engine::Cancel;
use crate::fmt::human_bytes;
use crate::theme;
use crate::widgets::{self, Status};

const STEPS: &[&str] = &["Install path", "Get patch", "Apply", "Done"];

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Running,
    Succeeded,
    Failed,
}

enum Msg {
    AuthNote(String),
    AuthDone(Result<String, String>),
    NotInGuild { message: String, invite: String },
    Progress(Snapshot),
    Staged(PathBuf),
    Failed { message: String, needs_new_link: bool },
}

pub struct PcPatch {
    path: String,
    /// Where the prefilled or adopted folder came from. Shown under the field, because a
    /// suggestion whose reasoning is invisible is the app deciding.
    path_note: Option<&'static str>,
    inspection: Inspection,

    auth_phase: Phase,
    auth_note: Option<String>,
    auth_error: Option<String>,
    guild_invite: Option<String>,
    manual_url: String,
    patch_url: Option<String>,

    stage_phase: Phase,
    staged: Option<PathBuf>,
    progress: Option<Snapshot>,
    error: Option<String>,
    needs_new_link: bool,

    applied_to: Option<PathBuf>,

    cancel: Cancel,
    rx: Option<Receiver<Msg>>,
    /// Applying failed only for want of rights, so the broker can redo just the copy.
    needs_elevation: bool,
    elevated: crate::flows::elevated::Elevated,
}

impl Default for PcPatch {
    fn default() -> Self {
        // Both of these act on an existing install, so they want the same suggestion the
        // update flow wants: where Echo actually is, not where it would go.
        let (path, path_note) = crate::config::suggested_update_path(guessed_path);
        let inspection = install::inspect(std::path::Path::new(&path));
        PcPatch {
            path,
            path_note,
            inspection,
            auth_phase: Phase::Idle,
            auth_note: None,
            auth_error: None,
            guild_invite: None,
            manual_url: String::new(),
            patch_url: None,
            stage_phase: Phase::Idle,
            staged: None,
            progress: None,
            error: None,
            needs_new_link: false,
            applied_to: None,
            cancel: Cancel::new(),
            rx: None,
            needs_elevation: false,
            elevated: Default::default(),
        }
    }
}

fn guessed_path() -> String {
    if cfg!(windows) {
        "C:\\EchoVR".to_string()
    } else {
        format!("{}/EchoVR", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
    }
}

impl PcPatch {
    fn reinspect(&mut self) {
        self.inspection = install::inspect(std::path::Path::new(&self.path));
    }

    fn start_auth(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.auth_phase = Phase::Running;
        self.auth_error = None;
        self.guild_invite = None;
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        thread::spawn(move || {
            let tx2 = tx.clone();
            let result = patch::obtain(Kind::Dll, &cancel, &mut |p| {
                let note = match p {
                    patch::Progress::WaitingForBrowser => {
                        "Waiting for you to authorise in the browser..."
                    }
                    patch::Progress::Generating => {
                        "Authorised. The bot is building your patch, about ten seconds..."
                    }
                };
                let _ = tx2.send(Msg::AuthNote(note.into()));
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

    /// Redoes only the copy, elevated.
    ///
    /// Not the whole command: the patch is personal, single-use and expires after a day, so
    /// asking Discord again is not a retry, it is a second request the user has to sit
    /// through. The file is already on disk, so the elevated run is handed that.
    fn start_elevated_apply(&mut self) {
        let Some(staged) = self.staged.clone() else { return };
        self.error = None;
        self.needs_elevation = false;
        self.elevated.start(vec![
            "patch".into(),
            "--path".into(),
            self.path.clone(),
            "--from".into(),
            staged.display().to_string(),
        ]);
    }

    fn start_stage(&mut self) {
        let Some(url) = self.patch_url.clone() else { return };
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.stage_phase = Phase::Running;
        self.error = None;
        self.needs_new_link = false;
        self.progress = None;
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        let staging = config::dir().join("staging");
        thread::spawn(move || {
            let tx2 = tx.clone();
            match pc_patch::stage(&url, &staging, &cancel, &mut |s| {
                let _ = tx2.send(Msg::Progress(s));
            }) {
                Ok(path) => {
                    let _ = tx.send(Msg::Staged(path));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Failed {
                        needs_new_link: e.needs_new_link(),
                        message: e.to_string(),
                    });
                }
            }
        });
    }

    fn pump(&mut self) {
        for update in self.elevated.poll() {
            match update {
                crate::flows::elevated::Update::Finished => {
                    // The elevated copy placed it; re-read rather than assume.
                    self.applied_to = Some(install::bin_dir(std::path::Path::new(&self.path)));
                    self.error = None;
                }
                crate::flows::elevated::Update::Failed(e) => self.error = Some(e),
                _ => {}
            }
        }

        let (inbox, _) = crate::channel::drain(&self.rx);
        for msg in inbox {
            match msg {
                Msg::AuthNote(n) => self.auth_note = Some(n),
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
                Msg::Progress(s) => self.progress = Some(s),
                Msg::Staged(p) => {
                    self.staged = Some(p);
                    self.stage_phase = Phase::Succeeded;
                }
                Msg::Failed { message, needs_new_link } => {
                    self.error = Some(message);
                    self.needs_new_link = needs_new_link;
                    self.stage_phase = Phase::Failed;
                }
            }
        }
    }
}

impl crate::flows::Flow for PcPatch {
    /// Going back voids everything downstream of the authorisation, and the authorisation
    /// with it: a patch link is tied to one Discord round trip and expires in 24 hours, so
    /// carrying one across a restarted run is worse than asking again.
    fn reset_after(&mut self, _step: usize) {
        self.cancel.cancel();
        self.rx = None;
        self.auth_phase = Phase::Idle;
        self.auth_note = None;
        self.auth_error = None;
        self.patch_url = None;
        self.stage_phase = Phase::Idle;
        self.staged = None;
        self.progress = None;
        self.error = None;
        self.needs_new_link = false;
        self.applied_to = None;
        self.reinspect();
    }

    fn steps(&self) -> &'static [&'static str] {
        STEPS
    }

    fn status_note(&self) -> Option<(bool, String)> {
        Some((true, "no extra tools needed for this".into()))
    }

    fn blocked_reason(&self, step: usize) -> Option<String> {
        match step {
            0 if !self.inspection.has_echo => {
                Some("Point at a folder containing echovr.exe".into())
            }
            1 if self.patch_url.is_none() => Some("Get your patch first".into()),
            2 if self.applied_to.is_none() => Some("Apply the patch first".into()),
            _ => None,
        }
    }

    fn on_exit(&mut self) {
        self.cancel.cancel();
    }

    fn content(&mut self, ui: &mut Ui, step: usize, signals: &mut crate::flows::Signals) {
        self.pump();
        if self.auth_phase == Phase::Running || self.stage_phase == Phase::Running {
            signals.keep_repainting = true;
        }
        match step {
            0 => self.step_path(ui),
            1 => self.step_get(ui),
            2 => self.step_apply(ui),
            _ => self.step_done(ui),
        }
    }
}

impl PcPatch {
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

        // The one place in the app where a failed check does block: the patch is a file
        // dropped beside the game, and dropping it anywhere else silently does nothing.
        if self.inspection.has_echo {
            widgets::status(ui, Status::Ok, "echovr.exe found at this path");
        } else if self.inspection.root_exists {
            widgets::status(ui, Status::Err, "no echovr.exe here");
            ui.label(
                RichText::new("The patch sits next to the game, so this has to be the install folder.")
                    .font(theme::font_ui(11.0))
                    .color(theme::TEXT_MUTED),
            );
        } else {
            widgets::status(ui, Status::Err, "this folder does not exist");
        }
        if self.inspection.has_echo && !self.inspection.writable {
            widgets::status(ui, Status::Warn, "this folder needs administrator rights to write to");
        }
    }

    fn step_get(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new(
                "Discord builds a patch tied to your account. You will be asked to authorise \
                 access to your profile and server list, nothing else.",
            )
            .font(theme::font_ui(12.0))
            .color(theme::TEXT_MUTED),
        );
        ui.add_space(theme::UNIT * 1.25);

        match self.auth_phase {
            Phase::Succeeded => {
                widgets::status(ui, Status::Ok, "Your patch is ready to download");
                ui.label(
                    RichText::new("The link is personal to you and stops working after 24 hours.")
                        .font(theme::font_ui(10.5))
                        .color(theme::TEXT_FAINT),
                );
                ui.add_space(theme::UNIT * 0.75);
                if widgets::secondary(ui, "Get a different one", true) {
                    self.auth_phase = Phase::Idle;
                    self.patch_url = None;
                }
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
                    let usable = crate::flows::quest_install::looks_like_patch_url(&self.manual_url);
                    if widgets::secondary(ui, "Use this", usable) {
                        self.patch_url = Some(self.manual_url.trim().to_string());
                        self.auth_phase = Phase::Succeeded;
                    }
                });
            }
        }
    }

    fn step_apply(&mut self, ui: &mut Ui) {
        if self.stage_phase == Phase::Idle {
            ui.label(
                RichText::new("Downloads the patch, then copies it next to echovr.exe.")
                    .font(theme::font_ui(12.0))
                    .color(theme::TEXT_MUTED),
            );
            ui.add_space(theme::UNIT * 1.5);
            if widgets::primary(ui, "Apply patch", true) {
                self.start_stage();
            }
            return;
        }

        if let Some(s) = self.progress {
            widgets::progress_row(
                ui,
                pc_patch::PATCH_FILE,
                s.fraction().unwrap_or(0.0),
                &match s.total {
                    Some(t) => format!("{} / {}", human_bytes(s.done), human_bytes(t)),
                    None => human_bytes(s.done),
                },
            );
            ui.add_space(theme::UNIT * 0.5);
        }

        match self.stage_phase {
            Phase::Running => {
                if widgets::secondary(ui, "Cancel", true) {
                    self.cancel.cancel();
                    self.stage_phase = Phase::Idle;
                }
            }
            Phase::Failed => {
                widgets::status(ui, Status::Err, self.error.as_deref().unwrap_or("Failed"));
                ui.add_space(theme::UNIT * 0.5);
                ui.horizontal(|ui| {
                    // Retrying an expired link can only fail the same way, so the offer is
                    // to go back and get a new one instead.
                    if self.needs_new_link {
                        if widgets::secondary(ui, "Get a new link", true) {
                            self.auth_phase = Phase::Idle;
                            self.patch_url = None;
                            self.stage_phase = Phase::Idle;
                        }
                    } else if widgets::secondary(ui, "Retry", true) {
                        self.start_stage();
                    }
                    widgets::external_link(ui, "Ask for help on Discord", endpoints::DISCORD_LOUNGE);
                });
            }
            Phase::Succeeded if self.applied_to.is_none() => {
                // Downloaded but not yet placed: the copy is instant, so it happens on the
                // click rather than on another worker.
                widgets::status(ui, Status::Ok, "Downloaded");
                ui.add_space(theme::UNIT * 0.75);
                if widgets::primary(ui, "Copy into the game folder", true) {
                    let staged = self.staged.clone();
                    let root = PathBuf::from(&self.path);
                    if let Some(staged) = staged {
                        match pc_patch::apply(&staged, &root) {
                            Ok(dest) => {
                                self.applied_to = Some(dest);
                                self.error = None;
                            }
                            Err(e) => {
                                self.needs_elevation = e.needs_elevation();
                                self.error = Some(e.to_string());
                            }
                        }
                    }
                }
                if let Some(e) = &self.error {
                    ui.add_space(theme::UNIT * 0.5);
                    widgets::status(ui, Status::Err, e);
                    if self.needs_elevation && crate::flows::elevated::Elevated::available() {
                        ui.add_space(theme::UNIT * 0.5);
                        widgets::status(
                            ui,
                            Status::Warn,
                            "That folder needs administrator rights. Windows will ask.",
                        );
                        ui.add_space(theme::UNIT * 0.5);
                        if widgets::primary(ui, "Run as administrator", true) {
                            self.start_elevated_apply();
                        }
                    }
                    ui.label(
                        RichText::new(
                            "The download is kept, so fixing the path and pressing this again \
                             costs nothing.",
                        )
                        .font(theme::font_ui(10.5))
                        .color(theme::TEXT_FAINT),
                    );
                }
            }
            _ => {
                widgets::status(ui, Status::Ok, "Patch applied");
            }
        }
    }

    fn step_done(&mut self, ui: &mut Ui) {
        widgets::status(ui, Status::Ok, "Licence patch applied");
        ui.add_space(theme::UNIT);
        if let Some(dest) = &self.applied_to {
            widgets::card(ui, |ui| {
                widgets::kv(ui, "Placed at ", &dest.display().to_string());
            });
        }
        ui.add_space(theme::UNIT * 1.5);
        widgets::status(ui, Status::Info, "SteamVR headset? Run Revive setup next.");
    }
}
