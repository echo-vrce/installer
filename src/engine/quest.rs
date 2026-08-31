// SPDX-License-Identifier: GPL-3.0-or-later
//! Quest-side operations, over adb.
//!
//! Two things here are load-bearing and neither is obvious from the original.
//!
//! **The install marker is an interop format, not ours.** Three independent installers now
//! write `.echo_installer_version` at the manifest target root with the same keys. If this
//! one deviates, its update flow refuses installs made by the other two and theirs refuse
//! ours. The shape below is fixed; DOCS.md has it under "The install marker, which is an
//! interop standard".
//!
//! **Every device command carries `-s <serial>` when a serial is known.** The original
//! never does, so the moment a second Android device is attached, adb refuses everything
//! with "more than one device" and the installer reports it as a missing headset.

use std::path::Path;

use crate::engine::adb::{Adb, Device};

pub const PACKAGE: &str = "com.readyatdawn.r15";
/// Where game data lives. App-owned external media, so it needs no storage permission and
/// works on secondary Quest accounts, unlike the `/sdcard/readyatdawn` the original used to
/// use. Android also wipes it on uninstall, which is what makes a marker here trustworthy:
/// it can never outlive the install it describes.
pub const MEDIA_ROOT: &str = "/sdcard/Android/media/com.readyatdawn.r15";
pub const MARKER_NAME: &str = ".echo_installer_version";

/// The activity that starts the game.
///
/// Named explicitly rather than resolved. Echo's APK files its MAIN activity under
/// `android.intent.category.INFO` and carries no `LAUNCHER` category, which is unusual
/// enough that intent resolution cannot be relied on to find it. Starting it by component
/// name is unambiguous and was confirmed working on a Quest 2.
pub const LAUNCH_ACTIVITY: &str = "com.readyatdawn.r15/com.oculus.gles3jni.MainActivity";

pub fn marker_path() -> String {
    format!("{MEDIA_ROOT}/{MARKER_NAME}")
}

/// On-device record of which base version an install came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marker {
    pub base_apk: String,
    pub base_sha256: String,
    /// SHA-256 of the APK actually installed. Differs from `base_sha256` when the APK was
    /// personalised, which is why the base hash cannot simply be re-derived from the
    /// device.
    pub installed_sha256: String,
    pub patched: bool,
    pub installed_at: String,
    pub installer_version: String,
}

impl Marker {
    /// Tolerant on purpose: any line without `=` is ignored, so adb chatter mixed into the
    /// output of `cat` cannot make a valid marker unreadable.
    pub fn parse(text: &str) -> Option<Marker> {
        let mut m = Marker::default();
        let mut saw_any = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "base_apk" => {
                    m.base_apk = value;
                    saw_any = true;
                }
                "base_sha256" => {
                    m.base_sha256 = value;
                    saw_any = true;
                }
                "installed_sha256" => {
                    m.installed_sha256 = value;
                    saw_any = true;
                }
                "patched" => {
                    m.patched = value.eq_ignore_ascii_case("true");
                    saw_any = true;
                }
                "installed_at" => m.installed_at = value,
                "installer_version" => m.installer_version = value,
                _ => {}
            }
        }
        saw_any.then_some(m)
    }

    /// Key order matters as little as it ever does, but staying byte-identical to what the
    /// other installers write makes a diff between two markers readable.
    pub fn serialize(&self) -> String {
        format!(
            "version=1\nbase_apk={}\nbase_sha256={}\ninstalled_sha256={}\npatched={}\n\
             installed_at={}\ninstaller_version={}\n",
            strip(&self.base_apk),
            strip(&self.base_sha256),
            strip(&self.installed_sha256),
            self.patched,
            strip(&self.installed_at),
            strip(&self.installer_version),
        )
    }
}

fn strip(v: &str) -> String {
    v.replace(['\r', '\n'], "")
}

/// Whether an update may be applied, and why not when it may not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    NotInstalled,
    /// Carries the sentence shown to the user.
    Mismatch(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    /// The install is recognisably the stock base build but has no marker, so one should be
    /// written before updating. Happens to anyone who installed with an older build.
    pub self_heal: bool,
}

