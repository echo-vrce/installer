// SPDX-License-Identifier: GPL-3.0-or-later
//! Watching for devices without blocking the window.
//!
//! Polling adb from inside the frame loop is fine right up until it is not: on a healthy
//! port `adb devices` answers in tens of milliseconds, but a failing cable, a device
//! mid-re-enumeration, or a cold adb server can take seconds, and the window is frozen for
//! every one of them. So the poll lives on its own thread and the UI reads whatever the
//! last one produced.
//!
//! The second half of the problem is flapping. On an unreliable port adb reports the
//! headset, then nothing, then the headset again, seconds apart. Reflecting that literally
//! gives a UI that blinks between "connected" and "no headset" and is useless to the person
//! trying to work out whether their cable is bad. So a device is not declared gone until
//! several polls in a row have missed it, and the meantime is reported honestly as an
//! unstable connection rather than as either state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::engine::adb::{Adb, Device, State};

/// While something is attached, more often: this is when someone is watching the screen to
/// see whether their cable works.
const POLL_PRESENT: Duration = Duration::from_secs(2);
/// While nothing is attached, less often. Nobody is waiting on a millisecond here, and each
/// poll is a process.
const POLL_ABSENT: Duration = Duration::from_secs(3);
/// Consecutive misses before a device is treated as really gone. Three polls is six to nine
/// seconds, which comfortably outlasts a re-enumeration but is still quick enough that
/// unplugging feels immediate.
const GRACE_MISSES: u32 = 3;

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The most recent successful, non-empty reading.
    devices: Vec<Device>,
    /// Polls in a row that found nothing or failed, since that reading.
    misses: u32,
    last_success: Option<Instant>,
    last_error: Option<String>,
    polled_at_least_once: bool,
}

impl Snapshot {
    /// The headset to act on: the one explicitly chosen, or the only ready one.
    ///
    /// Returns nothing when several are ready and none was chosen. That is deliberate and
    /// it is the rule everywhere: picking for someone is how the wrong headset gets wiped.
    pub fn pick(&self, chosen: Option<&str>) -> Option<Device> {
        if let Some(serial) = chosen {
            return self
                .devices()
                .iter()
                .find(|d| d.serial == serial && d.state == State::Ready)
                .cloned();
        }
        let mut ready = self.devices().iter().filter(|d| d.state == State::Ready);
        let only = ready.next()?;
        ready.next().is_none().then(|| only.clone())
    }

    /// The devices the UI should show. Empty once the grace period has run out.
    pub fn devices(&self) -> &[Device] {
        if self.misses >= GRACE_MISSES {
            &[]
        } else {
            &self.devices
        }
    }

    /// True while a device has been seen recently but the last poll or two missed it. The
    /// honest answer for a bad cable, and better than claiming either extreme.
    pub fn unstable(&self) -> bool {
        self.misses > 0 && self.misses < GRACE_MISSES && !self.devices.is_empty()
    }

    /// How long since a device was last actually seen.
    pub fn since_seen(&self) -> Option<Duration> {
        self.last_success.map(|t| t.elapsed())
    }

