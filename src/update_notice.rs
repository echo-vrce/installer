// SPDX-License-Identifier: GPL-3.0-or-later
//! The startup update check, and the one line it is allowed to draw on Home.
//!
//! Nobody asked for this check, so it is held to a stricter standard than the rest of the
//! app: it never delays the window, never draws a spinner, and says nothing at all when
//! there is nothing to say. A progress indicator for something the user did not request is
//! worse than no indicator, because when it vanishes without a result it reads as a fault.
//!
//! What it does report is staleness rather than failure. One dropped connection means
//! nothing and a message about it is noise; a week without a successful check means a
//! firewall or a DNS block, and that is worth knowing, because otherwise the absence of a
//! line is indistinguishable from "you are up to date" and that is a lie by omission.

use std::sync::mpsc::Receiver;

use crate::config::Settings;
use crate::engine::selfupdate;
use crate::engine::Cancel;

/// Seconds between checks. Asking on every launch buys nothing and is rude to a server
/// that is answering for free.
const CHECK_EVERY: u64 = 24 * 60 * 60;

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the app knows about published versions, and how long ago it knew it.
///
/// Not `Clone`: it owns the receiving end of the check running on another thread, and two
/// copies of that would mean two halves of one answer.
#[derive(Debug, Default)]
pub struct State {
    /// Set only when a check succeeded and found something newer.
    pub newer: Option<String>,
    /// Unix seconds of the last successful check.
    pub checked_at: Option<u64>,
    /// The last attempt's failure, for the screen where someone is trying to find out why.
    pub last_error: Option<String>,
    waiting: Option<Receiver<Result<String, String>>>,
}

/// What Home should draw, if anything.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    Nothing,
    Available(String),
    /// No successful check in a long time. Carries the number of days.
    Stale(u64),
}

impl State {
    /// Kicks off a check if one is due. Returns immediately: the work is on its own thread
    /// and the window is drawn without waiting for it.
    pub fn begin_if_due(&mut self, settings: &Settings) {
        if !settings.update_check || self.waiting.is_some() {
            return;
        }
        self.checked_at = settings.update_checked_at;
        // What the last check saw, so a restart inside the interval still knows.
        self.newer = settings
            .update_latest_seen
            .as_deref()
            .filter(|v| selfupdate::is_newer(v, selfupdate::current()))
            .map(str::to_string);
        if let Some(last) = settings.update_checked_at {
            if now_secs().saturating_sub(last) < CHECK_EVERY {
                return;
            }
        }
        self.begin();
    }

    /// Checks now, whatever the settings say about how recently it last happened. This is
    /// the path a button takes.
    pub fn begin(&mut self) {
        if self.waiting.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = selfupdate::published(&Cancel::new()).map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        self.waiting = Some(rx);
    }

    pub fn is_checking(&self) -> bool {
        self.waiting.is_some()
    }

