// SPDX-License-Identifier: GPL-3.0-or-later
//! Window shell and screen dispatch.
//!
//! The shell owns navigation: the step column, the nav bar, and what Back and Continue
//! mean. A flow owns only its steps' content and when the user may move on. That split is
//! what keeps every flow's chrome identical, and it is why adding a flow is writing one
//! file rather than another wizard.

use egui::{vec2, Align, Layout, RichText, Sense, Ui};

use crate::dependencies::Dependencies;
use crate::tools_screen::Tools;
use crate::endpoints;
use crate::flows::{
    pc_install::PcInstall, pc_patch::PcPatch, pc_update::PcUpdate, quest_install::QuestInstall,
    quest_update::QuestUpdate, revive::Revive, Flow, Signals,
};
use crate::{icons, logo, theme, widgets};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Home,
    Flow,
    About,
    /// A settings panel, not a wizard: it has no sequence to walk.
    Dependencies,
    Tools,
}

/// The flows a home row can start. Every row has one now; the `Option` and the greyed
/// rendering it drives are kept for the next flow that gets stubbed before it is built.
#[derive(Clone, Copy, PartialEq)]
enum Task {
    InstallPc,
    UpdatePc,
    InstallQuest,
    PatchPc,
    Revive,
    UpdateQuest,
    /// Not a flow: opens the settings panel instead.
    Deps,
    Tools,
}

impl Task {
    /// A stable name for the log. Not the flow heading: that changes per step, which would
    /// make one task look like several different ones.
    fn name(self) -> &'static str {
        match self {
            Task::InstallPc => "Install Echo VR (PC)",
            Task::UpdatePc => "Update Echo VR (PC)",
            Task::InstallQuest => "Install Echo VR (Quest)",
            Task::UpdateQuest => "Update Echo VR (Quest)",
            Task::PatchPc => "Licence patch (PC)",
            Task::Revive => "Revive setup",
            Task::Deps => "Dependencies",
            Task::Tools => "Tools",
        }
    }

    fn build(self) -> Option<Box<dyn Flow>> {
        match self {
            Task::InstallPc => Some(Box::new(PcInstall::default())),
            Task::UpdatePc => Some(Box::new(PcUpdate::default())),
            Task::InstallQuest => Some(Box::new(QuestInstall::default())),
            Task::PatchPc => Some(Box::new(PcPatch::default())),
            Task::Revive => Some(Box::new(Revive::default())),
            Task::UpdateQuest => Some(Box::new(QuestUpdate::default())),
            Task::Deps | Task::Tools => None,
        }
    }
}

pub struct App {
    screen: Screen,
    step: usize,
    flow: Option<Box<dyn Flow>>,
    /// Built lazily: locating adb runs a process, which is not worth doing at startup for
    /// a screen most runs never open.
    deps: Option<Dependencies>,
    tools: Option<Tools>,
    /// A confirmation waiting to be answered, and the step it would advance to.
    pending: Option<(crate::flows::Confirm, usize)>,
}

impl Default for App {
    fn default() -> Self {
        App { screen: Screen::Home, step: 0, flow: None, deps: None, tools: None, pending: None }
    }
}

impl App {
    /// Debug entry point so a screen can be rendered without clicking to it. Not reachable
    /// from the UI.
    pub fn starting_at(spec: &str) -> Self {
        let mut app = Self::default();
        match spec {
            "about" => app.screen = Screen::About,
            "deps" => app.screen = Screen::Dependencies,
            "tools" => app.screen = Screen::Tools,
            "install" => app.open(Task::InstallPc),
            "update" => app.open(Task::UpdatePc),
            "qupdate" => app.open(Task::UpdateQuest),
            "qinstall" => app.open(Task::InstallQuest),
            "patch" => app.open(Task::PatchPc),
            "revive" => app.open(Task::Revive),
            _ => {}
        }
        app
    }

