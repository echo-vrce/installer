// SPDX-License-Identifier: GPL-3.0-or-later
//! The Tools screen: collecting a support bundle, and clearing what was cached.
//!
//! A settings screen rather than a wizard, for the same reason as Dependencies: neither of
//! these is a sequence. They are two independent things someone comes here to do once.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use egui::{Align, Layout, RichText, Ui};

use crate::config::{self, Settings};
use crate::engine::adb::{self, Adb};
use crate::engine::quest::Quest;
use crate::engine::tools::{self, Bundle, CacheReport};
use crate::engine::watch::DeviceWatcher;
use crate::fmt::human_bytes;
use crate::theme;
use crate::widgets::{self, Status};

enum Msg {
    Step(String),
    Done(Result<Bundle, String>),
}

pub struct Tools {
    settings: Settings,
    adb: Option<adb::Located>,
    watcher: Option<DeviceWatcher>,

    collecting: Option<Receiver<Msg>>,
    step: Option<String>,
    bundle: Option<Bundle>,
    collect_error: Option<String>,

    cache: CacheReport,
    cleared: Option<u64>,
    clear_error: Option<String>,

    installer: crate::update_notice::Installer,
}

impl Default for Tools {
    fn default() -> Self {
        let settings = Settings::load();
        let adb = adb::locate(settings.adb_path.as_deref());
        let watcher = adb.as_ref().map(|f| DeviceWatcher::start(f.path.clone()));
        let mut tools = Tools {
            settings,
            adb,
            watcher,
            collecting: None,
            step: None,
            bundle: None,
            collect_error: None,
            cache: CacheReport::default(),
            cleared: None,
            clear_error: None,
            installer: Default::default(),
        };
        tools.cache = tools.report();
        tools
    }
}

impl Tools {
    /// Runs the adb search again, for when something outside this screen has changed it.
    ///
    /// The screen keeps the adb it found when it was first opened, because locating one
    /// runs the binary and that is not a thing to do every frame. The cost of that was
    /// visible: install adb from Dependencies, come back here, and this screen still said
    /// "adb is not set up" until the whole app was restarted. What it was showing had been
    /// true when it was built and had not been true since.
    ///
    /// The device watcher is only restarted when the path actually changes, so re-entering
    /// the screen does not drop a working connection and start hunting again.
    pub fn recheck(&mut self) {
        self.settings = Settings::load();
        let found = adb::locate(self.settings.adb_path.as_deref());
        let same = found.as_ref().map(|f| &f.path) == self.adb.as_ref().map(|f| &f.path);
        if !same {
            self.watcher = found.as_ref().map(|f| DeviceWatcher::start(f.path.clone()));
        }
        self.adb = found;
        // The cache can have changed too: an install run since this screen was last open
        // leaves staged files behind, or removes them.
        self.cache = self.report();
    }
}

fn staging() -> PathBuf {
    config::dir().join("staging")
}

/// Everywhere worth looking, including the folder the user last installed into.
fn caches_for(settings: &Settings) -> (PathBuf, Option<PathBuf>) {
    (staging(), settings.install_path.as_ref().map(PathBuf::from))
}

/// Where a collected bundle is written. Beside the app's other data, so it is somewhere
/// findable rather than wherever the process happened to be started from.
fn logs_dir() -> PathBuf {
    config::logs_dir()
}

impl Tools {
    /// Both places large files end up: staging, and the folder last installed into.
    fn report(&self) -> CacheReport {
        let (staging, root) = caches_for(&self.settings);
        tools::cache_report(&tools::caches(&staging, root.as_deref()))
    }

    fn clear(&self) -> Result<u64, tools::Error> {
        let (staging, root) = caches_for(&self.settings);
        tools::clear_cache(&tools::caches(&staging, root.as_deref()))
    }

    fn start_collect(&mut self) {
        let (Some(found), Some(watcher)) = (&self.adb, &self.watcher) else { return };
        let Some(device) = watcher.snapshot().first_ready().cloned() else { return };

        let (tx, rx) = mpsc::channel();
        self.collecting = Some(rx);
        self.bundle = None;
        self.collect_error = None;
        self.step = None;
        let path = found.path.clone();

        thread::spawn(move || {
            let adb = Adb::at(&path);
            let quest = Quest::new(&adb, Some(&device));
            let tx2 = tx.clone();
            let result = tools::collect_logs(&quest, &logs_dir(), &mut |s| {
                let _ = tx2.send(Msg::Step(s.to_string()));
            });
            let _ = tx.send(Msg::Done(result.map_err(|e| e.to_string())));
        });
    }

