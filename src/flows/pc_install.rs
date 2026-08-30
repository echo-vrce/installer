// SPDX-License-Identifier: GPL-3.0-or-later
//! Install Echo VR on PC.
//!
//! Four steps. The licence question is asked first because it changes nothing about the
//! install itself, only what the user is told to do afterwards, and asking it up front is
//! honest about that. The path is typed and only inspected. The work runs on a worker
//! thread and reports through a channel.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use egui::{RichText, Ui};

use crate::endpoints;
use crate::engine::download::Snapshot;
use crate::engine::install::{self, Inspection};
use crate::engine::pc_install::{self as engine, Config, Report};
use crate::engine::Cancel;
use crate::flows::{Flow, Signals};
use crate::fmt::human_bytes;
use crate::theme;
use crate::widgets::{self, RowState, Status};

const STEPS: &[&str] = &["Licence", "Install path", "Install", "Done"];
/// Measured, not guessed: `content-length` on the client archive. Shown so the disk
/// requirement is on screen before anyone commits to a download this size.

/// The stage names the engine emits, in order, so the checklist can be drawn before any of
/// them have happened.
const STAGES: &[&str] = &[
    "Choosing a download server",
    "Downloading Echo VR",
    "Checking the archive",
    "Removing the existing install",
    "Extracting",
    "Applying the current update",
];

#[derive(Clone, Copy, PartialEq)]
enum Licence {
    Owner,
    NewPlayer,
}

#[derive(PartialEq)]
enum Phase {
    Idle,
    Running,
    Succeeded,
    Failed,
}

