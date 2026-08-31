// SPDX-License-Identifier: GPL-3.0-or-later
//! Turning numbers into things people read.
//!
//! Deliberately neutral: both the engine (for error messages) and the flows (for progress)
//! need this, and the engine must not depend on the UI to get it.

/// Sizes in the units people actually read them in.
pub fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

/// Compact remaining-time, for an ETA that has to fit beside a progress bar.
pub fn human_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// How long ago something was, counted in days, in words a person would use.
///
/// Days all the way up was the first version and it does not survive contact with a check
/// that has been failing for a while: "1 days ago" is wrong on the second day, and by the
/// time it reads "912 days ago" the number has stopped carrying meaning. Which matters
/// precisely because a large number here is the signal that something has been broken for a
/// long time, so it is the moment the line most needs to be readable.
pub fn days_ago(days: u64) -> String {
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        // Up to a month, days are still the unit somebody thinks in.
        2..=30 => format!("{days} days ago"),
        31..=364 => {
            let months = (days / 30).max(1);
            if months == 1 {
                "last month".to_string()
            } else {
                format!("{months} months ago")
            }
        }
        _ => {
            let years = days / 365;
            if years == 1 {
                "over a year ago".to_string()
            } else {
                format!("over {years} years ago")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn says_how_long_ago_without_mangling_the_grammar() {
        assert_eq!(days_ago(0), "today");
        assert_eq!(days_ago(1), "yesterday", "\"1 days ago\" showed up on day two");
        assert_eq!(days_ago(2), "2 days ago");
        assert_eq!(days_ago(30), "30 days ago");
        assert_eq!(days_ago(31), "last month");
        assert_eq!(days_ago(90), "3 months ago");
        assert_eq!(days_ago(364), "12 months ago");
        assert_eq!(days_ago(365), "over a year ago");
        assert_eq!(days_ago(900), "over 2 years ago");
        // Centuries are not a real case, but a number that runs off the end of the line is
        // not a good reason for the sentence to stop making sense.
        assert_eq!(days_ago(40_000), "over 109 years ago");
    }

    #[test]
    fn formats_sizes_the_way_people_read_them() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024 + 500_000), "3.5 MB");
        // The number that matters: the client archive.
        assert_eq!(human_bytes(5_024_528_313), "4.68 GB");
    }

    #[test]
    fn formats_durations_compactly() {
        assert_eq!(human_duration(Duration::from_secs(45)), "45s");
        assert_eq!(human_duration(Duration::from_secs(161)), "2m 41s");
        assert_eq!(human_duration(Duration::from_secs(3 * 3600 + 240)), "3h 04m");
    }
}

/// The one-line description of a transfer in progress: how much, how fast, how long left,
/// and whether this is a retry.
///
/// Here rather than in a flow because the window and the terminal were formatting the same
/// four facts in two places, and the retry then only reached one of them.
pub fn transfer(s: &crate::engine::download::Snapshot) -> String {
    let mut parts = Vec::new();
    match s.total {
        Some(total) => parts.push(format!("{} / {}", human_bytes(s.done), human_bytes(total))),
        None => parts.push(human_bytes(s.done)),
    }
    if s.bytes_per_sec > 0.0 {
        parts.push(format!("{}/s", human_bytes(s.bytes_per_sec as u64)));
    }
    if let Some(eta) = s.eta() {
        parts.push(format!("{} left", human_duration(eta)));
    }
    if s.attempt > 0 {
        // Otherwise a dropped connection looks like a stall: the bar stops moving and
        // nothing says why.
        parts.push(format!("retry {}/{}", s.attempt, crate::engine::download::RETRIES));
    }
    // Plain ASCII: this line is read in a terminal as often as in a window.
    parts.join("  -  ")
}

/// A sha256 with its middle taken out. Nobody reads 64 characters; the ends are what
/// differ between two of them.
///
/// Anything that is not a full-length hash is returned untouched, because a short string
/// here means something already went wrong and hiding it would not help.
pub fn short_hash(hash: &str) -> String {
    if hash.len() == 64 {
        format!("{}...{}", &hash[..8], &hash[56..])
    } else {
        hash.to_string()
    }
}

/// A Windows path, written the way Windows writes it.
///
/// `Path::join` uses the separator of the machine it runs on, so a path built from a
/// Windows base on any other system comes out as `C:\\Program Files\\Meta Horizon/Software`.
/// These paths are only ever read on Windows - and pasted into Explorer - so the separator
/// is a display decision, not a platform one.
pub fn windows_path(path: &std::path::Path) -> String {
    path.display().to_string().replace('/', "\\")
}

/// Howard Hinnant's days-to-civil algorithm.
///
/// Here rather than in an engine module because three unrelated things need a date and none
/// of them justifies pulling in a date library for it.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Seconds since the epoch, right now. One place, so nothing has to remember what to do
/// about a clock set before 1970.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `20260829-014723Z`. For filenames: sorts chronologically, has no characters a shell or
/// a filesystem objects to, and the Z says which clock it came from.
pub fn utc_stamp(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}Z", tod / 3600, (tod % 3600) / 60, tod % 60)
}

/// `01:47:23` - time of day only, for lines inside a file whose name already carries the
/// date.
pub fn utc_clock(secs: u64) -> String {
    let tod = secs % 86_400;
    format!("{:02}:{:02}:{:02}", tod / 3600, (tod % 3600) / 60, tod % 60)
}

#[cfg(test)]
mod date_tests {
    use super::*;

    #[test]
    fn windows_paths_are_written_with_backslashes() {
        // Built on any machine, read on Windows. A mixed separator is not wrong to the
        // filesystem and looks broken to the person told to go and delete it.
        let p = std::path::Path::new(r"C:\Program Files\Meta Horizon")
            .join("Software")
            .join("ready-at-dawn-echo-arena");
        let shown = windows_path(&p);
        assert!(!shown.contains('/'), "got {shown}");
        assert!(shown.starts_with(r"C:\Program Files\Meta Horizon\Software"), "got {shown}");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn utc_stamp_sorts_and_is_filename_safe() {
        let a = utc_stamp(1_700_000_000);
        let b = utc_stamp(1_700_000_001);
        assert!(a < b, "{a} !< {b}");
        assert_eq!(a.len(), 16, "got {a}");
        assert!(a.ends_with('Z'));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "got {a}");
    }

    #[test]
    fn utc_clock_is_time_of_day() {
        assert_eq!(utc_clock(0), "00:00:00");
        assert_eq!(utc_clock(86_399), "23:59:59");
    }
}