    fn pump(&mut self) -> bool {
        let (inbox, _) = crate::channel::drain(&self.collecting);
        for msg in inbox {
            match msg {
                Msg::Step(s) => self.step = Some(s),
                Msg::Done(Ok(b)) => {
                    self.bundle = Some(b);
                    self.collecting = None;
                    self.step = None;
                }
                Msg::Done(Err(e)) => {
                    self.collect_error = Some(e);
                    self.collecting = None;
                    self.step = None;
                }
            }
        }
        self.collecting.is_some()
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        update: &mut crate::update_notice::State,
        settings: &mut Settings,
    ) -> bool {
        let busy = self.pump();
        let updating = self.section_updates(ui, update, settings);
        ui.add_space(theme::UNIT * 2.0);
        self.section_logs(ui, busy);
        ui.add_space(theme::UNIT * 2.0);
        section_own_log(ui);
        ui.add_space(theme::UNIT * 2.0);
        self.section_cache(ui);
        busy || updating
    }

    /// The update section.
    ///
    /// Here rather than on Home because Home is the list of things you came to do, and this
    /// is maintenance of the tool itself, which is what the rest of this screen already is.
    fn section_updates(
        &mut self,
        ui: &mut Ui,
        update: &mut crate::update_notice::State,
        settings: &mut Settings,
    ) -> bool {
        use crate::engine::selfupdate;
        widgets::section_label(ui, "UPDATES");

        let running = self.installer.pump();

        if running {
            widgets::status(
                ui,
                Status::Info,
                self.installer.stage.as_deref().unwrap_or("Working"),
            );
            if let Some((done, total)) = self.installer.progress {
                ui.add_space(theme::UNIT * 0.75);
                let fraction = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                widgets::progress_row(
                    ui,
                    "",
                    fraction,
                    &format!(
                        "{} / {}",
                        crate::fmt::human_bytes(done),
                        crate::fmt::human_bytes(total)
                    ),
                );
            }
            ui.add_space(theme::UNIT * 0.75);
            if widgets::secondary(ui, "Cancel", true) {
                self.installer.cancel();
            }
            return true;
        }

        match self.installer.finished.take() {
            Some(Ok(())) => {
                widgets::status(ui, Status::Ok, "Update installed");
                ui.label(
                    RichText::new(
                        "Close this window and open it again to run the new version. The one \
                         you were running is still beside it, with .old on the name, in case \
                         you need to go back.",
                    )
                    .font(theme::font_ui(11.0))
                    .color(theme::TEXT_FAINT),
                );
                return false;
            }
            Some(Err(e)) => {
                widgets::status(ui, Status::Err, &e);
                ui.add_space(theme::UNIT * 0.5);
            }
            None => {}
        }

        let current = selfupdate::current();
        match &update.newer {
            Some(v) => {
                widgets::status(ui, Status::Info, &format!("{v} is available"));
                widgets::mono_color(
                    ui,
                    &format!("you are on {current}"),
                    10.5,
                    theme::TEXT_DIM,
                );
                ui.add_space(theme::UNIT * 0.75);
                // Checked before the button is drawn rather than after the download: inside
                // Program Files there is no way to do this, because the elevation broker
                // runs the very binary that would be replaced.
                if selfupdate::can_replace_in_place() {
                    if widgets::primary(ui, &format!("Install {v}"), true) {
                        self.installer.start();
                    }
                } else {
                    widgets::status(
                        ui,
                        Status::Warn,
                        "this folder cannot be written to, so the update has to be done by hand",
                    );
                    ui.add_space(theme::UNIT * 0.5);
                    widgets::external_link(ui, "Download it", crate::endpoints::RELEASE_LATEST);
                }
            }
            None if update.is_checking() => {
                widgets::status(ui, Status::Info, "Checking...");
            }
            None => {
                match update.days_since_check() {
                    Some(0) => widgets::status(
                        ui,
                        Status::Ok,
                        &format!("{current} is the latest, checked today"),
                    ),
                    Some(d) => widgets::status(
                        ui,
                        Status::Ok,
                        &format!("{current} was the latest {d} days ago"),
                    ),
                    None => widgets::status(ui, Status::Warn, "never checked successfully"),
                }
                // The whole truth, on the screen where somebody is trying to work out why.
                if let Some(e) = &update.last_error {
                    widgets::status(ui, Status::Err, e);
                }
                ui.add_space(theme::UNIT * 0.75);
                if widgets::secondary(ui, "Check now", true) {
                    update.begin();
                }
            }
        }

        ui.add_space(theme::UNIT);
        let mut on = settings.update_check;
        if ui.checkbox(&mut on, "Check for updates at startup").changed() {
            settings.update_check = on;
            settings.save();
        }
        ui.label(
            RichText::new(
                "One request to GitHub for a file listing the newest version. Nothing about \
                 you is sent, and nothing is installed without asking.",
            )
            .font(theme::font_ui(10.5))
            .color(theme::TEXT_FAINT),
        );
        false
    }