    fn open(&mut self, task: Task) {
        // A marker in the log, so a file covering several attempts reads back as "they
        // tried this, then that" rather than one undivided stream.
        crate::log::line(&format!("=== {} ===", task.name()));
        match task.build() {
            Some(mut flow) => {
                flow.on_enter(0);
                self.flow = Some(flow);
                self.step = 0;
                self.screen = Screen::Flow;
            }
            None => {
                self.screen = match task {
                    Task::Tools => Screen::Tools,
                    _ => Screen::Dependencies,
                };
                // These two screens are built once and kept, so what they found on the way
                // in stays on show until something asks them to look again. Entering is
                // that moment: the other screen may well be what changed it.
                if let Some(tools) = self.tools.as_mut() {
                    tools.recheck();
                }
                if let Some(deps) = self.deps.as_mut() {
                    // The same in the other direction: Revive can be installed from a flow,
                    // and an adb chosen from the command line while the window is open.
                    deps.reenter();
                }
            }
        }
    }

    fn leave_flow(&mut self) {
        if let Some(flow) = self.flow.as_mut() {
            flow.on_exit();
        }
        self.flow = None;
        self.step = 0;
        self.screen = Screen::Home;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.screen {
            Screen::Home => self.home(ui),
            Screen::Flow => self.flow_screen(ui),
            Screen::About => self.about(ui),
            Screen::Dependencies => self.dependencies(ui),
            Screen::Tools => self.tools(ui),
        }
    }
}

// ------------------------------------------------------------------ home
impl App {
    fn home(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("home_bar")
            .exact_size(46.0)
            .frame(panel_frame(theme::SURFACE, 16.0))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let r = ui.add(
                            egui::Label::new(
                                RichText::new("About")
                                    .font(theme::font_ui(11.5))
                                    .color(theme::TEXT_DIM),
                            )
                            .sense(Sense::click()),
                        );
                        if r.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if r.clicked() {
                            self.screen = Screen::About;
                        }
                    });
                });
            });

        let mut start: Option<Task> = None;
        egui::CentralPanel::no_frame()
            .frame(panel_frame(theme::BG, 0.0))
            .show(ui, |ui| {
                scroller().show(ui, |ui| {
                    ui.add_space(theme::UNIT * 3.5);
                    capped(ui, |ui| {
                        // Same helper as the settings screens, so the mark cannot end up
                        // sitting at a different height depending on where you came from.
                        settings_header(ui, "");

                        widgets::section_label(ui, "INSTALL");
                        task_row(ui, "Install Echo VR (PC)", "Download the client and apply the current update", Some(Task::InstallPc), &mut start);
                        task_row(ui, "Install Echo VR (Quest)", "Sideload the APK and game data over adb", Some(Task::InstallQuest), &mut start);
                        ui.add_space(theme::UNIT * 0.5);

                        widgets::section_label(ui, "UPDATE");
                        task_row(ui, "Update Echo VR (PC)", "Apply the latest update to an existing install", Some(Task::UpdatePc), &mut start);
                        task_row(ui, "Update Echo VR (Quest)", "Check the installed version, then update on device", Some(Task::UpdateQuest), &mut start);
                        ui.add_space(theme::UNIT * 0.5);

                        widgets::section_label(ui, "PATCHES");
                        task_row(ui, "Licence patch (PC)", "Generate and apply pnsovr.dll", Some(Task::PatchPc), &mut start);
                        task_row(ui, "Revive setup", "SteamVR headsets: injector shortcut and app list entry", Some(Task::Revive), &mut start);
                        ui.add_space(theme::UNIT * 0.5);

                        widgets::section_label(ui, "SETUP");
                        task_row(ui, "Dependencies", "adb and Revive: detected paths, manual override, install", Some(Task::Deps), &mut start);
                        task_row(ui, "Tools", "Collect a support bundle, clear cached downloads", Some(Task::Tools), &mut start);

                        ui.add_space(theme::UNIT * 2.0);
                    });
                });
            });

        if let Some(task) = start {
            self.open(task);
        }
    }
}

