// SPDX-License-Identifier: GPL-3.0-or-later
//! The shared widget vocabulary. Every screen is built from these, so a change to a
//! path field or a status line lands everywhere at once.
//!
//! Layout convention throughout: one left-aligned column, label above field. Centred
//! layouts read as decorative; left-aligned ones read as functional, which is the
//! whole register this tool is aiming for.

use egui::{vec2, Align2, Color32, Rect, Response, RichText, Sense, Ui};

use crate::{icons, theme};

/// Step header: counter, title, and the rule under it.
pub fn step_heading(ui: &mut Ui, index: usize, total: usize, title: &str) {
    ui.label(
        RichText::new(format!("STEP {} OF {}", index + 1, total))
            .font(theme::font_med(10.0))
            .color(theme::TEXT_FAINT),
    );
    ui.add_space(2.0);
    ui.label(RichText::new(title).font(theme::font_med(19.0)).color(theme::TEXT));
    ui.add_space(theme::UNIT * 1.25);
    rule(ui);
    ui.add_space(theme::UNIT * 1.75);
}

/// A 1px hairline across the available width.
pub fn rule(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, theme::DIVIDER),
    );
}

/// Small caps label that sits above a field.
pub fn field_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::font_ui(11.0)).color(theme::TEXT_MUTED));
    ui.add_space(3.0);
}

/// A section label with a rule running off to the right of it.
///
/// Deliberately grey rather than accent blue: a list screen stacks four of these, and
/// in accent they stripe the page and compete with the two places accent actually
/// means something - the primary button and the current step.
pub fn section_label(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(text).font(theme::font_med(10.0)).color(theme::TEXT_DIM));
        let (rect, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().hline(
            rect.x_range(),
            rect.center().y,
            egui::Stroke::new(1.0, theme::DIVIDER),
        );
    });
    ui.add_space(theme::UNIT);
}

/// Paths and hashes get monospace because it is functional here: it aligns, and it
/// makes hex readable. Nothing else in the app does.
pub fn path_field(ui: &mut Ui, value: &mut String, width: f32) -> Response {
    let response = ui.add_sized(
        vec2(width, 30.0),
        egui::TextEdit::singleline(value)
            .font(theme::font_mono(12.0))
            .margin(egui::Margin::symmetric(9, 7))
            .vertical_align(egui::Align::Center),
    );
    // Every path the user types in this app comes through here, so the clipboard noise
    // Windows adds is dealt with once rather than at each of the six screens that ask for
    // a folder. Doing it on change rather than on blur means the checks under the field
    // agree with what is in it from the first frame.
    if response.changed() {
        crate::engine::path_input::clean_in_place(value);
    }
    response
}

pub enum Status {
    Ok,
    Info,
    Warn,
    Err,
}

/// Informative detection: what the app sees, stated plainly, never blocking. This is
/// the line that saves someone discovering a wrong path 8 GB into a download.
pub fn status(ui: &mut Ui, kind: Status, text: &str) {
    ui.horizontal(|ui| {
        status_inline(ui, kind, text);
    });
}

/// The icon and its line, drawn into the row already in progress.
///
/// Container-free on purpose, and that is the whole point of it existing. A nested
/// `ui.horizontal` inside a vertically centred row claims the full available height rather
/// than its own, so a bar sized from it overflows its panel and egui clips the frame: the
/// bar renders *shorter* than the size it asked for, by an amount that depends on what is
/// inside it. Returns the union of what was drawn, for a caller making the line clickable.
pub fn status_inline(ui: &mut Ui, kind: Status, text: &str) -> Rect {
    let size = 13.0;
    let (rect, _) = ui.allocate_exact_size(vec2(size, size + 2.0), Sense::hover());
    let icon_rect = Rect::from_center_size(rect.center(), vec2(size, size));
    let painter = ui.painter().clone();
    match kind {
        Status::Ok => icons::check(&painter, icon_rect, theme::SUCCESS),
        Status::Info => icons::info(&painter, icon_rect, theme::ACCENT_TEXT),
        Status::Warn => icons::warning(&painter, icon_rect, theme::WARNING),
        Status::Err => icons::cross(&painter, icon_rect, theme::ERROR),
    }
    ui.add_space(6.0);
    let label = breaking_label(ui, text, theme::font_ui(12.0), theme::TEXT_MUTED);
    rect.union(label.rect)
}