    /// True before the first poll has come back, so the UI can say "looking" rather than
    /// "nothing there".
    pub fn still_looking(&self) -> bool {
        !self.polled_at_least_once
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn first_ready(&self) -> Option<&Device> {
        self.devices().iter().find(|d| d.state == State::Ready)
    }
}

/// Polls for devices on a background thread until dropped.
pub struct DeviceWatcher {
    shared: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    poke: Arc<AtomicBool>,
}

impl DeviceWatcher {
    pub fn start(adb_path: PathBuf) -> DeviceWatcher {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let poke = Arc::new(AtomicBool::new(false));

        let w = (shared.clone(), stop.clone(), poke.clone());
        thread::spawn(move || {
            let (shared, stop, poke) = w;
            let adb = Adb::at(&adb_path);
            while !stop.load(Ordering::Relaxed) {
                let result = adb.devices();
                let mut snap = shared.lock().unwrap_or_else(|e| e.into_inner());
                snap.polled_at_least_once = true;
                match result {
                    Ok(list) if !list.is_empty() => {
                        snap.devices = list;
                        snap.misses = 0;
                        snap.last_success = Some(Instant::now());
                        snap.last_error = None;
                    }
                    Ok(_) => {
                        // A clean answer of "nothing attached" is still a miss: on a bad
                        // port that is exactly what a blip looks like.
                        snap.misses = snap.misses.saturating_add(1);
                        snap.last_error = None;
                    }
                    Err(e) => {
                        snap.misses = snap.misses.saturating_add(1);
                        snap.last_error = Some(e.to_string());
                    }
                }
                let present = !snap.devices().is_empty();
                drop(snap);

                let interval = if present { POLL_PRESENT } else { POLL_ABSENT };
                // Slept in slices so a stop or a poke is acted on promptly rather than
                // after a full interval.
                let deadline = Instant::now() + interval;
                while Instant::now() < deadline {
                    if stop.load(Ordering::Relaxed) || poke.swap(false, Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });

        DeviceWatcher { shared, stop, poke }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Asks for a poll now rather than at the next interval.
    pub fn poke(&self) {
        self.poke.store(true, Ordering::Relaxed);
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(serial: &str) -> Device {
        Device { serial: serial.into(), state: State::Ready, model: Some("Quest 3".into()) }
    }

    fn seen(devices: Vec<Device>, misses: u32) -> Snapshot {
        Snapshot {
            devices,
            misses,
            last_success: Some(Instant::now()),
            last_error: None,
            polled_at_least_once: true,
        }
    }

    /// The behaviour that matters on a bad cable: one missed poll does not make the headset
    /// disappear from the screen.
    #[test]
    fn a_single_miss_does_not_drop_the_device() {
        let s = seen(vec![device("A")], 1);
        assert_eq!(s.devices().len(), 1, "a blip should not clear the list");
        assert!(s.unstable(), "but it should be reported as unstable");
    }

    #[test]
    fn a_device_is_dropped_once_the_grace_runs_out() {
        let s = seen(vec![device("A")], GRACE_MISSES);
        assert!(s.devices().is_empty());
        assert!(!s.unstable(), "gone is gone, not unstable");
    }

    #[test]
    fn a_steady_device_is_not_reported_as_unstable() {
        let s = seen(vec![device("A")], 0);
        assert_eq!(s.devices().len(), 1);
        assert!(!s.unstable());
    }

    /// Before the first poll returns, the UI should say it is looking rather than that
    /// nothing is there.
    #[test]
    fn starts_out_undecided() {
        let s = Snapshot::default();
        assert!(s.still_looking());
        assert!(s.devices().is_empty());
        assert!(!s.unstable());
        assert!(s.since_seen().is_none());
    }

    #[test]
    fn finds_the_first_ready_device_and_ignores_an_unauthorised_one() {
        let mut waiting = device("B");
        waiting.state = State::Unauthorized;
        let s = seen(vec![waiting, device("A")], 0);
        assert_eq!(s.first_ready().unwrap().serial, "A");

        let mut only_waiting = device("C");
        only_waiting.state = State::Unauthorized;
        assert!(seen(vec![only_waiting], 0).first_ready().is_none());
    }

    /// The watcher must never leave a thread running after the screen that owns it is gone.
    #[test]
    fn dropping_the_watcher_stops_the_thread() {
        let w = DeviceWatcher::start(PathBuf::from("/definitely/not/adb"));
        let stop = w.stop.clone();
        assert!(!stop.load(Ordering::Relaxed));
        drop(w);
        assert!(stop.load(Ordering::Relaxed));
    }

    /// A watcher pointed at nothing must fail quietly and keep saying so, not panic or
    /// pretend a device is there.
    #[test]
    fn a_missing_adb_is_reported_not_fatal() {
        let w = DeviceWatcher::start(PathBuf::from("/definitely/not/adb"));
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && w.snapshot().still_looking() {
            thread::sleep(Duration::from_millis(50));
        }
        let s = w.snapshot();
        assert!(!s.still_looking(), "the first poll should have completed");
        assert!(s.devices().is_empty());
        assert!(s.last_error().is_some(), "the reason should be available to show");
    }
}
