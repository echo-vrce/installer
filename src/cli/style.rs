// SPDX-License-Identifier: GPL-3.0-or-later
//! How the CLI looks.
//!
//! Same design as the window: one accent, a grey ramp, no boxes drawn in ASCII art, no
//! spinner. The rules are the terminal ones - colour is an enhancement and never the only
//! carrier of meaning, so every coloured thing also has a glyph or a word.

use std::io::IsTerminal;

use crate::engine::download::Snapshot;
use crate::fmt::{human_bytes, human_duration};

/// Truecolor escape for the theme's accent, `theme::ACCENT_TEXT`.
const ACCENT: (u8, u8, u8) = (0x5B, 0x9B, 0xFF);
const OK: (u8, u8, u8) = (0x3F, 0xB9, 0x50);
const WARN: (u8, u8, u8) = (0xD2, 0x99, 0x22);
const ERR: (u8, u8, u8) = (0xF8, 0x51, 0x49);
const DIM: (u8, u8, u8) = (0x8A, 0x92, 0x9E);

/// Bar width in cells. Fixed rather than measured: it has to look the same in a wrapped
/// log as on a wide terminal, and 24 fits comfortably inside 80 columns with the numbers.
const BAR: usize = 24;

/// Nothing here needs a font from this century.
///
/// Box drawing and block characters look fine in a modern terminal and turn into rubbish in
/// an old console, a wrong code page, or a raster font - and this is a tool people run on
/// whatever Windows they already have. Everything drawn below is typeable on a keyboard.
const SPINNER: [char; 4] = ['-', '\\', '|', '/'];

#[derive(Clone, Copy, PartialEq)]
pub struct Style {
    /// Machine-readable mode. Stdout carries one JSON object and nothing else; anything a
    /// person would want to read goes to stderr, which is the usual division of labour and
    /// keeps `prog --json | jq` working while a retry is happening.
    pub json: bool,
    pub colour: bool,
    /// Whether stdout is a terminal. Drives the progress line: rewriting with `\r` is
    /// right on a terminal and garbage in a file.
    pub tty: bool,
    pub quiet: bool,
}

impl Style {
    /// Honours `NO_COLOR` and a `--no-color` flag, and never colours a pipe.
    ///
    /// `NO_COLOR` is checked for presence, not value, which is what the convention says:
    /// setting it to `0` still means no colour.
    pub fn detect(no_color: bool, quiet: bool, json: bool) -> Style {
        let tty = std::io::stdout().is_terminal();
        let colour = tty
            && !no_color
            && !json
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
        // JSON implies quiet: every decorated line would be sitting in the same stream as
        // the object and would break the first thing that tried to parse it.
        Style { json, colour, tty, quiet: quiet || json }
    }

    /// Prints the one object, and hands back the exit code so call sites stay one line.
    pub fn emit(self, code: i32, value: serde_json::Value) -> i32 {
        if self.json {
            match serde_json::to_string_pretty(&value) {
                Ok(text) => println!("{text}"),
                Err(e) => eprintln!("could not serialise the result: {e}"),
            }
        }
        code
    }

    fn paint(self, rgb: (u8, u8, u8), text: &str) -> String {
        if !self.colour {
            return text.to_string();
        }
        format!("\x1b[38;2;{};{};{}m{text}\x1b[0m", rgb.0, rgb.1, rgb.2)
    }

    pub fn accent(self, text: &str) -> String {
        self.paint(ACCENT, text)
    }

    pub fn dim(self, text: &str) -> String {
        self.paint(DIM, text)
    }

    pub fn bold(self, text: &str) -> String {
        if !self.colour {
            return text.to_string();
        }
        format!("\x1b[1m{text}\x1b[0m")
    }

    /// Prints a line, and records it.
    ///
    /// The record is not optional and `--quiet` does not silence it. That flag is about the
    /// screen; the log is what an elevated run reports back through, and it is what someone
    /// reads afterwards. Getting this wrong is how an elevated run came to look like it did
    /// nothing at all: it was running fine and telling nobody.
    ///
    /// `plain` is what goes in the log - escape sequences in a file help no one.
    fn say(self, plain: &str, decorated: String) {
        crate::log::line(plain);
        if !self.quiet {
            println!("{decorated}");
        }
    }

    /// As `say`, but for the channel a person is meant to notice.
    fn say_loud(self, plain: &str, decorated: String) {
        crate::log::line(plain);
        eprintln!("{decorated}");
    }

    /// A section heading: the name, then a rule out to a fixed column, like the divider
    /// under every section label in the window.
    pub fn heading(self, text: &str) {
        let rule = "-".repeat(62usize.saturating_sub(text.chars().count() + 3));
        self.say(&format!("== {text}"), format!("\n  {}  {}", self.bold(text), self.dim(&rule)));
    }

    /// A label/value line. Labels are padded to a common column so a run of them reads as
    /// a table without drawing one.
    pub fn field(self, label: &str, value: &str) {
        self.say(
            &format!("{label}: {value}"),
            format!("  {}  {value}", self.dim(&format!("{label:<10}"))),
        );
    }