/// The one accent-filled button on screen. There is never more than one.
pub fn primary(ui: &mut Ui, label: &str, enabled: bool) -> bool {
    let fill = if enabled { theme::ACCENT } else { theme::SURFACE_RAISED };
    let text = if enabled { theme::ON_ACCENT } else { theme::TEXT_FAINT };
    let btn = egui::Button::new(RichText::new(label).font(theme::font_med(12.5)).color(text))
        .fill(fill)
        .stroke(egui::Stroke::new(
            1.0,
            if enabled { theme::ACCENT } else { theme::BORDER },
        ))
        .min_size(vec2(104.0, 30.0));
    ui.add_enabled(enabled, btn).clicked()
}

pub fn secondary(ui: &mut Ui, label: &str, enabled: bool) -> bool {
    let btn = egui::Button::new(RichText::new(label).font(theme::font_ui(12.5)))
        .min_size(vec2(76.0, 30.0));
    ui.add_enabled(enabled, btn).clicked()
}

/// A download row: name in monospace, bar, then the numbers that actually matter -
/// bytes, rate, time left. A progress bar is data, not decoration, so it stays even
/// though the app has no animation.
pub fn progress_row(ui: &mut Ui, name: &str, fraction: f32, detail: &str) {
    ui.label(RichText::new(name).font(theme::font_mono(11.0)).color(theme::TEXT_DIM));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 52.0).max(80.0);
        let (rect, _) = ui.allocate_exact_size(vec2(w, 6.0), Sense::hover());
        let painter = ui.painter();
        let radius = egui::CornerRadius::same(3);
        painter.rect_filled(rect, radius, theme::SURFACE_HOVER);
        if fraction > 0.0 {
            let mut filled = rect;
            filled.set_width(rect.width() * fraction.clamp(0.0, 1.0));
            painter.rect_filled(filled, radius, theme::ACCENT_HOVER);
        }
        ui.add_space(10.0);
        ui.label(
            RichText::new(format!("{:>3.0}%", fraction * 100.0))
                .font(theme::font_mono_med(11.0))
                .color(theme::TEXT),
        );
    });
    ui.add_space(2.0);
    ui.label(RichText::new(detail).font(theme::font_mono(10.0)).color(theme::TEXT_DIM));
}

#[derive(Clone, Copy, PartialEq)]
pub enum RowState {
    Pending,
    Working,
    Done,
    Failed,
}

/// One line of a checklist. Used for the adb install sequence and the Revive chain, so
/// the user can see exactly what is being done to their machine or headset.
pub fn check_row(ui: &mut Ui, state: RowState, label: &str) {
    ui.horizontal(|ui| {
        let size = 13.0;
        let (rect, _) = ui.allocate_exact_size(vec2(size, size + 4.0), Sense::hover());
        let icon_rect = Rect::from_center_size(rect.center(), vec2(size, size));
        let painter = ui.painter().clone();
        match state {
            RowState::Pending => icons::dot_hollow(&painter, icon_rect, theme::TEXT_FAINT),
            RowState::Working => icons::dot_filled(&painter, icon_rect, theme::ACCENT_HOVER),
            RowState::Done => icons::check(&painter, icon_rect, theme::SUCCESS),
            RowState::Failed => icons::cross(&painter, icon_rect, theme::ERROR),
        }
        ui.add_space(8.0);
        let colour = match state {
            RowState::Pending => theme::TEXT_FAINT,
            RowState::Working => theme::TEXT,
            RowState::Done => theme::TEXT_MUTED,
            RowState::Failed => theme::TEXT,
        };
        breaking_label(ui, label, theme::font_ui(12.0), colour);
    });
}

