// SPDX-License-Identifier: GPL-3.0-or-later
//! Wizard flows.
//!
//! The shell in [`crate::app`] owns navigation, the step column and the nav bar; a flow
//! owns its steps' content and decides when the user may move on. That split is what keeps
//! seven flows from each reinventing the chrome, and it is why the step column behaves
//! identically everywhere.

pub mod elevated;
pub mod pc_install;
pub mod pc_patch;
pub mod pc_update;
pub mod quest_install;
pub mod revive;
pub mod quest_update;

use egui::{RichText, Ui};

use crate::engine::adb::State;
use crate::engine::watch::Snapshot;
use crate::theme;
use crate::widgets::{self, Status};

/// What a flow can ask of the shell during a frame.
#[derive(Default)]
pub struct Signals {
    /// Move to the next step. Set by a flow that has just finished its work, so the user
    /// is not made to press Continue for a step that is plainly over.
    pub advance: bool,
    /// Leave the flow.
    pub go_home: bool,
    /// Something is running: keep repainting so progress actually moves. egui otherwise
    /// repaints only on input, and a background job produces none.
    pub keep_repainting: bool,
}

/// What a step is about to do, when that is worth stopping for.
///
/// Only for the moment of committing. A warning that gates nothing stays on the page: a
/// dialog that appears every time is one people learn to dismiss without reading, which is
/// the opposite of what it is for.
pub struct Confirm {
    /// A few words naming the action, not the warning.
    pub title: String,
    /// What will actually happen, in plain sentences. This is the sentence the decision is
    /// made on, so it goes here rather than on the page behind it.
    pub consequence: String,
    /// The affirmative button. Says what it does - "Continue" tells nobody anything.
    pub proceed: String,
}

/// Adopts the install root when the typed path names some part of one.
///
/// Returns the note to show when it changed something. Visible rather than silent: this is
/// still "the folder you typed is the folder we use", it is just reading the answer instead
/// of the letters. Someone who meant that other folder can see it happened and undo it.
pub fn adopt_install_root(path: &mut String) -> Option<&'static str> {
    let typed = std::path::Path::new(path.trim());
    let root = crate::engine::install::root_of(typed)?;
    if root == typed {
        return None;
    }
    *path = root.display().to_string();
    Some("that path is inside an install; using its root")
}

pub trait Flow {
    /// Step names, in order. Drives the step column and the counter.
    fn steps(&self) -> &'static [&'static str];

    /// Heading for a step. Defaults to its name in the column.
    fn heading(&self, step: usize) -> &str {
        self.steps()[step]
    }

    fn content(&mut self, ui: &mut Ui, step: usize, signals: &mut Signals);

    /// Why Continue is unavailable on this step, or None when it is free to go.
    ///
    /// The shell renders whatever comes back here beside the button, so a blocked step
    /// always explains itself rather than presenting a dead control.
    fn blocked_reason(&self, step: usize) -> Option<String>;

    /// Called when a step becomes visible, forwards or backwards.
    fn on_enter(&mut self, _step: usize) {}

    /// Going back throws away everything the later steps produced.
    ///
    /// The shell calls this on any backward move, so a flow never has to remember to do it
    /// and cannot be half-reset. The rule is the one a person expects: step back, and it is
    /// as though you had not gone forward - no old plan, no old result, no thread still
    /// running against the folder you have just stopped pointing at.
    ///
    /// Implementations must cancel in-flight work and drop its receiver, not merely blank
    /// the display: a late message from an abandoned run is exactly how the previous
    /// folder's result ends up reported against the new one.
    fn reset_after(&mut self, _step: usize) {}

    /// Asked when Continue is pressed, before the step advances.
    ///
    /// Return `Some` only when all three hold: pressing Continue actually does something,
    /// the result is expensive or hard to undo, and a warning is live right now. Anything
    /// less and this becomes a dialog people click through on reflex.
    fn confirm_advance(&self, _step: usize) -> Option<Confirm> {
        None
    }

    /// Called when the user leaves the flow, so a running job can be stopped.
    fn on_exit(&mut self) {}

    /// One line for the left of the nav bar, saying what this flow depends on and whether
    /// it has it. The shell has no idea which flows need adb, so it does not try to guess.
    fn status_note(&self) -> Option<(bool, String)> {
        None
    }
}

/// Renders the headset picker shared by both Quest flows, and returns a serial when the
/// user picks one.
///
/// Shared rather than written twice because the two differ only in wording, and a device
/// list that behaves differently between two screens of the same app is its own bug.
pub fn device_picker(ui: &mut Ui, snap: &Snapshot, chosen: &Option<String>) -> Option<String> {
    if snap.still_looking() {
        widgets::status(ui, Status::Info, "looking for a headset...");
        return None;
    }

    // Named rather than hidden: on a failing cable the truth is neither "connected" nor
    // "nothing there", and saying so is what tells someone their port is the problem.
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

    let devices = snap.devices();
    if devices.is_empty() {
        widgets::status(ui, Status::Info, "no headset connected");
        ui.label(
            RichText::new("Connect it by USB with Developer Mode on. This refreshes on its own.")
                .font(theme::font_ui(11.0))
                .color(theme::TEXT_FAINT),
        );
        if let Some(e) = snap.last_error() {
            ui.add_space(theme::UNIT * 0.5);
            widgets::status(ui, Status::Err, e);
        }
        return None;
    }

    // More than one device is the case that breaks the original outright, so it is offered
    // as a choice rather than treated as a failure.
    let multiple = devices.len() > 1;
    if multiple {
        ui.label(
            RichText::new("More than one device is attached. Choose the headset.")
                .font(theme::font_ui(12.0))
                .color(theme::TEXT_MUTED),
        );
        ui.add_space(theme::UNIT);
    }

    let mut picked = None;
    for d in devices {
        let name = d.model.clone().unwrap_or_else(|| d.serial.clone());
        let label = format!("{name}  ({})", d.state.describe());
        if multiple {
            let selected = chosen.as_deref() == Some(d.serial.as_str());
            if widgets::option_row(ui, selected, &label, &d.serial) {
                picked = Some(d.serial.clone());
            }
            ui.add_space(theme::UNIT * 0.5);
        } else {
            let kind = match d.state {
                State::Ready => Status::Ok,
                State::Unauthorized => Status::Warn,
                _ => Status::Err,
            };
            widgets::status(ui, kind, &label);
            widgets::mono_color(ui, &d.serial, 10.5, theme::TEXT_FAINT);
        }
        if d.state == State::Unauthorized {
            ui.label(
                RichText::new(
                    "Put the headset on and tap Allow on the USB debugging prompt. Replug \
                     the cable if it does not appear.",
                )
                .font(theme::font_ui(11.0))
                .color(theme::TEXT_DIM),
            );
        }
    }
    picked
}