    /// Collects the answer if it has arrived. Returns true when something changed, so a
    /// caller can decide whether it is worth writing settings back to disk.
    pub fn pump(&mut self, settings: &mut Settings) -> bool {
        let Some(rx) = &self.waiting else { return false };
        let received = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.waiting = None;
                return false;
            }
        };
        self.waiting = None;

        match received {
            Ok(published) => {
                self.last_error = None;
                let current = selfupdate::current();
                self.newer =
                    selfupdate::is_newer(&published, current).then(|| published.clone());
                crate::log::line(&format!("update check: published {published}, running {current}"));
                // Only a success moves the clock. What gets reported is how long since the
                // app last knew something, not how long since it last tried.
                let stamp = now_secs();
                self.checked_at = Some(stamp);
                settings.update_checked_at = Some(stamp);
                settings.update_latest_seen = Some(published);
                true
            }
            Err(e) => {
                crate::log::line(&format!("update check failed: {e}"));
                self.last_error = Some(e);
                false
            }
        }
    }

    /// Days since the last successful check, if there ever was one.
    pub fn days_since_check(&self) -> Option<u64> {
        self.checked_at.map(|t| now_secs().saturating_sub(t) / 86_400)
    }

    /// The one thing Home is allowed to say.
    pub fn notice(&self, settings: &Settings) -> Notice {
        if !settings.update_check {
            return Notice::Nothing;
        }
        if let Some(v) = &self.newer {
            return Notice::Available(v.clone());
        }
        match self.days_since_check() {
            // Never checked successfully and not currently trying: the same silence as
            // "up to date" would be a lie, so it counts as stale from the start.
            None if !self.is_checking() => Notice::Stale(0),
            Some(days) if days >= selfupdate::STALE_AFTER_DAYS => Notice::Stale(days),
            _ => Notice::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_on() -> Settings {
        Settings { update_check: true, ..Settings::default() }
    }

    #[test]
    fn nothing_to_say_says_nothing() {
        let st = State { checked_at: Some(now_secs()), ..State::default() };
        assert_eq!(st.notice(&settings_on()), Notice::Nothing);
    }

    #[test]
    fn a_newer_version_is_announced() {
        let st = State {
            newer: Some("0.9.9".into()),
            checked_at: Some(now_secs()),
            ..State::default()
        };
        assert_eq!(st.notice(&settings_on()), Notice::Available("0.9.9".into()));
    }

    #[test]
    fn one_bad_day_is_not_worth_a_word() {
        // A single failure leaves the clock alone, and a check from yesterday is recent
        // enough that the app still knows what it is talking about.
        let st = State {
            checked_at: Some(now_secs() - 2 * 86_400),
            last_error: Some("connection refused".into()),
            ..State::default()
        };
        assert_eq!(st.notice(&settings_on()), Notice::Nothing);
    }

    #[test]
    fn a_week_of_bad_days_is() {
        let st = State {
            checked_at: Some(now_secs() - 9 * 86_400),
            ..State::default()
        };
        assert_eq!(st.notice(&settings_on()), Notice::Stale(9));
    }

    #[test]
    fn never_having_checked_is_stale_not_silent() {
        // Otherwise the absence of a line means both "up to date" and "never managed to
        // ask", and the user cannot tell which.
        assert_eq!(State::default().notice(&settings_on()), Notice::Stale(0));
    }

    #[test]
    fn switched_off_means_switched_off() {
        // Including the staleness line. Someone who turned the check off is not waiting to
        // be told that it has not run.
        let off = Settings { update_check: false, ..Settings::default() };
        let st = State { newer: Some("0.9.9".into()), ..State::default() };
        assert_eq!(st.notice(&off), Notice::Nothing);
        assert_eq!(State::default().notice(&off), Notice::Nothing);
    }

    #[test]
    fn a_due_check_respects_the_interval() {
        let mut recent = settings_on();
        recent.update_checked_at = Some(now_secs());
        let mut st = State::default();
        st.begin_if_due(&recent);
        assert!(!st.is_checking(), "checked again within the interval");

        let mut old = settings_on();
        old.update_checked_at = Some(now_secs() - CHECK_EVERY - 1);
        let mut st = State::default();
        st.begin_if_due(&old);
        assert!(st.is_checking());
    }
}

/// The install half: downloading the new version and putting it in place.
///
/// Kept beside the check rather than in the screen that draws it, so the screen stays a
/// screen. Uses the same stage-and-bar vocabulary every download flow in the app uses;
/// there is nothing new here for a reader to learn.
#[derive(Default)]
pub struct Installer {
    running: Option<Receiver<Msg>>,
    pub stage: Option<String>,
    pub progress: Option<(u64, u64)>,
    pub finished: Option<Result<(), String>>,
    cancel: Cancel,
}

enum Msg {
    Event(selfupdate::Event),
    Done(Result<(), String>),
}

impl Installer {
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn start(&mut self) {
        if self.running.is_some() {
            return;
        }
        self.stage = None;
        self.progress = None;
        self.finished = None;
        self.cancel = Cancel::new();
        let cancel = self.cancel.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let sender = tx.clone();
            let result = selfupdate::apply(&cancel, &mut |e| {
                let _ = sender.send(Msg::Event(e));
            });
            let _ = tx.send(Msg::Done(result.map_err(|e| e.to_string())));
        });
        self.running = Some(rx);
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Drains whatever the worker has said. Returns true while it is still working, so the
    /// caller knows to keep repainting.
    pub fn pump(&mut self) -> bool {
        let Some(rx) = &self.running else { return false };
        let mut done = None;
        for msg in rx.try_iter() {
            match msg {
                Msg::Event(selfupdate::Event::Stage(s)) => {
                    self.stage = Some(s.to_string());
                    self.progress = None;
                }
                Msg::Event(selfupdate::Event::Downloading(snap)) => {
                    self.progress = snap.total.map(|t| (snap.done, t));
                }
                Msg::Event(selfupdate::Event::Extracting { done: d, total }) => {
                    self.progress = Some((d, total));
                }
                Msg::Done(r) => done = Some(r),
            }
        }
        if let Some(r) = done {
            self.running = None;
            if let Err(e) = &r {
                crate::log::line(&format!("update failed: {e}"));
            }
            self.finished = Some(r);
            return false;
        }
        true
    }
}