/// A selectable option with its consequence spelled out underneath. Radios rather
/// than big buttons that jump you forward: the user picks, then presses Continue.
pub fn option_row(ui: &mut Ui, selected: bool, title: &str, consequence: &str) -> bool {
    let w = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(w, 46.0), Sense::click());
    let painter = ui.painter().clone();

    let hovered = response.hovered();
    let fill = if selected {
        theme::ACCENT_SUBTLE
    } else if hovered {
        theme::SURFACE_HOVER
    } else {
        theme::SURFACE
    };
    let stroke = egui::Stroke::new(1.0, if selected { theme::ACCENT } else { theme::BORDER });
    painter.rect(rect, egui::CornerRadius::same(theme::RADIUS), fill, stroke, egui::StrokeKind::Inside);

    let marker = Rect::from_center_size(
        egui::pos2(rect.left() + 18.0, rect.center().y),
        vec2(14.0, 14.0),
    );
    painter.circle_stroke(
        marker.center(),
        6.0,
        egui::Stroke::new(1.2, if selected { theme::ACCENT_TEXT } else { theme::TEXT_FAINT }),
    );
    if selected {
        painter.circle_filled(marker.center(), 3.2, theme::ACCENT_TEXT);
    }

    let tx = marker.right() + 12.0;
    painter.text(
        egui::pos2(tx, rect.top() + 9.0),
        Align2::LEFT_TOP,
        title,
        theme::font_med(12.5),
        // Unselected options step back, the way pending steps do in the side column. The
        // branch used to return the same colour twice, which said a difference was intended
        // and then did not make one.
        if selected { theme::TEXT } else { theme::TEXT_MUTED },
    );
    painter.text(
        egui::pos2(tx, rect.top() + 26.0),
        Align2::LEFT_TOP,
        consequence,
        theme::font_ui(11.0),
        theme::TEXT_DIM,
    );

    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

/// Collapsible raw-output pane. Cheap to build and the best support tool the app will
/// have: "paste the log" beats twenty questions.
pub fn log_pane(ui: &mut Ui, open: &mut bool, lines: &[String]) {
    let header = ui.horizontal(|ui| {
        let (rect, resp) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::click());
        let painter = ui.painter().clone();
        icons::chevron(&painter, rect, theme::TEXT_MUTED, *open);
        ui.add_space(4.0);
        let label = ui.add(
            egui::Label::new(
                RichText::new("Log").font(theme::font_ui(11.0)).color(theme::TEXT_MUTED),
            )
            .sense(Sense::click()),
        );
        resp.clicked() || label.clicked()
    });
    if header.inner {
        *open = !*open;
    }

    if *open {
        ui.add_space(4.0);
        egui::Frame::new()
            .fill(theme::SURFACE_RAISED)
            .stroke(egui::Stroke::new(1.0, theme::BORDER))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS))
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(96.0)
                    .auto_shrink([false, true])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in lines {
                            breaking_label(
                                ui,
                                line,
                                theme::font_mono(10.5),
                                theme::TEXT_DIM,
                            );
                        }
                    });
            });
    }
}

/// A bordered surface panel. Groups related controls without needing a heading.
///
/// Always spans the content column: a Frame otherwise shrinks to its contents, which
/// left a checklist card and a progress card side by side at different widths.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let full = ui.available_width();
    egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            // Both, and the maximum is the one that was missing. A Frame grows to fit its
            // contents, and text inside a horizontal layout does not wrap - so one long
            // line pushed the card off the edge of the window and nothing stopped it.
            // Bounding the width here is also what makes `available_width` mean something
            // to everything drawn inside.
            let inner = full - 30.0; // minus the two 14 px margins and the strokes
            ui.set_min_width(inner);
            ui.set_max_width(inner);
            add(ui)
        })
        .inner
}

/// Muted monospace key/value line, for the About build info and version markers.
pub fn kv(ui: &mut Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(key).font(theme::font_ui(11.0)).color(theme::TEXT_DIM));
        // The value is usually a path, and it gets whatever the key left behind.
        breaking_label(ui, value, theme::font_mono(11.0), theme::TEXT_MUTED);
    });
}

/// Text link that opens something outside the app.
pub fn external_link(ui: &mut Ui, label: &str, url: &str) {
    let resp = ui.horizontal(|ui| {
        let r = ui.add(
            egui::Label::new(
                RichText::new(label).font(theme::font_ui(12.0)).color(theme::ACCENT_TEXT),
            )
            .sense(Sense::click()),
        );
        let (rect, _) = ui.allocate_exact_size(vec2(11.0, 11.0), Sense::hover());
        let painter = ui.painter().clone();
        icons::arrow_out(&painter, rect, theme::ACCENT_TEXT);
        r
    });
    if resp.inner.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.inner.clicked() {
        let _ = open_url(url);
    }
}

/// Opens a folder in the system file manager.
pub fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    open_url(&path.to_string_lossy())
}