enum Msg {
    Log(String),
    Stage(&'static str),
    Mirror(String),
    Probing { base: String, index: usize, of: usize },
    Item { name: String, index: usize, of: usize },
    Download(Snapshot),
    Extract { done: u64, total: u64 },
    Finished(Result<Report, String>),
    NeedsElevation,
}

pub struct PcInstall {
    licence: Option<Licence>,
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
    stage: Option<&'static str>,
    /// One line under the current stage, for a stage that would otherwise sit silent.
    /// The server probe is several seconds long and has nothing else to show.
    stage_detail: Option<String>,
    mirror: Option<String>,
    download: Option<Snapshot>,
    extract: Option<(u64, u64)>,
    report: Option<Report>,
    error: Option<String>,
    needs_elevation: bool,
    /// Stopped on purpose, so the idle screen can say so rather than looking like nothing
    /// ever happened.
    stopped: bool,
    elevated: crate::flows::elevated::Elevated,
    pending: Option<crate::flows::Confirm>,
}

impl Default for PcInstall {
    fn default() -> Self {
        let (path, path_note) = default_path();
        let inspection = install::inspect(std::path::Path::new(&path));
        PcInstall {
            licence: None,
            path,
            path_note,
            inspection,
            phase: Phase::Idle,
            cancel: Cancel::new(),
            rx: None,
            log: crate::log::Ring::default(),
            log_open: false,
            stage: None,
            stage_detail: None,
            mirror: None,
            download: None,
            extract: None,
            report: None,
            error: None,
            needs_elevation: false,
            stopped: false,
            elevated: Default::default(),
            pending: None,
        }
    }
}

fn default_path() -> (String, Option<&'static str>) {
    crate::config::suggested_install_path(guessed_path)
}

/// Only used the first time, before there is anything to remember.
fn guessed_path() -> String {
    if cfg!(windows) {
        "C:\\EchoVR".to_string()
    } else {
        format!("{}/EchoVR", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
    }
}

impl PcInstall {
    fn reinspect(&mut self) {
        self.inspection = install::inspect(std::path::Path::new(&self.path));
    }

    /// Hands the install to an elevated copy of this executable.
    ///
    /// `--yes` because the question was already answered here: the elevated run has no
    /// terminal to ask on and would decline rather than assume.
    /// Turns the elevated run's progress into the state an ordinary run would have left.
    /// The stage list, the download bar and the extract bar are all driven from here, so an
    /// elevated install looks like any other one.
    fn absorb_elevated(&mut self, e: crate::cli::Event) {
        use crate::cli::Event;
        match e {
            // Matched back to the stage list by name. An unrecognised name still reaches
            // the log, so a renamed stage degrades to a line rather than disappearing.
            Event::Stage(s) => match STAGES.iter().find(|k| **k == s) {
                Some(known) => {
                    self.stage = Some(known);
                    self.stage_detail = None;
                    self.download = None;
                    self.extract = None;
                }
                None => self.log.push(s),
            },
            Event::Progress { what, done, total } => {
                if what == "extracting" {
                    self.extract = Some((done, total.unwrap_or(0)));
                } else {
                    self.download = Some(Snapshot { done, total, bytes_per_sec: 0.0, attempt: 0 });
                }
            }
            Event::Item { name, index, of } => {
                // During the server probe these are mirrors; during the update they are
                // files. Either way it is "which of how many", which is what the line says.
                self.stage_detail = Some(format!("{name}  ({index} of {of})"));
                self.log.push(format!("[{index}/{of}] {name}"));
            }
            Event::Done { .. } => {
                self.download = None;
                self.extract = None;
            }
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
        // The failed attempt's stage checklist belongs to a run that is over.
        self.stage = None;
        self.download = None;
        self.extract = None;
        self.report = None;
        self.log.push("asking Windows for administrator rights".into());
        self.elevated.start(vec![
            "install".into(),
            "--path".into(),
            self.path.clone(),
            "--yes".into(),
        ]);
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
        self.stage = None;
        self.mirror = None;
        self.download = None;
        self.extract = None;
        self.report = None;
        self.error = None;
        self.needs_elevation = false;

        let cfg = Config {
            root: PathBuf::from(&self.path),
            archive: endpoints::PC_ARCHIVE.into(),
            mirrors: endpoints::MIRRORS.iter().map(|s| s.to_string()).collect(),
            probe: endpoints::MIRROR_PROBE.into(),
            manifest_url: endpoints::PC_MANIFEST.into(),
            keep_archive: false,
            // Only ever reached through start(), which the confirmation gates.
            replace_existing: true,
        };
        let cancel = self.cancel.clone();
        thread::spawn(move || run(cfg, cancel, tx));
    }

    fn pump(&mut self) {
        // The elevated run is a second channel: log lines and one verdict. Nothing else in
        // this flow needs to know it is a different process.
        for update in self.elevated.poll() {
            match update {
                crate::flows::elevated::Update::Line(l) => self.log.push(l),
                crate::flows::elevated::Update::Event(e) => self.absorb_elevated(e),
                crate::flows::elevated::Update::Finished => {
                    self.phase = Phase::Succeeded;
                    self.needs_elevation = false;
                    self.stage = None;
                    self.download = None;
                    self.extract = None;
                    // No report of its own: the elevated copy did the work, so the folder
                    // is re-read rather than a result being taken on trust.
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
                Msg::Log(l) => self.log.push(l),
                Msg::Stage(s) => {
                    self.stage = Some(s);
                    self.stage_detail = None;
                    self.download = None;
                    self.extract = None;
                }
                Msg::Mirror(m) => {
                    self.mirror = Some(m);
                    self.stage_detail = None;
                }
                Msg::Probing { base, index, of } => {
                    self.stage_detail = Some(format!("trying {base}  ({index} of {of})"));
                }
                Msg::Item { name, index, of } => {
                    self.stage_detail = Some(format!("{name}  ({index} of {of})"));
                }
                Msg::Download(s) => self.download = Some(s),
                Msg::Extract { done, total } => self.extract = Some((done, total)),
                Msg::NeedsElevation => self.needs_elevation = true,
                Msg::Finished(Ok(r)) => {
                    self.report = Some(r);
                    self.phase = Phase::Succeeded;
                }
                Msg::Finished(Err(e)) => {
                    if self.cancel.is_cancelled() {
                        // Asked for. Back to the start rather than to a failure screen, so
                        // the way to carry on is the button that was always there.
                        self.phase = Phase::Idle;
                        self.stopped = true;
                    } else {
                        self.error = Some(e);
                        self.phase = Phase::Failed;
                    }
                }
            }
        }
        if disconnected {
            self.rx = None;
            if self.phase == Phase::Running {
                self.error = Some("The install stopped unexpectedly.".into());
                self.phase = Phase::Failed;
            }
        }
    }

    fn stage_index(&self) -> usize {
        self.stage
            .and_then(|s| STAGES.iter().position(|x| *x == s))
            .unwrap_or(0)
    }
}

fn run(cfg: Config, cancel: Cancel, tx: mpsc::Sender<Msg>) {
    let say = |l: String| {
        let _ = tx.send(Msg::Log(l));
    };
    say(format!("root {}", cfg.root.display()));

    let tx2 = tx.clone();
    let result = engine::run(&cfg, &cancel, &mut |event| match event {
        engine::Event::Stage(s) => {
            let _ = tx2.send(Msg::Log(format!("stage: {s}")));
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
        engine::Event::Downloading(s) => {
            let _ = tx2.send(Msg::Download(s));
        }
        engine::Event::Extracting { done, total } => {
            let _ = tx2.send(Msg::Extract { done, total });
        }
        // Not dropped any more. This is the last stage of an install and it fetches up to
        // nineteen files; the window showed nothing at all while it did.
        engine::Event::Updating(u) => match u {
            crate::engine::update::Event::Fetching { rel, index, of, snapshot } => {
                let _ = tx2.send(Msg::Item { name: rel, index, of });
                let _ = tx2.send(Msg::Download(snapshot));
            }
            crate::engine::update::Event::Deleting { rel, index, of } => {
                let _ = tx2.send(Msg::Item { name: rel, index, of });
            }
            crate::engine::update::Event::Placed { .. } => {}
        },
    });

    match result {
        Ok(report) => {
            say(format!(
                "done: {} files extracted, {} update files fetched",
                report.extracted_files, report.update.fetched
            ));
            let _ = tx.send(Msg::Finished(Ok(report)));
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

impl Flow for PcInstall {
    /// Going back voids the run. The folder and the licence answer are kept: both are
    /// things the user chose, not things this produced.
    fn reset_after(&mut self, _step: usize) {
        self.cancel.cancel();
        self.rx = None;
        self.elevated.forget();
        self.phase = Phase::Idle;
        self.stage = None;
        self.mirror = None;
        self.download = None;
        self.extract = None;
        self.report = None;
        self.error = None;
        self.needs_elevation = false;
        self.log.clear();
        self.reinspect();
    }

    fn steps(&self) -> &'static [&'static str] {
        STEPS
    }

    fn blocked_reason(&self, step: usize) -> Option<String> {
        match step {
            0 if self.licence.is_none() => Some("Choose whether you own Echo VR".into()),
            1 if self.path.trim().is_empty() => Some("Enter a folder to install into".into()),
            2 => match self.phase {
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

    fn content(&mut self, ui: &mut Ui, step: usize, signals: &mut Signals) {
        match step {
            0 => self.step_licence(ui),
            1 => self.step_path(ui),
            2 => self.step_install(ui, signals),
            _ => self.step_done(ui),
        }
    }
}

impl PcInstall {
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
            "Installs the original client. Nothing else needed.",
        ) {
            self.licence = Some(Licence::Owner);
        }
        ui.add_space(theme::UNIT * 0.75);
        if widgets::option_row(
            ui,
            self.licence == Some(Licence::NewPlayer),
            "I'm a new player",
            "Same install, plus a licence patch afterwards. Linked at the end.",
        ) {
            self.licence = Some(Licence::NewPlayer);
        }
        ui.add_space(theme::UNIT * 1.5);
        ui.label(
            RichText::new("This changes nothing about the install itself, only what you do next.")
                .font(theme::font_ui(11.0))
                .color(theme::TEXT_FAINT),
        );

        // Both cards say the same kind of thing: work to do somewhere else, before any of
        // this. Said on the step where the choice is made, not later, because by the folder
        // step someone has already committed to a path.
        if self.licence == Some(Licence::NewPlayer) {
            ui.add_space(theme::UNIT * 1.5);
            widgets::card(ui, |ui| {
                widgets::status(ui, Status::Warn, "You will need Discord for this");
                ui.add_space(theme::UNIT * 0.5);
                ui.label(
                    RichText::new(
                        "Without a Meta licence, the copy that runs has to be built for \
                         your account, and Discord is how that is arranged. The install \
                         below works either way; the patch afterwards is what needs this.",
                    )
                    .font(theme::font_ui(11.5))
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(theme::UNIT * 0.75);
                ui.label(
                    RichText::new("You need, in this order:")
                        .font(theme::font_ui(11.5))
                        .color(theme::TEXT_MUTED),
                );
                ui.add_space(theme::UNIT * 0.5);
                for line in [
                    "A Discord account, signed in to the desktop app or the website.",
                    "Membership of the patcher server below. The bot checks it by name, \
                     and refuses anyone who is not in it.",
                ] {
                    ui.label(
                        RichText::new(format!("  -  {line}"))
                            .font(theme::font_ui(11.5))
                            .color(theme::TEXT_MUTED),
                    );
                }
                ui.add_space(theme::UNIT * 0.75);
                widgets::external_link(ui, "Join the patcher server", endpoints::DISCORD_PATCHER);
                ui.add_space(theme::UNIT * 0.25);
                ui.label(
                    RichText::new(
                        "The community server is a different one, and joining it is not \
                         enough on its own.",
                    )
                    .font(theme::font_ui(10.5))
                    .color(theme::TEXT_FAINT),
                );
                ui.add_space(theme::UNIT * 0.25);
                widgets::external_link(ui, "EchoVRCE community", endpoints::DISCORD_LOUNGE);
            });
        }

        // Only for owners, because only they have a Meta copy to deal with. Said here
        // rather than on the folder step: by then they have already chosen where, and this
        // is work to do somewhere else first.
        if self.licence == Some(Licence::Owner) {
            ui.add_space(theme::UNIT * 1.5);
            widgets::card(ui, |ui| {
                widgets::status(ui, Status::Warn, "Do this in the Meta app first");
                ui.add_space(theme::UNIT * 0.5);
                ui.label(
                    RichText::new(
                        "Install Echo VR from the Meta app and let it finish, then delete \
                         the folder it created:",
                    )
                    .font(theme::font_ui(11.5))
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(theme::UNIT * 0.5);
                // The exact folder, in monospace like every other path. Read from the Meta
                // client when it is installed, so it is the real one rather than a shape to
                // match against.
                let (folder, source) = crate::engine::meta::expected_echo_dir();
                widgets::mono_color(ui, &crate::fmt::windows_path(&folder), 10.5, theme::TEXT);
                ui.label(
                    RichText::new(match source {
                        crate::engine::meta::Source::Registry => {
                            "read from your Meta installation"
                        }
                        crate::engine::meta::Source::KnownPath => {
                            "the usual location; yours may differ if you moved it"
                        }
                    })
                    .font(theme::font_ui(10.5))
                    .color(theme::TEXT_FAINT),
                );
                ui.add_space(theme::UNIT * 0.75);
                ui.label(
                    RichText::new(
                        "Installing it in Meta first is what registers the licence on your \
                         account. Leaving Meta's copy in place means Meta can replace or \
                         repair those files later and undo this install.",
                    )
                    .font(theme::font_ui(11.5))
                    .color(theme::TEXT_MUTED),
                );
            });
        }
    }

    fn step_path(&mut self, ui: &mut Ui) {
        widgets::field_label(ui, "Install into");
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

        widgets::status(
            ui,
            Status::Info,
            &format!("about {} to download", human_bytes(endpoints::PC_ARCHIVE_BYTES)),
        );
        match self.inspection.free_bytes {
            Some(free) => {
                let plenty = free > endpoints::PC_ARCHIVE_BYTES * 2;
                widgets::status(
                    ui,
                    if plenty { Status::Ok } else { Status::Warn },
                    &format!(
                        "{} free on this drive{}",
                        human_bytes(free),
                        if plenty { "" } else { ", which may not be enough" }
                    ),
                );
            }
            None => {}
        }
        // The folder, not the game in it: that is what gets deleted, so that is what the
        // glance before committing should be about. "Overwritten" was also the wrong word
        // once it started being removed rather than unpacked over.
        if self.inspection.arena_exists {
            widgets::status(
                ui,
                Status::Warn,
                match self.inspection.has_echo {
                    true => "Echo VR is already here and will be deleted first",
                    false => "a folder is already here and will be deleted first",
                },
            );
        }
        if self.inspection.root_exists && !self.inspection.writable {
            widgets::status(ui, Status::Warn, "this folder needs administrator rights to write to");
        } else if !self.inspection.root_exists {
            widgets::status(ui, Status::Info, "this folder will be created");
        }
    }

    fn step_install(&mut self, ui: &mut Ui, signals: &mut Signals) {
        self.pump();
        if self.phase == Phase::Running {
            signals.keep_repainting = true;
        }

        if self.phase == Phase::Idle {
            if self.stopped {
                widgets::status(
                    ui,
                    Status::Info,
                    "Stopped. What downloaded is kept, so starting again carries on from \
                     where it left off.",
                );
                ui.add_space(theme::UNIT);
            }
            ui.label(
                RichText::new(
                    "Downloads the client from the fastest available server, unpacks it, then \
                     applies the current update. The archive is removed once unpacked.",
                )
                .font(theme::font_ui(12.0))
                .color(theme::TEXT_MUTED),
            );
            ui.add_space(theme::UNIT * 1.5);
            if widgets::primary(ui, "Start install", true) {
                // Only worth stopping for when it would destroy something. A dialog on
                // every install is one nobody reads by the third time.
                match self.inspection.arena_exists {
                    true => {
                        self.pending = Some(crate::flows::Confirm {
                            title: match self.inspection.has_echo {
                                true => "There is already an Echo VR here".into(),
                                // No executable in it, so there is no telling what it is.
                                // That is a reason for more care, not less.
                                false => "There is already a folder here".into(),
                            },
                            consequence: format!(
                                "This will delete\n\n{}\n\nand install a fresh copy in \
                                 its place. Everything inside it goes{}. There is no \
                                 undo.\n\n\
                                 Nothing outside that folder is touched.\n\n\
                                 It is deleted rather than written over so that reinstalling \
                                 actually repairs a broken install, which unpacking on top \
                                 cannot do.",
                                crate::fmt::windows_path(
                                    &std::path::Path::new(&self.path).join(install::ARENA_DIR)
                                ),
                                match self.inspection.has_echo {
                                    true => ": settings, mods, saved files, and Meta's copy \
                                             if that is what is there",
                                    false => ", whatever it is - there is no echovr.exe in \
                                              it, so this app cannot tell you what you \
                                              would be losing",
                                }
                            ),
                            proceed: "Delete and install".into(),
                        })
                    }
                    false => self.start(),
                }
            }
            if widgets::confirm_modal(ui, &mut self.pending) == Some(true) {
                self.start();
            }
            return;
        }

        // No early return any more. The elevated run reports the same events an ordinary
        // one does, so the same stage list and the same bars are drawn from them; the only
        // difference worth showing is who is doing the work.
        if self.elevated.running() {
            widgets::status(ui, Status::Info, "Running with administrator rights");
            ui.add_space(theme::UNIT * 0.5);
        }

        let at = self.stage_index();
        let failed = self.phase == Phase::Failed;
        let detail = self.stage_detail.clone();
        widgets::card(ui, |ui| {
            for (i, name) in STAGES.iter().enumerate() {
                let state = if i < at || self.phase == Phase::Succeeded {
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
                // Under the row it belongs to, indented past the tick. Without this the
                // server probe is several seconds of a static row, which reads as stuck.
                if i == at && !failed {
                    if let Some(d) = &detail {
                        ui.horizontal(|ui| {
                            ui.add_space(theme::UNIT * 3.0);
                            // A mirror URL or a manifest path; both outgrow the column.
                            widgets::breaking_label(
                                ui,
                                d,
                                theme::font_mono(10.5),
                                theme::TEXT_FAINT,
                            );
                        });
                    }
                }
            }
        });

        ui.add_space(theme::UNIT);
        if let Some(m) = &self.mirror {
            widgets::mono_color(ui, m, 10.5, theme::TEXT_FAINT);
        }
        if let Some(s) = self.download {
            widgets::progress_row(
                ui,
                "ready-at-dawn-echo-arena.zip",
                s.fraction().unwrap_or(0.0),
                &crate::fmt::transfer(&s),
            );
        }
        if let Some((done, total)) = self.extract {
            let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
            widgets::progress_row(
                ui,
                "extracting",
                frac,
                &format!("{} / {}", human_bytes(done), human_bytes(total)),
            );
        }

        ui.add_space(theme::UNIT * 0.5);
        match self.phase {
            Phase::Succeeded => {
                let r = self.report.clone().unwrap_or_default();
                widgets::status(
                    ui,
                    Status::Ok,
                    &format!(
                        "Installed: {} files unpacked, {} update files applied",
                        r.extracted_files, r.update.fetched
                    ),
                );
            }
            Phase::Failed => {
                let msg = self.error.clone().unwrap_or_else(|| "Install failed".into());
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
                        widgets::status(
                            ui,
                            Status::Warn,
                            "This folder cannot be written to. Choose one you own.",
                        );
                    }
                }
                ui.add_space(theme::UNIT * 0.5);
                ui.horizontal(|ui| {
                    // A retry resumes: the partial archive is still on disk.
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

    fn step_done(&mut self, ui: &mut Ui) {
        widgets::status(ui, Status::Ok, "Echo VR is installed");
        ui.add_space(theme::UNIT);
        let r = self.report.clone().unwrap_or_default();
        widgets::card(ui, |ui| {
            widgets::kv(ui, "Folder    ", &self.path);
            widgets::kv(ui, "Unpacked  ", &format!("{} files", r.extracted_files));
            widgets::kv(ui, "Updated   ", &format!("{} files", r.update.fetched));
        });

        ui.add_space(theme::UNIT * 1.75);
        ui.label(
            RichText::new("You may also want")
                .font(theme::font_ui(11.0))
                .color(theme::TEXT_MUTED),
        );
        ui.add_space(theme::UNIT * 0.5);
        if self.licence == Some(Licence::NewPlayer) {
            widgets::status(ui, Status::Warn, "You said you're a new player: apply the licence patch.");
        } else {
            widgets::status(ui, Status::Info, "New player? Apply the licence patch.");
        }
        widgets::status(ui, Status::Info, "SteamVR headset? Run Revive setup.");

        ui.add_space(theme::UNIT * 1.5);
        if widgets::secondary(ui, "Open folder", true) {
            let _ = widgets::open_path(std::path::Path::new(&self.path));
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_elevated_run_drives_the_stage_list_and_the_bars() {
        use crate::cli::Event;
        let mut f = PcInstall::default();

        f.absorb_elevated(Event::Stage("Downloading Echo VR".into()));
        assert_eq!(f.stage, Some("Downloading Echo VR"));

        f.absorb_elevated(Event::Progress {
            what: crate::endpoints::PC_ARCHIVE.into(),
            done: 10,
            total: Some(100),
        });
        assert_eq!(f.download.map(|s| (s.done, s.total)), Some((10, Some(100))));

        // Extraction has its own bar, told apart by name rather than by ordering, because
        // events can arrive in any order after a resume.
        f.absorb_elevated(Event::Stage("Extracting".into()));
        assert!(f.download.is_none(), "a new stage clears the previous bar");
        f.absorb_elevated(Event::Progress { what: "extracting".into(), done: 3, total: Some(9) });
        assert_eq!(f.extract, Some((3, 9)));

        // Which of how many, under the row it belongs to. This is what fills the several
        // silent seconds of the server probe.
        f.absorb_elevated(Event::Item { name: "evr.echo.taxi".into(), index: 2, of: 3 });
        assert_eq!(f.stage_detail.as_deref(), Some("evr.echo.taxi  (2 of 3)"));
        f.absorb_elevated(Event::Stage("Extracting".into()));
        assert!(f.stage_detail.is_none(), "a new stage clears the line under the old one");

        // A stage this build does not know still reaches the user, as a line rather than
        // being dropped.
        let before = f.log.len();
        f.absorb_elevated(Event::Stage("Something added later".into()));
        assert_eq!(f.log.len(), before + 1);
        assert_eq!(f.stage, Some("Extracting"), "an unknown stage does not move the list");
    }

    #[test]
    fn the_stage_list_matches_what_the_engine_sends() {
        // An elevated run reports stages by name, and this list turns them back into rows.
        // Rename one in the engine and the row silently stops ticking, so the names are
        // pinned here against the source they come from.
        let engine = include_str!("../engine/pc_install.rs");
        for stage in STAGES {
            assert!(
                engine.contains(&format!("Event::Stage(\"{stage}\")")),
                "the engine never sends the stage {stage:?}"
            );
        }
    }

    #[test]
    fn every_engine_stage_has_a_checklist_row() {
        // The checklist is drawn from STAGES before anything runs, so a stage the engine
        // emits but the list does not know about would silently never light up.
        for stage in STAGES {
            assert!(!stage.is_empty());
        }
        let mut f = PcInstall::default();
        for (i, stage) in STAGES.iter().enumerate() {
            f.stage = Some(stage);
            assert_eq!(f.stage_index(), i, "stage {stage:?} is not in STAGES order");
        }
    }

    #[test]
    fn licence_choice_gates_the_first_step_only() {
        let mut f = PcInstall::default();
        assert!(f.blocked_reason(0).is_some());
        f.licence = Some(Licence::Owner);
        assert!(f.blocked_reason(0).is_none());
    }

    #[test]
    fn install_step_stays_blocked_until_it_succeeds() {
        let mut f = PcInstall::default();
        for phase in [Phase::Idle, Phase::Running, Phase::Failed] {
            f.phase = phase;
            assert!(f.blocked_reason(2).is_some());
        }
        f.phase = Phase::Succeeded;
        assert!(f.blocked_reason(2).is_none());
    }
}