    fn section_logs(&mut self, ui: &mut Ui, busy: bool) {
        widgets::section_label(ui, "QUEST LOGS");
        ui.label(
            RichText::new(
                "Collects Echo's logs from the headset into one zip, along with which build \
                 is installed and how it got there. Drag it into a help channel.",
            )
            .font(theme::font_ui(11.5))
            .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::UNIT * 0.75);

        let snap = self.watcher.as_ref().map(|w| w.snapshot()).unwrap_or_default();
        let device = snap.first_ready().cloned();
        match (&self.adb, &device) {
            (None, _) => widgets::status(ui, Status::Warn, "adb is not set up"),
            (Some(_), None) => widgets::status(ui, Status::Info, "no headset connected"),
            (Some(_), Some(d)) => widgets::status(
                ui,
                Status::Ok,
                &d.model.clone().unwrap_or_else(|| d.serial.clone()),
            ),
        }

        if let Some(step) = &self.step {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(ui, Status::Info, step);
        }
        if let Some(e) = &self.collect_error {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(ui, Status::Err, e);
        }
        if let Some(b) = &self.bundle {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(
                ui,
                Status::Ok,
                &format!("{} files collected, {}", b.files, human_bytes(b.bytes)),
            );
            widgets::mono_color(ui, &b.path.display().to_string(), 10.5, theme::TEXT_DIM);
        }

        ui.add_space(theme::UNIT * 0.75);
        ui.horizontal(|ui| {
            if widgets::secondary(ui, "Collect logs", !busy && device.is_some()) {
                self.start_collect();
            }
            if self.bundle.is_some() && widgets::secondary(ui, "Open folder", true) {
                let _ = widgets::open_path(&logs_dir());
            }
        });
    }

    fn section_cache(&mut self, ui: &mut Ui) {
        widgets::section_label(ui, "CACHED DOWNLOADS");
        ui.label(
            RichText::new(
                "Partly finished downloads and staged files. Safe to remove; anything still \
                 needed is fetched again, and a part-finished download resumes.",
            )
            .font(theme::font_ui(11.5))
            .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::UNIT * 0.75);

        if self.cache.entries.is_empty() {
            widgets::status(ui, Status::Info, "nothing cached");
        } else {
            // Listed before anything is removed, so nobody has to trust a number after the
            // fact. The original tells you what it deleted once it is gone.
            widgets::card(ui, |ui| {
                for (path, size) in self.cache.entries.iter().take(8) {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    // Sizes right-aligned against the card edge so they read as a column.
                    // A label/value pair would leave them wherever each name happens to end.
                    ui.horizontal(|ui| {
                        // A cached file's name, which is as long as whoever named it liked.
                        widgets::breaking_label(ui, &name, theme::font_ui(11.0), theme::TEXT_DIM);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new(human_bytes(*size))
                                    .font(theme::font_mono(11.0))
                                    .color(theme::TEXT_MUTED),
                            );
                        });
                    });
                }
                if self.cache.entries.len() > 8 {
                    ui.label(
                        RichText::new(format!("and {} more", self.cache.entries.len() - 8))
                            .font(theme::font_ui(10.5))
                            .color(theme::TEXT_FAINT),
                    );
                }
            });
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(
                ui,
                Status::Info,
                &format!("{} in total", human_bytes(self.cache.total)),
            );
        }

        if let Some(freed) = self.cleared {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(ui, Status::Ok, &format!("{} freed", human_bytes(freed)));
        }
        if let Some(e) = &self.clear_error {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(ui, Status::Err, e);
        }

        ui.add_space(theme::UNIT * 0.75);
        ui.horizontal(|ui| {
            let has = self.cache.total > 0;
            if widgets::secondary(ui, "Clear", has) {
                match self.clear() {
                    Ok(freed) => {
                        self.cleared = Some(freed);
                        self.clear_error = None;
                    }
                    Err(e) => self.clear_error = Some(e.to_string()),
                }
                self.cache = self.report();
            }
            if widgets::secondary(ui, "Re-check", true) {
                self.cache = self.report();
                self.cleared = None;
            }
        });
    }
}

/// What this installer wrote about itself, this run.
///
/// Free function rather than a method: it owns no state, because the logger is process
/// wide and asking it where it is writing is always current.
fn section_own_log(ui: &mut Ui) {
    widgets::section_label(ui, "INSTALLER LOG");
    ui.label(
        RichText::new(
            "What this installer did, kept across restarts. The last few runs are retained; \
             older ones are removed on their own.",
        )
        .font(theme::font_ui(11.5))
        .color(theme::TEXT_DIM),
    );
    ui.add_space(theme::UNIT * 0.75);

    match crate::log::path() {
        Some(path) => {
            widgets::status(ui, Status::Ok, "writing this run");
            widgets::mono_color(ui, &path.display().to_string(), 10.5, theme::TEXT_DIM);
            ui.add_space(theme::UNIT * 0.75);
            if widgets::secondary(ui, "Open folder", true) {
                if let Some(dir) = path.parent() {
                    let _ = widgets::open_path(dir);
                }
            }
        }
        None => {
            // Logging failing is not a reason to stop working, so this is a note, not an
            // error: everything else on this screen still functions.
            widgets::status(ui, Status::Warn, "not writing a log: the folder is not writable");
        }
    }
}