fn open_url(url: &str) -> std::io::Result<()> {
    // Windows is the shipping target; the others are here so the thing is testable on
    // a Linux dev box.
    #[cfg(target_os = "windows")]
    let (cmd, args): (&str, Vec<&str>) = ("rundll32", vec!["url.dll,FileProtocolHandler", url]);
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (cmd, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    crate::engine::hide_console(&mut std::process::Command::new(cmd)).args(args).spawn().map(|_| ())
}

/// A label that stays inside its column, breaking where a reader would.
///
/// egui offers two modes and neither is right on its own. Word wrapping leaves a long path
/// hanging off the edge of the window, because a path is a single unbreakable token. Break
/// anywhere fixes that and ruins everything else: it will split `SteamVR` across two lines
/// while there is a space sitting right beside it.
///
/// So the choice is made per token, in order of how much a reader minds:
///
/// 1. Spaces, wherever they exist. That is ordinary wrapping and it handles all prose.
/// 2. Inside a token too long to fit, after a separator - `\`, `/`, `-`, `_`, `.` - so a
///    path breaks between its parts, which is where someone reading it would pause anyway.
/// 3. Only when a single run of characters still will not fit, mid-token.
pub fn breaking_label(
    ui: &mut Ui,
    text: &str,
    font: egui::FontId,
    colour: Color32,
) -> egui::Response {
    // Inside a horizontal layout egui hands children an unbounded width - that is what
    // "extend" means - so `available_width` can come back as infinity. Wrapping to infinity
    // is not wrapping, so it falls back to the width of the enclosing panel.
    let mut room = ui.available_width();
    if !room.is_finite() || room <= 0.0 {
        room = ui.max_rect().width().max(120.0);
    }

    let prepared = insert_breaks(ui, text, &font, room);
    let mut job = egui::text::LayoutJob::simple(prepared, font, colour, room);
    // Words from here on: the only tokens that needed splitting have already been split,
    // and doing it again would undo the point of having chosen where.
    job.wrap.break_anywhere = false;
    // Set explicitly because a horizontal parent would otherwise override it back to
    // extending, which is the whole problem.
    ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Wrap))
}

/// Where a path or a hash may break, in order of preference. A break lands *after* one of
/// these, keeping the separator with the part it belongs to.
const BREAK_AFTER: [char; 5] = ['\\', '/', '-', '_', '.'];

/// Splits only the tokens that do not fit, leaving everything else exactly as written.
fn insert_breaks(ui: &Ui, text: &str, font: &egui::FontId, room: f32) -> String {
    let painter = ui.painter();
    insert_breaks_with(text, room, |s| {
        painter.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).size().x
    })
}

/// The rule on its own, with the font taken out of it.
///
/// Separated so the cascade can be tested without a window: the order it tries things in is
/// the whole contract, and it is easy to get subtly wrong.
fn insert_breaks_with(text: &str, room: f32, width: impl Fn(&str) -> f32) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, token) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if width(token) <= room {
            out.push_str(token);
            continue;
        }
        // Too long for the column even on a line of its own. Fill greedily, and when the
        // next character would overflow, go back to the last separator in what has been
        // gathered rather than cutting wherever the tally happened to run out.
        let mut line = String::new();
        let mut since_separator: Option<usize> = None;
        for ch in token.chars() {
            let mut candidate = line.clone();
            candidate.push(ch);
            if width(&candidate) > room && !line.is_empty() {
                match since_separator {
                    Some(at) if at > 0 => {
                        let (keep, carry) = line.split_at(at);
                        out.push_str(keep);
                        out.push('\n');
                        line = carry.to_string();
                    }
                    _ => {
                        out.push_str(&line);
                        out.push('\n');
                        line.clear();
                    }
                }
                since_separator = None;
            }
            line.push(ch);
            if BREAK_AFTER.contains(&ch) {
                since_separator = Some(line.len());
            }
        }
        out.push_str(&line);
    }
    out
}

pub fn mono_color(ui: &mut Ui, text: &str, size: f32, colour: Color32) {
    breaking_label(ui, text, theme::font_mono(size), colour);
}