    pub fn ok(self, text: &str) {
        self.say(&format!("ok: {text}"), format!("  {}  {text}", self.paint(OK, "ok")));
    }

    pub fn info(self, text: &str) {
        self.say(text, format!("  {}  {text}", self.dim("..")));
    }

    pub fn warn(self, text: &str) {
        // Warnings survive --quiet, and --json: they are on stderr, and they are the reason
        // someone reads the output later.
        self.say_loud(&format!("warning: {text}"), format!("  {}  {text}", self.paint(WARN, "!!")));
    }

    pub fn err(self, text: &str) {
        // A cancel reaches here as whatever error stopping produced. It is still worth
        // recording, but showing it in red would tell someone their own Ctrl+C was a fault.
        if crate::cli::interrupted().is_cancelled() {
            crate::log::line(&format!("cancelled: {text}"));
            return;
        }
        self.say_loud(&format!("error: {text}"), format!("  {}  {text}", self.paint(ERR, "XX")));
    }

    pub fn plain(self, text: &str) {
        self.say(text, format!("  {text}"));
    }

    /// One progress line, rewritten in place on a terminal.
    ///
    /// Off a terminal this prints nothing: the caller emits milestones instead, because a
    /// file full of carriage returns is worse than no progress at all.
    pub fn progress(self, done: u64, total: u64, rate: Option<f64>, eta: Option<std::time::Duration>) {
        self.progress_labelled(done, total, rate, eta, "");
    }

    fn progress_labelled(
        self,
        done: u64,
        total: u64,
        rate: Option<f64>,
        eta: Option<std::time::Duration>,
        note: &str,
    ) {
        if self.quiet || !self.tty {
            return;
        }
        let known = total > 0;
        let frac = if known { (done as f64 / total as f64).clamp(0.0, 1.0) } else { 0.0 };

        let bar = if known {
            // `====>` with the head only while there is still room for one, so a finished
            // bar reads as full rather than as one short.
            let filled = (frac * BAR as f64).round() as usize;
            let head = usize::from(filled < BAR);
            format!(
                "[{}{}{}]",
                "=".repeat(filled.saturating_sub(head)),
                if head == 1 { ">" } else { "" },
                " ".repeat(BAR - filled)
            )
        } else {
            // No total to fill, so the only honest thing to show is that it is still moving.
            let tick = SPINNER[(done as usize / 65_536) % SPINNER.len()];
            format!("[{}{tick}{}]", " ".repeat(BAR / 2), " ".repeat(BAR - BAR / 2 - 1))
        };

        let mut tail = if known {
            format!("{:>3}%   {}", (frac * 100.0) as u32, human_bytes(done))
        } else {
            format!("       {}", human_bytes(done))
        };
        if known {
            tail.push_str(&format!(" / {}", human_bytes(total)));
        }
        if let Some(r) = rate {
            tail.push_str(&format!("   {}/s", human_bytes(r as u64)));
        }
        if let Some(d) = eta {
            tail.push_str(&format!("   {} left", human_duration(d)));
        }
        if !note.is_empty() {
            tail.push_str(&self.paint(WARN, note));
        }
        // Pad to clear whatever the previous, longer line left behind.
        print!("\r  {}  {tail}{:<12}", self.accent(&bar), "");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    /// The download case, which is most of them.
    ///
    /// A retry is called out on the bar itself rather than as a separate line: it is the
    /// same download continuing, and a new line every few seconds on a flaky connection
    /// would bury the thing being retried.
    pub fn download(self, snap: &Snapshot) {
        if snap.attempt > 0 {
            self.progress_labelled(
                snap.done,
                snap.total.unwrap_or(0),
                Some(snap.bytes_per_sec),
                snap.eta(),
                &format!("  retry {}/{}", snap.attempt, crate::engine::download::RETRIES),
            );
        } else {
            self.progress(snap.done, snap.total.unwrap_or(0), Some(snap.bytes_per_sec), snap.eta());
        }
    }

    /// Ends a progress line so the next print starts on a fresh row.
    pub fn progress_done(self) {
        if self.quiet || !self.tty {
            return;
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Style {
        Style { json: false, colour: false, tty: false, quiet: false }
    }

    #[test]
    fn without_colour_nothing_is_escaped() {
        let s = plain();
        assert_eq!(s.accent("x"), "x");
        assert_eq!(s.dim("x"), "x");
        assert_eq!(s.bold("x"), "x");
    }

    #[test]
    fn json_silences_stdout_decoration_and_colour() {
        // Both matter for the same reason: anything else on stdout breaks the first thing
        // that tries to parse it.
        let s = Style::detect(false, false, true);
        assert!(s.quiet, "--json must imply quiet");
        assert!(!s.colour, "--json must never emit escapes");
    }

    #[test]
    fn with_colour_the_sequence_is_reset_again() {
        let s = Style { json: false, colour: true, tty: true, quiet: false };
        let painted = s.accent("x");
        assert!(painted.starts_with("\x1b[38;2;91;155;255m"), "got {painted:?}");
        assert!(painted.ends_with("\x1b[0m"));
        // Colour must never be the only difference: the caller pairs it with a glyph.
        assert!(painted.contains('x'));
    }
}