/// The scroll area every screen uses.
///
/// The bar is always drawn rather than appearing when it is needed. A bar that comes and
/// goes moves the content sideways underneath the pointer, and on a screen that grows a row
/// at a time - a checklist ticking over - that happens mid-read.
fn scroller() -> egui::ScrollArea {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
}

/// One row of the home task list. A row with no task is drawn greyed and inert.
fn task_row(
    ui: &mut Ui,
    title: &str,
    subtitle: &str,
    task: Option<Task>,
    start: &mut Option<Task>,
) {
    let enabled = task.is_some();
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::click());
    let painter = ui.painter().clone();
    let hovered = enabled && resp.hovered();

    if hovered {
        painter.rect_filled(rect, egui::CornerRadius::same(theme::RADIUS), theme::SURFACE_HOVER);
    }
    let (title_col, sub_col) = if enabled {
        (theme::TEXT, theme::TEXT_DIM)
    } else {
        (theme::TEXT_FAINT, theme::TEXT_FAINT)
    };
    painter.text(
        egui::pos2(rect.left() + 10.0, rect.top() + 6.0),
        egui::Align2::LEFT_TOP,
        title,
        theme::font_med(12.5),
        title_col,
    );
    painter.text(
        egui::pos2(rect.left() + 10.0, rect.top() + 23.0),
        egui::Align2::LEFT_TOP,
        subtitle,
        theme::font_ui(11.0),
        sub_col,
    );
    if enabled {
        let chev = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 16.0, rect.center().y),
            vec2(13.0, 13.0),
        );
        icons::chevron(
            &painter,
            chev,
            if hovered { theme::ACCENT_TEXT } else { theme::TEXT_FAINT },
            false,
        );
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            *start = task;
        }
    }
}

// ------------------------------------------------------------------ flow shell
impl App {
    fn flow_screen(&mut self, ui: &mut Ui) {
        // Taken out so the flow can be given &mut self.flow's contents while the shell
        // also reads its own fields.
        let Some(mut flow) = self.flow.take() else {
            self.screen = Screen::Home;
            return;
        };
        let step = self.step.min(flow.steps().len() - 1);
        let mut signals = Signals::default();
        let mut nav: Option<Nav> = None;

        egui::Panel::left("steps")
            .exact_size(theme::SIDEBAR_W)
            .resizable(false)
            .frame(panel_frame(theme::SURFACE, 0.0))
            .show(ui, |ui| {
                if let Some(target) = step_column(ui, flow.steps(), step) {
                    nav = Some(Nav::To(target));
                }
            });

        egui::Panel::bottom("nav")
            .exact_size(52.0)
            .frame(panel_frame(theme::SURFACE, 16.0))
            .show(ui, |ui| {
                let reason = flow.blocked_reason(step);
                let last = step + 1 == flow.steps().len();
                let note = flow.status_note();
                if let Some(n) = nav_bar(ui, step, last, reason, note) {
                    nav = Some(n);
                }
            });

        egui::CentralPanel::no_frame()
            .frame(panel_frame(theme::BG, 0.0))
            .show(ui, |ui| {
                scroller().show(ui, |ui| {
                    ui.add_space(theme::UNIT * 3.5);
                    capped(ui, |ui| {
                        widgets::step_heading(ui, step, flow.steps().len(), flow.heading(step));
                        flow.content(ui, step, &mut signals);
                        ui.add_space(theme::UNIT * 2.0);
                    });
                });
            });

        // Answered here, after the flow has drawn, so the dialog sits over the page it is
        // asking about.
        if let Some((_, target)) = self.pending.as_ref().map(|(c, t)| (c, *t)) {
            let mut pending = self.pending.take().map(|(c, _)| c);
            let answer = widgets::confirm_modal(ui, &mut pending);
            match answer {
                Some(true) => {
                    self.step = target;
                    if let Some(f) = self.flow.as_mut() {
                        f.on_enter(target);
                    }
                }
                // Declined: nothing moves and nothing is remembered.
                Some(false) => {}
                None => self.pending = pending.map(|c| (c, target)),
            }
        }

        if signals.keep_repainting {
            ui.ctx().request_repaint();
        }
        if signals.advance && step + 1 < flow.steps().len() {
            nav = Some(Nav::To(step + 1));
        }
        if signals.go_home {
            nav = Some(Nav::Home);
        }

        self.flow = Some(flow);

        // A forward move may have something to say for itself first. Asked here rather than
        // in the nav bar so every flow gets it without doing anything, and so a flow cannot
        // forget to ask.
        if let (Some(Nav::To(target)), Some(f)) = (&nav, self.flow.as_ref()) {
            if *target > self.step {
                if let Some(c) = f.confirm_advance(self.step) {
                    self.pending = Some((c, *target));
                    nav = None;
                }
            }
        }

        match nav {
            Some(Nav::Home) => self.leave_flow(),
            Some(Nav::To(target)) => {
                let going_back = target < self.step;
                self.step = target;
                if let Some(f) = self.flow.as_mut() {
                    // Backwards first, so on_enter sees a clean flow rather than having to
                    // undo what the abandoned steps left behind.
                    if going_back {
                        f.reset_after(target);
                    }
                    f.on_enter(target);
                }
            }
            None => {}
        }
    }
}