/// The confirmation dialog, used both by the shell before a step advances and by a flow
/// before a button does something expensive.
///
/// One drawing, two callers: a confirmation that looks different depending on which part of
/// the app raised it is a confirmation people stop trusting.
///
/// Returns `Some(true)` when accepted, `Some(false)` when declined, `None` while it is
/// still open. Declining clears it, so the next press asks again rather than being waved
/// through on a stale answer.
pub fn confirm_modal(ui: &mut Ui, pending: &mut Option<crate::flows::Confirm>) -> Option<bool> {
    let c = pending.as_ref()?;
    let (title, consequence, proceed) = (c.title.clone(), c.consequence.clone(), c.proceed.clone());

    // egui's default modal frame is tight against the text. A dialog that interrupts you
    // should not also feel cramped, so it gets the same breathing room as a card.
    let frame = egui::Frame {
        inner_margin: egui::Margin::same(theme::UNIT as i8 * 3),
        fill: theme::SURFACE_RAISED,
        stroke: egui::Stroke::new(1.0, theme::BORDER),
        corner_radius: egui::CornerRadius::same(theme::RADIUS),
        ..Default::default()
    };

    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::new("evrce.confirm"))
        .frame(frame)
        // Darker than egui's default: the page behind should read as out of reach, not as
        // merely tinted.
        .backdrop_color(egui::Color32::from_black_alpha(160))
        .show(ui.ctx(), |ui| {
        ui.set_width(430.0);
        ui.label(RichText::new(&title).font(theme::font_med(15.0)).color(theme::TEXT));
        ui.add_space(theme::UNIT * 1.25);
        ui.label(RichText::new(&consequence).font(theme::font_ui(12.0)).color(theme::TEXT_MUTED));
        ui.add_space(theme::UNIT * 2.5);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // The affirmative on the right, where Continue sits in the nav bar underneath,
            // so the hand does not have to learn a second place.
            if primary(ui, &proceed, true) {
                answer = Some(true);
            }
            ui.add_space(2.0);
            if secondary(ui, "Cancel", true) {
                answer = Some(false);
            }
        });
    });

    // Clicking away or pressing Escape means no. A dialog that only closes on a deliberate
    // answer is one people answer carelessly to get rid of.
    if modal.should_close() {
        answer = Some(false);
    }
    if answer.is_some() {
        *pending = None;
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One unit per character, so a width is just a length and the arithmetic is readable.
    fn chars(s: &str) -> f32 {
        s.chars().count() as f32
    }

    #[test]
    fn text_that_fits_is_left_exactly_as_written() {
        let t = "Revive setup finished";
        assert_eq!(insert_breaks_with(t, 100.0, chars), t, "nothing to do, so do nothing");
    }

    #[test]
    fn spaces_are_preferred_and_words_survive() {
        // The bug that made this necessary: SteamVR was split across two lines while a
        // space sat right beside it.
        let out = insert_breaks_with("install SteamVR once then retry", 10.0, chars);
        assert!(!out.contains('\n'), "every token fits, so wrapping is egui's job: {out:?}");
        assert!(out.contains("SteamVR"), "the word must survive whole: {out:?}");
    }

    #[test]
    fn a_long_path_breaks_after_a_separator() {
        let out = insert_breaks_with(r"C:\Program\Files\Meta\Horizon\Software", 12.0, chars);
        for line in out.lines() {
            assert!(chars(line) <= 12.0, "line too wide: {line:?}");
            if line != out.lines().last().unwrap() {
                assert!(
                    line.ends_with('\\'),
                    "a break should land after a separator, not mid-name: {line:?}"
                );
            }
        }
    }

    #[test]
    fn a_run_with_nowhere_to_break_is_cut_at_the_edge() {
        // Last resort, and it has to work: a hash has no spaces and no separators.
        let hash = "a".repeat(64);
        let out = insert_breaks_with(&hash, 10.0, chars);
        assert!(out.contains('\n'), "it has to break somewhere");
        for line in out.lines() {
            assert!(chars(line) <= 10.0, "line too wide: {line:?}");
        }
        assert_eq!(out.replace('\n', ""), hash, "and nothing may be lost doing it");
    }

    #[test]
    fn nothing_is_ever_dropped() {
        for t in [
            r"C:\Program Files\Meta Horizon\Software\Software\ready-at-dawn-echo-arena",
            "a b c",
            "",
            "-----",
        ] {
            let out = insert_breaks_with(t, 7.0, chars);
            assert_eq!(out.replace('\n', ""), t.replace('\n', ""), "text changed: {t:?}");
        }
    }
}