/// Decides whether an update may proceed. Pure: no device access, so the whole table is
/// testable.
///
/// The reason this is not simply "does the installed APK hash match the manifest" is that a
/// personalised APK is repacked, so its hash can never equal the manifest's base hash. The
/// marker records which base an install corresponds to; without it, only a stock install
/// can be recognised.
pub fn decide(
    manifest_base_sha: Option<&str>,
    marker: Option<&Marker>,
    installed: bool,
    installed_sha: Option<&str>,
) -> Decision {
    let plain = |verdict| Decision { verdict, self_heal: false };

    if !installed {
        return plain(Verdict::NotInstalled);
    }
    // A manifest with no BASE_APK header has nothing to gate on.
    let Some(base) = manifest_base_sha else {
        return plain(Verdict::Ok);
    };

    if let Some(marker) = marker.filter(|m| !m.base_sha256.is_empty()) {
        if !base.eq_ignore_ascii_case(&marker.base_sha256) {
            let from = if marker.base_apk.is_empty() {
                String::new()
            } else {
                format!(" (installed from {})", marker.base_apk)
            };
            return plain(Verdict::Mismatch(format!(
                "The Echo VR version on your Quest is older than this update{from}."
            )));
        }
        // The marker agrees about the base, but the APK on the device is not the one the
        // marker was written for: something replaced it since.
        if let (Some(actual), false) = (installed_sha, marker.installed_sha256.is_empty()) {
            if !actual.eq_ignore_ascii_case(&marker.installed_sha256) {
                return plain(Verdict::Mismatch(
                    "The Echo VR app on your Quest was replaced since it was installed.".into(),
                ));
            }
        }
        return plain(Verdict::Ok);
    }

    // No marker: only a stock base APK can be recognised, and the marker is back-filled.
    match installed_sha {
        Some(sha) if base.eq_ignore_ascii_case(sha) => {
            Decision { verdict: Verdict::Ok, self_heal: true }
        }
        _ => plain(Verdict::Mismatch(
            "The Echo VR version installed on your Quest could not be matched to this update."
                .into(),
        )),
    }
}