enum Nav {
    To(usize),
    Home,
}

/// The step column. Returns a step to jump to when a completed one is clicked.
fn step_column(ui: &mut Ui, steps: &[&str], current: usize) -> Option<usize> {
    ui.add_space(theme::UNIT * 2.0);
    logo::lockup(ui, logo::Lockup::Sidebar);
    ui.add_space(theme::UNIT * 1.5);
    // Hairline between identity and navigation, inset to line up with the brand.
    let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(
        (line.left() + 14.0)..=(line.right() - 12.0),
        line.center().y,
        egui::Stroke::new(1.0, theme::DIVIDER),
    );
    ui.add_space(theme::UNIT * 1.75);

    let mut jump = None;
    for (i, name) in steps.iter().enumerate() {
        let (rect, resp) =
            ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
        let painter = ui.painter().clone();
        let done = i < current;
        let is_current = i == current;

        if is_current {
            let mut bar = rect;
            bar.set_width(3.0);
            painter.rect_filled(bar, egui::CornerRadius::ZERO, theme::ACCENT);
        }
        let icon = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 26.0, rect.center().y),
            vec2(12.0, 12.0),
        );
        // Completed steps get a grey check, not a green one: green is reserved for
        // validation results, so the column does not turn into a christmas tree.
        if done {
            icons::check(&painter, icon, theme::TEXT_DIM);
        } else if is_current {
            icons::dot_filled(&painter, icon, theme::ACCENT_HOVER);
        } else {
            icons::dot_hollow(&painter, icon, theme::TEXT_FAINT);
        }

        let (colour, font) = if is_current {
            (theme::ACCENT_TEXT, theme::font_med(12.5))
        } else if done {
            (theme::TEXT_DIM, theme::font_ui(12.5))
        } else {
            (theme::TEXT_FAINT, theme::font_ui(12.5))
        };
        painter.text(
            egui::pos2(rect.left() + 42.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            *name,
            font,
            colour,
        );

        // Clicking a completed step goes back. Pending steps are inert: Continue is the
        // only way forward, so nothing can skip a step's input.
        if done {
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                jump = Some(i);
            }
        }
    }
    jump
}

