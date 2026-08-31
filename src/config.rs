// SPDX-License-Identifier: GPL-3.0-or-later
//! Settings that outlive a run, and where they live.
//!
//! A stable, user-owned, no-admin location, unlike the original which writes its log into
//! whatever directory it happened to be launched from. Plain `key=value` lines rather than
//! a serialisation format: it keeps a dependency out of the tree, and it means someone
//! debugging an install can read and edit the file without tooling.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const FILE: &str = "settings.conf";

/// Environment override for the whole app directory.
///
/// Two uses, one of them a real feature. It makes the app **portable**: point it at a
/// folder beside the executable and settings, logs and the managed adb travel with it on a
/// USB stick, leaving nothing on the host. And it keeps test runs out of a developer's real
/// profile.
pub const HOME_ENV: &str = "ECHO_VRCE_HOME";

/// Root for everything this app keeps between runs: settings, logs, and the adb it
/// manages. Never needs administrator rights.
pub fn dir() -> PathBuf {
    dir_from(std::env::var_os(HOME_ENV))
}

/// The logic behind [`dir`], with the environment passed in.
///
/// Split out so it can be tested without mutating a process-global: cargo runs tests in
/// parallel, and a test that sets an environment variable is a test that breaks a different
/// one at random.
fn dir_from(override_value: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(base) = override_value {
        if !base.is_empty() {
            return PathBuf::from(base);
        }
    }
    #[cfg(windows)]
    {
        // LOCALAPPDATA rather than APPDATA: this is machine-local state, not something
        // worth syncing to a roaming profile.
        if let Ok(base) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(base).join("EchoVRCE");
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(base) = std::env::var("XDG_CONFIG_HOME") {
            return PathBuf::from(base).join("echo-vrce");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".config").join("echo-vrce");
        }
    }
    std::env::temp_dir().join("echo-vrce")
}

/// Where a managed copy of adb is unpacked, if the user asks us to fetch one.
pub fn tools_dir() -> PathBuf {
    dir().join("tools")
}

pub fn logs_dir() -> PathBuf {
    dir().join("logs")
}

/// Remembers where the user last installed, and hands it back next time.
///
/// The field existed and was serialised from the start, but nothing ever wrote it, so the
/// folder box always offered the same hardcoded guess and the cache cleaner had no idea
/// where the PC archive might have been left.
pub fn remember_install_path(path: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    let mut s = Settings::load();
    if s.install_path.as_deref() == Some(path) {
        return;
    }
    s.install_path = Some(path.to_string());
    s.save();
}

/// Where the folder box should start, and why.
///
/// Precedence, and the order matters: a folder the user has already used beats anything
/// deduced, because it is a decision rather than a guess. Only when there is none does
/// detection get a say, and only when that finds nothing does the neutral fallback appear.
/// For updating: the folder Echo is actually in. A folder without it has nothing to update.
pub fn suggested_update_path(fallback: impl FnOnce() -> String) -> (String, Option<&'static str>) {
    let detected = crate::engine::meta::echo_root()
        .map(|d| (d.root.display().to_string(), d.source.describe()));
    choose_suggestion(Settings::load().install_path, detected, fallback)
}

/// For installing: the folder Echo belongs in, which is a different question.
///
/// Whether the game is there is the wrong test here. Someone installing either does not own
/// it - so it will never be in a Meta library - or has just been told to delete Meta's copy
/// first. Both make the careful-looking check fail every single time.
pub fn suggested_install_path(fallback: impl FnOnce() -> String) -> (String, Option<&'static str>) {
    let detected = crate::engine::meta::library_root()
        .map(|d| (d.root.display().to_string(), d.source.describe_library()));
    choose_suggestion(Settings::load().install_path, detected, fallback)
}

