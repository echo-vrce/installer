// SPDX-License-Identifier: GPL-3.0-or-later
// Echo VRCE Installer - a rebuild of the Echo VR Installer with a native UI.
//
// Windows is the shipping target; it also has to survive Wine/Proton, which is why the
// renderer is glow (OpenGL) rather than wgpu. Builds and runs on Linux too, purely so
// the UI can be iterated on without a VM round-trip.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use echo_vrce_installer::{app, config, log, logo, os, theme};

/// Rasterised from the same geometry the in-app mark uses, so there is one definition
/// of the disc and no icon asset to keep in sync. The Bright variant is deliberate: the
/// in-app blue lacks punch against a taskbar.
fn window_icon() -> egui::IconData {
    const SIDE: u32 = 256;
    egui::IconData {
        rgba: logo::icon_rgba(SIDE, theme::ACCENT_TEXT),
        width: SIDE,
        height: SIDE,
    }
}

fn main() -> eframe::Result {
    // First line of the process: it decides whether a bad path in a file picker is an
    // error this code reports, or a system dialog that freezes the window behind it.
    os::quiet_hard_error_dialogs();

    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Before anything else that can fail, so a failure early in startup still leaves a
    // file behind. A GUI build on Windows has no console, so this is the only record.
    log::install_panic_hook();
    log::init(&config::logs_dir(), false);
    // The two questions every support conversation opens with, answered before anyone has
    // to ask them.
    log::line(&format!("app data: {}", config::dir().display()));
    if let Ok(exe) = std::env::current_exe() {
        log::line(&format!("running:  {}", exe.display()));
    }

    // The command line used to live behind `--cli` on this binary. Anyone who learned that,
    // or copied it from an older note, gets told where it went instead of a window opening
    // as though they had typed nothing.
    if argv.iter().any(|a| a == "--cli" || a == "-c") {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Info)
            .set_title("Echo VRCE Installer")
            .set_description(
                "The command line is its own program now: echo-vrce-cli.exe, in this same \
                 folder.\n\nIt was a flag on this one, but a window's executable cannot \
                 report an exit code to PowerShell, which made every scripted use of it \
                 quietly useless.\n\nRun echo-vrce-cli --help to start.",
            )
            .show();
        return Ok(());
    }

    // Spike-only: `--at w2` opens straight onto a given screen, so the visuals can be
    // reviewed without clicking through. Drops out with the fake data.
    let mut start = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == "--at" {
            start = it.next().cloned();
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Echo VRCE Installer")
            // The smallest size the layout still looks composed at. It was taller, so the
            // whole task list would fit without scrolling, but that made the window
            // overbearing for what it holds. The scrollbar is always drawn instead, so
            // there is never any doubt that there is more below.
            .with_inner_size([920.0, 640.0])
            // Resizable, unlike the fixed 1280x720 of the original - but with a floor,
            // below which the step column and content stop coexisting.
            .with_min_inner_size([880.0, 520.0])
            .with_icon(window_icon()),
        ..Default::default()
    };

    log::line("opening the window");
    let result = eframe::run_native(
        "Echo VRCE Installer",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(match start {
                Some(spec) => app::App::starting_at(&spec),
                None => app::App::default(),
            }))
        }),
    );

    // A window that fails to open in a GUI subsystem binary takes the whole process with it
    // and shows nothing at all: no console to print to, no window to put a message in. So
    // the failure is logged, and then said out loud in the only place left.
    if let Err(e) = &result {
        let detail = e.to_string();
        log::line(&format!("the window could not be opened: {detail}"));
        report_startup_failure(&detail);
    }
    result
}

/// The last resort when there is no window and no console.
///
/// `rfd` puts up a native dialog without needing an event loop of its own, which is the
/// only reason there is anything to say here at all.
fn report_startup_failure(detail: &str) {
    // Graphics is the overwhelmingly likely cause, and the wording matters: "it needs a
    // graphics driver" is actionable, the raw glutin error is not. A virtual machine with
    // no 3D acceleration is the usual way to meet this.
    let looks_graphical = detail.to_lowercase().contains("opengl")
        || detail.to_lowercase().contains("glutin")
        || detail.to_lowercase().contains("context")
        || detail.to_lowercase().contains("pixel format");

    let body = if looks_graphical {
        // Ordered for the person most likely to see it: someone on real hardware with a
        // driver problem, not someone in a virtual machine. The raw error is last, because
        // it is for whoever they forward this to, not for them.
        format!(
            "Echo VRCE Installer needs OpenGL 3.3, and this system does not provide it.\n\n\
             The usual cause is a graphics driver that is missing or out of date. \
             Installing the driver from your graphics card maker's site normally fixes it. \
             Windows Update alone often does not, because it ships a display driver without \
             the full OpenGL support.\n\n\
             If this is a virtual machine, that is expected: most have no 3D acceleration.\n\n\
             Everything except the window still works from the command line:\n\
             echo-vrce-cli.exe --help\n\n\
             Technical detail:\n{detail}"
        )
    } else {
        format!(
            "Echo VRCE Installer could not start.\n\n\
             The command line may still work:\n\
             echo-vrce-cli.exe --help\n\n\
             Technical detail:\n{detail}"
        )
    };

    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Echo VRCE Installer")
        .set_description(&body)
        .show();
}