fn nav_bar(
    ui: &mut Ui,
    step: usize,
    last: bool,
    reason: Option<String>,
    note: Option<(bool, String)>,
) -> Option<Nav> {
    let mut nav = None;
    ui.horizontal_centered(|ui| {
        // Left: whatever the flow says it depends on. Always present, so the absence of a
        // tool is visible before the step that needs it.
        if let Some((ok, text)) = note {
            let (rect, _) = ui.allocate_exact_size(vec2(13.0, 15.0), Sense::hover());
            let painter = ui.painter().clone();
            let icon = egui::Rect::from_center_size(rect.center(), vec2(13.0, 13.0));
            if ok {
                icons::check(&painter, icon, theme::SUCCESS);
            } else {
                icons::warning(&painter, icon, theme::WARNING);
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new(text)
                    .font(theme::font_ui(11.0))
                    .color(if ok { theme::TEXT_DIM } else { theme::TEXT_MUTED }),
            );
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = if last { "Finish" } else { "Continue" };
            if widgets::primary(ui, label, reason.is_none()) {
                nav = Some(if last { Nav::Home } else { Nav::To(step + 1) });
            }
            ui.add_space(2.0);
            // Back on the first step leaves the flow, which is what a Back button in that
            // position is expected to do.
            if widgets::secondary(ui, "Back", true) {
                nav = Some(if step == 0 { Nav::Home } else { Nav::To(step - 1) });
            }
            if let Some(r) = reason {
                ui.add_space(theme::UNIT);
                ui.label(RichText::new(r).font(theme::font_ui(11.0)).color(theme::WARNING));
            }
        });
    });
    nav
}

// ------------------------------------------------------------------ dependencies
impl App {
    fn dependencies(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("deps_bar")
            .exact_size(52.0)
            .frame(panel_frame(theme::SURFACE, 16.0))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::primary(ui, "Close", true) {
                            self.screen = Screen::Home;
                        }
                    });
                });
            });

        let deps = self.deps.get_or_insert_with(Dependencies::default);
        let mut busy = false;
        egui::CentralPanel::no_frame()
            .frame(panel_frame(theme::BG, 0.0))
            .show(ui, |ui| {
                scroller().show(ui, |ui| {
                    ui.add_space(theme::UNIT * 3.5);
                    capped(ui, |ui| {
                        settings_header(ui, "Dependencies");
                        busy = deps.show(ui);
                        ui.add_space(theme::UNIT * 2.0);
                    });
                });
            });
        if busy {
            // The device list polls on a timer, which only advances if frames keep coming.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}

/// Top of a settings screen: the mark, then which screen this is.
///
/// The wizards carry the mark in their step column. These have no step column, so without
/// this they were the only screens that did not say what application you were looking at.
fn settings_header(ui: &mut Ui, title: &str) {
    logo::lockup(ui, logo::Lockup::Header);
    ui.add_space(theme::UNIT * 2.0);
    // Home passes no title: there the mark is the title, and a second one under it would
    // only repeat the wordmark. Every screen still gets the same mark in the same place.
    if !title.is_empty() {
        ui.label(RichText::new(title).font(theme::font_med(19.0)).color(theme::TEXT));
        ui.add_space(theme::UNIT * 1.5);
    }
}

// ------------------------------------------------------------------ tools
impl App {
    fn tools(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("tools_bar")
            .exact_size(52.0)
            .frame(panel_frame(theme::SURFACE, 16.0))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::primary(ui, "Close", true) {
                            self.screen = Screen::Home;
                        }
                    });
                });
            });

        let tools = self.tools.get_or_insert_with(Tools::default);
        let mut busy = false;
        egui::CentralPanel::no_frame()
            .frame(panel_frame(theme::BG, 0.0))
            .show(ui, |ui| {
                scroller().show(ui, |ui| {
                    ui.add_space(theme::UNIT * 3.5);
                    capped(ui, |ui| {
                        settings_header(ui, "Tools");
                        busy = tools.show(ui);
                        ui.add_space(theme::UNIT * 2.0);
                    });
                });
            });
        if busy {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}