/// The precedence rule on its own, with the machine taken out of it.
///
/// Split out so the ordering can be tested without a registry, an installed game, or a
/// settings file: cargo runs tests in parallel, and a test that leans on process-wide state
/// is a test that breaks a different one at random.
fn choose_suggestion(
    remembered: Option<String>,
    detected: Option<(String, &'static str)>,
    fallback: impl FnOnce() -> String,
) -> (String, Option<&'static str>) {
    if let Some(p) = remembered.filter(|p| !p.trim().is_empty()) {
        return (p, Some("the folder you used last time"));
    }
    if let Some((path, why)) = detected {
        return (path, Some(why));
    }
    (fallback(), None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Explicit adb chosen by the user. Takes priority over everything found automatically.
    pub adb_path: Option<PathBuf>,
    /// Explicit Revive folder chosen by the user. Same rule: a choice beats a search.
    pub revive_path: Option<PathBuf>,
    /// Last install root used, so the field is not empty next time.
    pub install_path: Option<String>,
    /// Whether to ask GitHub at startup whether a newer installer exists.
    ///
    /// On by default and switchable off, with what it sends spelled out in About. It is one
    /// unauthenticated GET of a public file and carries nothing identifying, but it is still
    /// an outbound request nobody asked for, and an audience that sideloads unsigned
    /// executables is entitled to know about it.
    pub update_check: bool,
    /// Unix seconds of the last check that actually succeeded. Absent means never.
    ///
    /// A failed attempt deliberately does not touch this: what the app reports is how long
    /// it has been since it last knew something, not how long since it last tried. One
    /// dropped connection means nothing; a week of them means a firewall.
    pub update_checked_at: Option<u64>,
    /// The newest version the last successful check saw.
    ///
    /// Remembered rather than recomputed because the check runs at most once a day: without
    /// this, an app restarted two hours after finding an update forgets about it and says
    /// nothing until tomorrow. It also lets the command line report the same thing without
    /// making a network request of its own, which a script calling --version would not
    /// thank anyone for.
    pub update_latest_seen: Option<String>,
}

/// Written out rather than derived, because a derived `bool` is `false` and `load` falls
/// back to this when there is no settings file yet. Deriving it would have turned the
/// update check off on precisely the installs that have never been configured.
impl Default for Settings {
    fn default() -> Self {
        Settings {
            adb_path: None,
            revive_path: None,
            install_path: None,
            update_check: true,
            update_checked_at: None,
            update_latest_seen: None,
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        let path = dir().join(FILE);
        let Ok(text) = fs::read_to_string(path) else {
            return Settings::default();
        };
        Settings::load_from(&text)
    }

    /// The parsing half of [`load`], with the file read out of it, so a test can prove a
    /// round trip without writing to the real settings directory.
    fn load_from(text: &str) -> Settings {
        let map = parse(text);
        Settings {
            adb_path: map.get("adb_path").filter(|v| !v.is_empty()).map(PathBuf::from),
            install_path: map.get("install_path").filter(|v| !v.is_empty()).cloned(),
            revive_path: map
                .get("revive_path")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            // Absent means on. A setting whose default changes when the file is missing is
            // a setting that behaves differently on a fresh install than on an upgrade.
            update_check: map.get("update_check").map(|v| v != "false").unwrap_or(true),
            update_checked_at: map.get("update_checked_at").and_then(|v| v.parse().ok()),
            update_latest_seen: map
                .get("update_latest_seen")
                .filter(|v| !v.is_empty())
                .cloned(),
        }
    }

    /// Best effort. Losing a preference is not worth interrupting anyone over, so failures
    /// are reported to the log rather than to the user.
    /// The file's contents. Split from `save` so a test can check that every field survives
    /// a round trip without writing to the real settings directory.
    fn serialise(&self) -> String {
        let mut text = String::from("# Echo VRCE Installer settings\n");
        if let Some(p) = &self.adb_path {
            text.push_str(&format!("adb_path={}\n", sanitise(&p.to_string_lossy())));
        }
        if let Some(p) = &self.install_path {
            text.push_str(&format!("install_path={}\n", sanitise(p)));
        }
        if let Some(p) = &self.revive_path {
            text.push_str(&format!("revive_path={}\n", sanitise(&p.to_string_lossy())));
        }
        if !self.update_check {
            text.push_str("update_check=false\n");
        }
        if let Some(t) = self.update_checked_at {
            text.push_str(&format!("update_checked_at={t}\n"));
        }
        if let Some(v) = &self.update_latest_seen {
            text.push_str(&format!("update_latest_seen={}\n", sanitise(v)));
        }
        text
    }

    pub fn save(&self) {
        let dir = dir();
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("settings: could not create {}: {e}", dir.display());
            return;
        }
        let text = self.serialise();
        let file = dir.join(FILE);
        if let Err(e) = write_atomic(&file, &text) {
            eprintln!("settings: could not write {}: {e}", file.display());
        }
    }
}

/// Temp file then rename, so an interrupted write cannot leave a half-parsed settings file
/// behind.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

/// Values cannot contain newlines, or the file stops being parseable as lines.
fn sanitise(value: &str) -> String {
    value.replace(['\r', '\n'], "")
}

fn parse(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remembered_folder_beats_anything_detected() {
        // A path the user has already used is a decision. Detection is a deduction. The
        // decision wins, or the app is overruling them with a guess.
        let (path, why) = choose_suggestion(
            Some(r"D:\Games\Echo VR".into()),
            Some((r"C:\Program Files\Meta Horizon\Software\Software".into(), "detected")),
            || "fallback".into(),
        );
        assert_eq!(path, r"D:\Games\Echo VR");
        assert_eq!(why, Some("the folder you used last time"));
    }

    #[test]
    fn detection_is_used_when_there_is_nothing_remembered() {
        let (path, why) = choose_suggestion(
            None,
            Some((r"C:\Program Files\Meta Horizon\Software\Software".into(), "detected")),
            || "fallback".into(),
        );
        assert_eq!(path, r"C:\Program Files\Meta Horizon\Software\Software");
        assert_eq!(why, Some("detected"));
    }

    #[test]
    fn the_fallback_carries_no_explanation() {
        // Nothing was found, so there is nothing to explain. A note here would be the app
        // dressing a guess up as a finding.
        let (path, why) = choose_suggestion(None, None, || "C:\\EchoVR".into());
        assert_eq!(path, "C:\\EchoVR");
        assert_eq!(why, None);
    }

    #[test]
    fn a_blank_remembered_path_is_not_a_memory() {
        let (path, _) = choose_suggestion(Some("   ".into()), None, || "fallback".into());
        assert_eq!(path, "fallback");
    }

    #[test]
    fn parses_key_value_lines_and_ignores_noise() {
        let map = parse("# a comment\n\n  adb_path = /usr/bin/adb  \nbroken line\ninstall_path=C:\\Echo\n");
        assert_eq!(map.get("adb_path").unwrap(), "/usr/bin/adb");
        assert_eq!(map.get("install_path").unwrap(), "C:\\Echo");
        assert_eq!(map.len(), 2, "a line with no '=' must be skipped, not guessed at");
    }

    #[test]
    fn strips_newlines_from_values() {
        assert_eq!(sanitise("a\nb\r\nc"), "abc");
    }

    /// The override has to win outright, or portable mode is a lie.
    #[test]
    fn home_override_wins() {
        let chosen = dir_from(Some("/tmp/evrce_portable_probe".into()));
        assert_eq!(chosen, PathBuf::from("/tmp/evrce_portable_probe"));
    }

    /// An empty value is treated as unset rather than as "the root directory".
    #[test]
    fn an_empty_override_is_ignored() {
        assert_ne!(dir_from(Some("".into())), PathBuf::from(""));
        assert_eq!(dir_from(Some("".into())), dir_from(None));
    }

    #[test]
    fn config_dir_is_absolute_and_named() {
        let d = dir_from(None);
        assert!(d.is_absolute(), "got {}", d.display());
        assert!(
            d.to_string_lossy().to_lowercase().contains("echo-vrce")
                || d.to_string_lossy().contains("EchoVRCE")
        );
    }

    /// Round trip through a real file, in an isolated directory.
    #[test]
    fn round_trips_through_disk() {
        let tmp = std::env::temp_dir().join(format!("evrce_cfg_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let file = tmp.join(FILE);
        let s = Settings {
            adb_path: Some(PathBuf::from("/opt/adb")),
            install_path: Some("C:\\EchoVR".into()),
            revive_path: Some(PathBuf::from("D:\\Revive")),
            update_check: false,
            update_checked_at: Some(1_756_000_000),
            update_latest_seen: Some("0.9.9".into()),
        };
        // Written through the real serialiser, so a field added to Settings and forgotten
        // in save() fails here rather than going quietly missing on the next launch.
        write_atomic(&file, &s.serialise()).unwrap();

        let map = parse(&fs::read_to_string(&file).unwrap());
        assert_eq!(map.get("adb_path").unwrap(), "/opt/adb");
        assert_eq!(map.get("install_path").unwrap(), "C:\\EchoVR");
        assert_eq!(map.get("revive_path").unwrap(), "D:\\Revive");
        assert_eq!(map.get("update_check").unwrap(), "false");
        assert_eq!(map.get("update_checked_at").unwrap(), "1756000000");
        assert_eq!(map.get("update_latest_seen").unwrap(), "0.9.9");
        assert_eq!(Settings::load_from(&s.serialise()), s, "a field must survive a round trip");
        assert!(!file.with_extension("tmp").exists(), "temp file was left behind");

        fs::remove_dir_all(tmp).ok();
    }
}