/// A device, addressed explicitly.
pub struct Quest<'a> {
    adb: &'a Adb,
    serial: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    Adb(crate::engine::adb::Error),
    /// adb reported success but the device did not end up in the expected state.
    Unexpected(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Adb(e) => write!(f, "{e}"),
            Error::Unexpected(m) => write!(f, "{m}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::engine::adb::Error> for Error {
    fn from(e: crate::engine::adb::Error) -> Self {
        Error::Adb(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl<'a> Quest<'a> {
    pub fn new(adb: &'a Adb, device: Option<&Device>) -> Quest<'a> {
        Quest { adb, serial: device.map(|d| d.serial.clone()) }
    }

    /// Prefixes `-s <serial>` when one is known, so a second attached device cannot make
    /// every command ambiguous.
    fn args<'b>(&'b self, rest: &[&'b str]) -> Vec<&'b str> {
        let mut out = Vec::with_capacity(rest.len() + 2);
        if let Some(serial) = &self.serial {
            out.push("-s");
            out.push(serial.as_str());
        }
        out.extend_from_slice(rest);
        out
    }

    pub fn exec(&self, rest: &[&str]) -> Result<String, Error> {
        Ok(self.adb.exec(&self.args(rest))?)
    }

    /// Runs a script on the device. The whole script is one argv element, so only the
    /// device's own shell ever splits it and the host shell never sees it at all.
    pub fn shell(&self, script: &str) -> Result<String, Error> {
        self.exec(&["shell", script])
    }

    /// Absolute path of the installed base APK, or None when the package is absent.
    ///
    /// Keys off the presence of a `package:` line rather than the exit code: builds
    /// disagree about whether "not installed" is a failure or an empty success.
    pub fn installed_apk_path(&self) -> Option<String> {
        let out = self.exec(&["shell", "pm", "path", PACKAGE]).ok()?;
        parse_pm_path(&out)
    }

    pub fn is_installed(&self) -> bool {
        self.installed_apk_path().is_some()
    }

    /// versionCode of the installed package, used to confirm that what got installed is
    /// what was handed to adb.
    pub fn version_code(&self) -> Option<i64> {
        let out = self.exec(&["shell", "dumpsys", "package", PACKAGE]).ok()?;
        parse_version_code(&out)
    }

    /// SHA-256 of the installed APK, hashed on the device.
    pub fn installed_sha(&self) -> Option<String> {
        let path = self.installed_apk_path()?;
        let out = self.exec(&["shell", "sha256sum", &path]).ok()?;
        first_hash(&out)
    }

    pub fn read_marker(&self) -> Option<Marker> {
        let out = self.exec(&["shell", "cat", &marker_path()]).ok()?;
        Marker::parse(&out)
    }

    /// Writes the marker by pushing a local file.
    ///
    /// Deliberately not `adb shell "echo ... > file"`: shell redirection is fragile across
    /// platforms, and push is the transfer the install flow already depends on.
    pub fn write_marker(&self, marker: &Marker) -> Result<(), Error> {
        let temp = std::env::temp_dir().join(format!("evrce_marker_{}", std::process::id()));
        std::fs::write(&temp, marker.serialize())?;
        let result = (|| {
            self.shell(&format!("mkdir -p {MEDIA_ROOT}"))?;
            self.push(&temp, &marker_path())
        })();
        let _ = std::fs::remove_file(&temp);
        result
    }

    /// Pushes to an absolute remote **file** path.
    ///
    /// Always the full destination filename, never a directory: local staging files carry
    /// temp names, and pushing to a directory would land them under the wrong one.
    pub fn push(&self, local: &Path, remote: &str) -> Result<(), Error> {
        let local = local.to_string_lossy().into_owned();
        let out = self.exec(&["push", &local, remote])?;
        if transferred(&out) {
            Ok(())
        } else {
            Err(Error::Unexpected(format!("push reported no transfer: {}", out.trim())))
        }
    }

    /// Starts Echo VR on the headset.
    pub fn launch(&self) -> Result<(), Error> {
        let out = self.exec(&["shell", "am", "start", "-n", LAUNCH_ACTIVITY])?;
        // `am start` reports its refusals on stdout with a zero exit code, so the text is
        // the only signal there is.
        if out.contains("Error") || out.contains("does not exist") {
            Err(Error::Unexpected(format!("could not start Echo VR: {}", out.trim())))
        } else {
            Ok(())
        }
    }

    pub fn uninstall(&self) -> Result<(), Error> {
        // Failure is fine and expected on a first install; the caller decides.
        let _ = self.exec(&["uninstall", PACKAGE]);
        Ok(())
    }

    /// `-g` grants the manifest's runtime permissions up front, sparing the user a series
    /// of prompts inside the headset.
    pub fn install_apk(&self, local: &Path) -> Result<(), Error> {
        let local = local.to_string_lossy().into_owned();
        let out = self.exec(&["install", "-g", &local])?;
        if out.contains("Success") {
            Ok(())
        } else {
            Err(Error::Unexpected(format!("install did not report success: {}", out.trim())))
        }
    }
}

/// `package:/data/app/~~abc==/com.readyatdawn.r15-1/base.apk`
pub fn parse_pm_path(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("package:")
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
    })
}

/// `dumpsys package` prints `versionCode=12345 minSdk=...` among a great deal else.
pub fn parse_version_code(output: &str) -> Option<i64> {
    output.split_whitespace().find_map(|token| {
        token
            .strip_prefix("versionCode=")
            .and_then(|v| v.parse::<i64>().ok())
    })
}

/// First 64-character hex token in the output, which is what `sha256sum` leads with.
pub fn first_hash(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|token| {
        (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| token.to_ascii_lowercase())
    })
}

/// Parses batched `sha256sum a b c` output into path -> hash. Lines for missing files, and
/// any error noise, are simply absent from the result.
pub fn parse_hash_listing(output: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for line in output.lines() {
        let line = line.trim();
        let Some((hash, path)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            out.insert(path.trim().to_string(), hash.to_ascii_lowercase());
        }
    }
    out
}

/// adb push prints a summary line; anything else means nothing moved.
fn transferred(output: &str) -> bool {
    output.contains("bytes") && !output.contains("0 files pushed")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "0a7fa5f9cfc173013e152a75fac2ded7ca4f66b8d8530f598c0c2530b5cf0973";
    const OTHER: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const PATCHED: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn marker(base: &str, installed: &str, patched: bool) -> Marker {
        Marker {
            base_apk: "echo_quest_27-08-2026.001.apk".into(),
            base_sha256: base.into(),
            installed_sha256: installed.into(),
            patched,
            installed_at: "2026-08-29T10:00:00Z".into(),
            installer_version: "0.1.0".into(),
        }
    }

    // ---------------------------------------------------------------- marker format

    /// The exact bytes the other two installers write. Deviating here means this app's
    /// update flow refuses their installs and theirs refuses ours.
    #[test]
    fn marker_serialises_to_the_interop_format() {
        let text = marker(BASE, PATCHED, true).serialize();
        let keys: Vec<&str> = text
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
            .collect();
        assert_eq!(
            keys,
            vec![
                "version",
                "base_apk",
                "base_sha256",
                "installed_sha256",
                "patched",
                "installed_at",
                "installer_version"
            ]
        );
        assert!(text.starts_with("version=1\n"));
        assert!(text.contains("patched=true\n"), "booleans are lowercase in this format");
    }

    #[test]
    fn marker_round_trips() {
        let original = marker(BASE, PATCHED, true);
        let parsed = Marker::parse(&original.serialize()).unwrap();
        assert_eq!(parsed, original);
    }

    /// `adb shell cat` output can arrive with a stray line. A valid marker must survive it.
    #[test]
    fn marker_parsing_tolerates_noise() {
        let text = format!(
            "* daemon started\n# a comment\nnot a pair\nbase_sha256={BASE}\npatched=TRUE\n"
        );
        let m = Marker::parse(&text).unwrap();
        assert_eq!(m.base_sha256, BASE);
        assert!(m.patched, "the value should be read case-insensitively");
    }

    #[test]
    fn marker_parsing_rejects_nothing_useful() {
        assert!(Marker::parse("").is_none());
        assert!(Marker::parse("no pairs here at all\n").is_none());
        // A file of only comments is not a marker either.
        assert!(Marker::parse("# just a comment\n").is_none());
    }

    #[test]
    fn marker_values_cannot_smuggle_a_newline() {
        let mut m = marker(BASE, PATCHED, false);
        m.base_apk = "evil.apk\npatched=true".into();
        let text = m.serialize();
        assert_eq!(
            text.lines().filter(|l| l.starts_with("patched=")).count(),
            1,
            "a newline in a value must not be able to forge another field"
        );
    }

    // ---------------------------------------------------------------- the version gate

    #[test]
    fn refuses_when_nothing_is_installed() {
        let d = decide(Some(BASE), None, false, None);
        assert_eq!(d.verdict, Verdict::NotInstalled);
    }

    #[test]
    fn allows_when_the_marker_agrees_about_both_hashes() {
        let m = marker(BASE, PATCHED, true);
        let d = decide(Some(BASE), Some(&m), true, Some(PATCHED));
        assert_eq!(d.verdict, Verdict::Ok);
        assert!(!d.self_heal);
    }

    /// The case the whole marker exists for: a personalised APK cannot match the manifest's
    /// base hash, so only the marker can vouch for it.
    #[test]
    fn allows_a_patched_install_whose_hash_can_never_match_the_base() {
        let m = marker(BASE, PATCHED, true);
        assert_ne!(PATCHED, BASE);
        assert_eq!(decide(Some(BASE), Some(&m), true, Some(PATCHED)).verdict, Verdict::Ok);
    }

    #[test]
    fn refuses_when_the_marker_names_an_older_base() {
        let m = marker(OTHER, PATCHED, true);
        match decide(Some(BASE), Some(&m), true, Some(PATCHED)).verdict {
            Verdict::Mismatch(msg) => {
                assert!(msg.contains("older"), "got {msg}");
                assert!(msg.contains("echo_quest_27-08-2026.001.apk"), "should name the build");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn refuses_when_the_apk_was_swapped_since_install() {
        let m = marker(BASE, PATCHED, true);
        match decide(Some(BASE), Some(&m), true, Some(OTHER)).verdict {
            Verdict::Mismatch(msg) => assert!(msg.contains("replaced"), "got {msg}"),
            other => panic!("got {other:?}"),
        }
    }

    /// Installed by an older build that wrote no marker, but it is recognisably stock.
    #[test]
    fn back_fills_a_missing_marker_for_a_stock_install() {
        let d = decide(Some(BASE), None, true, Some(BASE));
        assert_eq!(d.verdict, Verdict::Ok);
        assert!(d.self_heal, "a stock install with no marker should get one written");
    }

    #[test]
    fn refuses_an_unrecognisable_install_with_no_marker() {
        let d = decide(Some(BASE), None, true, Some(OTHER));
        assert!(matches!(d.verdict, Verdict::Mismatch(_)));
        assert!(!d.self_heal, "nothing is known, so nothing should be written");
    }

    /// A manifest with no BASE_APK header has nothing to gate on.
    #[test]
    fn allows_when_the_manifest_declares_no_base() {
        assert_eq!(decide(None, None, true, Some(OTHER)).verdict, Verdict::Ok);
    }

    /// An empty base hash in a marker is not an agreement, it is an absence.
    #[test]
    fn treats_a_blank_marker_hash_as_no_marker() {
        let m = marker("", "", false);
        let d = decide(Some(BASE), Some(&m), true, Some(BASE));
        assert_eq!(d.verdict, Verdict::Ok);
        assert!(d.self_heal);
    }

    // ---------------------------------------------------------------- output parsing

    #[test]
    fn parses_pm_path() {
        let out = "package:/data/app/~~kQ8vX==/com.readyatdawn.r15-1/base.apk\n";
        assert_eq!(
            parse_pm_path(out).as_deref(),
            Some("/data/app/~~kQ8vX==/com.readyatdawn.r15-1/base.apk")
        );
        assert!(parse_pm_path("").is_none());
        // The shape of "not installed" on the builds that answer with a bare success.
        assert!(parse_pm_path("\n").is_none());
    }

    #[test]
    fn parses_version_code_out_of_dumpsys_noise() {
        let out = "  Packages:\n    Package [com.readyatdawn.r15] (abc):\n      \
                   versionCode=4294967 minSdk=29 targetSdk=32\n      versionName=1.0\n";
        assert_eq!(parse_version_code(out), Some(4294967));
        assert_eq!(parse_version_code("no version here"), None);
    }

    #[test]
    fn finds_a_hash_in_sha256sum_output() {
        let out = format!("{BASE}  /data/app/base.apk\n");
        assert_eq!(first_hash(&out).as_deref(), Some(BASE));
        // Not 64 chars, so not a hash.
        assert!(first_hash("deadbeef  file").is_none());
        assert!(first_hash("sha256sum: not found").is_none());
    }

    #[test]
    fn parses_a_batched_hash_listing_and_skips_errors() {
        let out = format!(
            "{BASE}  asset_patches/a\n\
             sha256sum: asset_patches/missing: No such file or directory\n\
             {OTHER}  asset_patches/b\n"
        );
        let map = parse_hash_listing(&out);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("asset_patches/a").unwrap(), BASE);
        assert_eq!(map.get("asset_patches/b").unwrap(), OTHER);
    }

    #[test]
    fn recognises_a_real_push_from_a_no_op_one() {
        assert!(transferred("file pushed. 12.3 MB/s (1024 bytes in 0.001s)"));
        assert!(!transferred("adb: error: failed to copy"));
        assert!(!transferred("0 files pushed. 1 file skipped."));
    }

    // ---------------------------------------------------------------- against a fake adb

    /// A stand-in adb that records the argv it was handed and answers with canned output.
    ///
    /// This is what makes the device layer testable without a headset: it pins the
    /// arguments actually constructed, which is where the original's bugs live, not just
    /// the parsing of what comes back.
    #[cfg(unix)]
    mod fake {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        pub struct Fake {
            pub dir: PathBuf,
            pub adb: Adb,
        }

        impl Fake {
            pub fn new(tag: &str, script_body: &str) -> Fake {
                let dir = std::env::temp_dir()
                    .join(format!("evrce_fakeadb_{}_{}", std::process::id(), tag));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let path = dir.join("adb");
                let log = dir.join("argv.log");
                let script = format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n{}\n",
                    log.display(),
                    script_body
                );
                std::fs::write(&path, script).unwrap();
                let mut perms = std::fs::metadata(&path).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&path, perms).unwrap();
                Fake { adb: Adb::at(&path), dir }
            }

            pub fn calls(&self) -> Vec<String> {
                std::fs::read_to_string(self.dir.join("argv.log"))
                    .unwrap_or_default()
                    .lines()
                    .map(|l| l.to_string())
                    .collect()
            }
        }

        impl Drop for Fake {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    /// The bug that breaks the original the moment a phone is plugged in alongside the
    /// headset: without `-s`, adb refuses every command as ambiguous.
    #[cfg(unix)]
    #[test]
    fn every_command_addresses_the_device_by_serial() {
        let f = fake::Fake::new("serial", "echo 'package:/data/app/base.apk'");
        let device = Device {
            serial: "1WMHH8ABC123".into(),
            state: crate::engine::adb::State::Ready,
            model: Some("Quest 3".into()),
        };
        let quest = Quest::new(&f.adb, Some(&device));

        quest.installed_apk_path();
        quest.shell("echo hi").ok();

        let calls = f.calls();
        assert!(!calls.is_empty());
        for call in &calls {
            assert!(
                call.starts_with("-s 1WMHH8ABC123 "),
                "command did not name the device: {call}"
            );
        }
    }

    /// With no device chosen, adb is left to pick, which is right when there is only one.
    #[cfg(unix)]
    #[test]
    fn omits_the_serial_when_no_device_was_chosen() {
        let f = fake::Fake::new("noserial", "echo ''");
        let quest = Quest::new(&f.adb, None);
        quest.shell("true").ok();
        assert_eq!(f.calls(), vec!["shell true".to_string()]);
    }

    /// A shell script must reach the device as a single argument, or the host shell gets a
    /// chance to split it first.
    #[cfg(unix)]
    #[test]
    fn a_shell_script_is_passed_as_one_argument() {
        let f = fake::Fake::new("onearg", "echo \"$2\"");
        let quest = Quest::new(&f.adb, None);
        let out = quest.shell("cd /sdcard && sha256sum a b c").unwrap();
        assert_eq!(
            out.trim(),
            "cd /sdcard && sha256sum a b c",
            "the script arrived split up"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reads_and_writes_a_marker_through_adb() {
        let body = [
            "case \"$*\" in",
            &format!("*cat*) echo 'version=1'; echo 'base_sha256={BASE}';;"),
            "*push*) echo '1 file pushed. 0.1 MB/s (200 bytes in 0.002s)';;",
            "*) echo '';;",
            "esac",
        ]
        .join("\n");
        let f = fake::Fake::new("marker", &body);
        let quest = Quest::new(&f.adb, None);

        let read = quest.read_marker().expect("marker should parse");
        assert_eq!(read.base_sha256, BASE);

        quest.write_marker(&marker(BASE, PATCHED, true)).expect("push should be accepted");
        let calls = f.calls();
        assert!(calls.iter().any(|c| c.contains("mkdir -p")), "target dir is created first");
        assert!(
            calls.iter().any(|c| c.contains("push") && c.ends_with(&marker_path())),
            "the marker is pushed to a full file path, not a directory: {calls:?}"
        );
    }

    /// A push that moves nothing must not be reported as success.
    #[cfg(unix)]
    #[test]
    fn a_push_that_transfers_nothing_is_an_error() {
        let f = fake::Fake::new("nopush", "echo 'adb: error: failed to copy'");
        let quest = Quest::new(&f.adb, None);
        let err = quest.push(Path::new("/tmp/whatever"), "/sdcard/x").unwrap_err();
        assert!(matches!(err, Error::Unexpected(_)), "got {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn install_requires_adb_to_say_success() {
        let ok = fake::Fake::new("inst_ok", "echo 'Success'");
        assert!(Quest::new(&ok.adb, None).install_apk(Path::new("/tmp/a.apk")).is_ok());

        let bad = fake::Fake::new("inst_bad", "echo 'Failure [INSTALL_FAILED_INVALID_APK]'");
        let err = Quest::new(&bad.adb, None)
            .install_apk(Path::new("/tmp/a.apk"))
            .unwrap_err();
        assert!(matches!(err, Error::Unexpected(_)));
    }

    /// Permissions are granted at install time rather than leaving the user to answer
    /// prompts inside the headset.
    #[cfg(unix)]
    #[test]
    fn install_grants_permissions_up_front() {
        let f = fake::Fake::new("inst_g", "echo 'Success'");
        Quest::new(&f.adb, None).install_apk(Path::new("/tmp/a.apk")).unwrap();
        assert!(f.calls()[0].starts_with("install -g "), "got {:?}", f.calls());
    }
}