// ------------------------------------------------------------------ about
impl App {
    fn about(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("about_bar")
            .exact_size(52.0)
            .frame(panel_frame(theme::SURFACE, 16.0))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::primary(ui, "Close", true) {
                            self.screen = Screen::Home;
                        }
                    });
                });
            });

        egui::CentralPanel::no_frame()
            .frame(panel_frame(theme::BG, 0.0))
            .show(ui, |ui| {
                scroller().show(ui, |ui| {
                    ui.add_space(theme::UNIT * 3.5);
                    capped(ui, |ui| {
                        logo::lockup(ui, logo::Lockup::Hero);
                        ui.add_space(theme::UNIT * 2.5);

                        // The disclaimer goes first: it is the part with legal weight.
                        ui.label(
                            RichText::new(
                                "Echo VR is copyright Meta / Ready At Dawn. This installer is not \
                                 associated with or endorsed by them.",
                            )
                            .font(theme::font_ui(12.0))
                            .color(theme::TEXT_MUTED),
                        );
                        ui.add_space(theme::UNIT * 2.0);

                        widgets::section_label(ui, "CREDITS");
                        ui.label(
                            RichText::new(
                                "marshmallow-mia: author of the original Echo VR Installer, and of \
                                 the server and Discord bot this build still depends on.",
                            )
                            .font(theme::font_ui(12.0))
                            .color(theme::TEXT),
                        );
                        ui.add_space(theme::UNIT * 0.5);
                        widgets::external_link(
                            ui,
                            "The original installer on GitHub",
                            endpoints::REPO_ORIGINAL,
                        );
                        ui.add_space(theme::UNIT * 0.75);
                        ui.label(
                            RichText::new("The EchoVRCE community, who keep the game alive.")
                                .font(theme::font_ui(12.0))
                                .color(theme::TEXT_MUTED),
                        );
                        ui.add_space(theme::UNIT * 0.75);
                        widgets::external_link(ui, "Echo VR Lounge on Discord", endpoints::DISCORD_LOUNGE);
                        ui.add_space(theme::UNIT * 2.0);

                        widgets::section_label(ui, "THIS BUILD");
                        widgets::kv(ui, "Version  ", VERSION);
                        widgets::kv(ui, "Licence  ", "GPL-3.0-or-later");
                        ui.add_space(theme::UNIT * 0.75);
                        // GPL-3.0 section 5(d): an interactive program has to show its
                        // legal notices where the user can see them. Copyright, the absence
                        // of a warranty, and the right to redistribute - the three things
                        // the licence asks for, in the place people look for them.
                        ui.label(
                            RichText::new(
                                "Copyright (C) 2026 kekt8c. This \
                                 program comes with absolutely no warranty. It is free \
                                 software, and you are welcome to redistribute it under the \
                                 terms of the GNU General Public License, version 3 or any \
                                 later version.",
                            )
                            .font(theme::font_ui(11.5))
                            .color(theme::TEXT_DIM),
                        );
                        ui.add_space(theme::UNIT * 0.5);
                        widgets::external_link(ui, "Read the licence", endpoints::LICENCE);
                        ui.add_space(theme::UNIT * 2.0);

                        widgets::section_label(ui, "THIRD PARTY");
                        ui.label(
                            RichText::new(
                                "Android platform-tools (adb), Apache License 2.0. Revive is \
                                 downloaded from its own releases, not bundled. Rust crates per \
                                 their respective licences.",
                            )
                            .font(theme::font_ui(11.5))
                            .color(theme::TEXT_DIM),
                        );
                        ui.add_space(theme::UNIT * 2.0);
                    });
                });
            });
    }
}

// ------------------------------------------------------------------ helpers
fn panel_frame(fill: egui::Color32, margin: f32) -> egui::Frame {
    egui::Frame::new().fill(fill).inner_margin(egui::Margin::same(margin as i8))
}

/// Caps the content column and centres it, so a wide window grows the margins rather than
/// stretching a line of text across 900 px. Text stays left-aligned *inside* the column;
/// it is the column that is centred, not the content.
fn capped<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let avail = ui.available_width();
    let w = avail.min(theme::CONTENT_MAX_W);
    let indent = ((avail - w) * 0.5).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(indent);
        ui.allocate_ui_with_layout(
            vec2(w, ui.available_height()),
            Layout::top_down(Align::Min),
            add,
        )
        .inner
    })
    .inner
}
