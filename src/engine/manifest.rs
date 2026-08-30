// SPDX-License-Identifier: GPL-3.0-or-later
//! Update manifest parsing.
//!
//! Ported from `UpdateManifest.java`, keeping its grammar, its header names and above all
//! its strictness. Manifest paths are attacker-influenced input that ends up interpolated
//! into `adb shell` scripts (including `rm -rf`) and into local filesystem paths, so this
//! module is the single choke point. Validation is hand-written rather than a regex: ten
//! lines of explicit predicate are easier to audit than a character class, and it keeps a
//! large dependency out of the tree.
//!
//! Body grammar, whitespace separated, `#` comments and blank lines ignored:
//!
//! ```text
//! add  path/to/file.dll  <64 hex>
//! del  path/to/old.dll
//! ```
//!
//! The Quest manifest additionally carries two headers, which live inside comments so the
//! plain body parser still reads the file:
//!
//! ```text
//! # BASE_APK: echo_quest_27-08-2026.001.apk 0a7fa5f9...
//! # Target:  /sdcard/Android/media/com.readyatdawn.r15
//! ```

use std::fmt;

/// Longest path we will accept. Well past anything the real manifests use; this only
/// exists so a hostile manifest cannot hand us a megabyte-long path.
const MAX_PATH_LEN: usize = 255;

/// The only on-device root that may reach `rm -rf`. Anything else is refused outright.
const TARGET_PREFIX: &str = "/sdcard/Android/media/com.readyatdawn.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Add,
    Del,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub action: Action,
    pub path: String,
    /// Lowercase hex. Present for `add`, absent for `del`.
    pub sha256: Option<String>,
}

/// The base APK a Quest manifest was built against, from the `# BASE_APK:` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseApk {
    pub name: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    UnknownAction(String),
    UnsafePath(String),
    MissingHash(String),
    BadHash { path: String, value: String },
    UnsafeTarget(String),
    /// A `# BASE_APK:` line that is present but malformed. Silently ignoring it would let
    /// a Quest install proceed with no version gate at all.
    MalformedBaseApk(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownAction(a) => write!(f, "unknown manifest action: {a}"),
            Error::UnsafePath(p) => write!(f, "unsafe path in manifest: {p}"),
            Error::MissingHash(p) => write!(f, "missing SHA-256 for manifest entry: {p}"),
            Error::BadHash { path, value } => {
                write!(f, "malformed SHA-256 for {path}: {value}")
            }
            Error::UnsafeTarget(t) => write!(f, "unsafe target root in manifest: {t}"),
            Error::MalformedBaseApk(l) => write!(f, "malformed BASE_APK header: {l}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    entries: Vec<Entry>,
    base_url: String,
    base_apk: Option<BaseApk>,
    target_root: Option<String>,
}

impl Manifest {
    /// Parses a manifest. `manifest_url` is only used to derive the base URL that entry
    /// paths resolve against, so tests can pass any plausible string.
    pub fn parse(content: &str, manifest_url: &str) -> Result<Self, Error> {
        let mut entries = Vec::new();
        let mut base_apk = None;
        let mut target_root = None;

        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }

            // Headers are comments, so they have to be matched before comments are skipped.
            if let Some(comment) = line.strip_prefix('#') {
                let comment = comment.trim();
                if let Some(rest) = strip_prefix_ci(comment, "BASE_APK:") {
                    base_apk = Some(parse_base_apk(rest.trim(), line)?);
                } else if let Some(rest) = strip_prefix_ci(comment, "Target:") {
                    target_root = Some(rest.trim().to_string());
                }
                continue;
            }

            let mut tokens = line.split_whitespace();
            let action = match tokens.next() {
                Some(a) => a,
                None => continue,
            };
            let path = match tokens.next() {
                Some(p) => p,
                // The Java skips a line with fewer than two tokens rather than failing.
                // Kept: a stray word is far more likely to be a typo than an attack.
                None => continue,
            };

            let action = match action {
                "add" => Action::Add,
                "del" => Action::Del,
                other => return Err(Error::UnknownAction(other.to_string())),
            };

            if !path_is_safe(path) {
                return Err(Error::UnsafePath(path.to_string()));
            }

            let sha256 = match action {
                Action::Add => {
                    let hash = tokens.next().ok_or_else(|| Error::MissingHash(path.to_string()))?;
                    if !is_sha256_hex(hash) {
                        return Err(Error::BadHash {
                            path: path.to_string(),
                            value: hash.to_string(),
                        });
                    }
                    Some(hash.to_ascii_lowercase())
                }
                Action::Del => None,
            };

            entries.push(Entry { action, path: path.to_string(), sha256 });
        }

        if let Some(root) = &target_root {
            if !target_is_safe(root) {
                return Err(Error::UnsafeTarget(root.clone()));
            }
        }

        let base_url = match manifest_url.rfind('/') {
            Some(i) => manifest_url[..i].to_string(),
            None => String::new(),
        };

        Ok(Manifest { entries, base_url, base_apk, target_root })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// `del` entries, which are applied before any `add`.
    pub fn dels(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.action == Action::Del)
    }

    pub fn adds(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.action == Action::Add)
    }

    /// Manifest URL up to but excluding the last `/`. Entry paths resolve against it.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn url_for(&self, entry: &Entry) -> String {
        format!("{}/{}", self.base_url, entry.path)
    }

    /// The base APK this manifest was built against, or None on a PC manifest.
    pub fn base_apk(&self) -> Option<&BaseApk> {
        self.base_apk.as_ref()
    }

    /// On-device root the entry paths are relative to, or None on a PC manifest.
    pub fn target_root(&self) -> Option<&str> {
        self.target_root.as_deref()
    }
}

fn parse_base_apk(rest: &str, line: &str) -> Result<BaseApk, Error> {
    let mut parts = rest.split_whitespace();
    let name = parts.next().unwrap_or_default();
    let hash = parts.next().unwrap_or_default();
    // The APK name lands in a local filename, so it goes through the same gate as any
    // other path, and it must not contain a directory separator.
    if name.is_empty() || !path_is_safe(name) || name.contains('/') || !is_sha256_hex(hash) {
        return Err(Error::MalformedBaseApk(line.to_string()));
    }
    Ok(BaseApk { name: name.to_string(), sha256: hash.to_ascii_lowercase() })
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Exactly 64 hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Is this path safe to interpolate into a shell command and to join onto a directory?
///
/// Mirrors the Java's `^[A-Za-z0-9._][A-Za-z0-9._/+-]*$` plus a few extra refusals.
/// Two details in that character class are load-bearing and easy to lose:
///
/// - The **first** character may not be `/`, so a path can never be absolute.
/// - The first character may not be `-`, so a path can never be mistaken for a command
///   line option. `rm -rf` handed a file called `-rf` is a different command.
///
/// `+` is allowed after the first character because the PC manifest genuinely ships
/// `libstdc++-6.dll`. It is neither a shell metacharacter nor a glob character, and it is
/// literal inside a URL path segment.
pub fn path_is_safe(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_PATH_LEN {
        return false;
    }
    // Rejects `../`, `a/../b` and a bare `..`, and by being a substring test it also
    // rejects the encoded-looking oddities that a smarter check might wave through.
    if path.contains("..") {
        return false;
    }

    let mut chars = path.chars();
    let first = chars.next().expect("checked non-empty");
    if !(first.is_ascii_alphanumeric() || first == '.' || first == '_') {
        return false;
    }
    if !path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '+' | '-'))
    {
        return false;
    }

    // Extra hardening beyond the Java: no trailing slash and no empty segment. Neither is
    // dangerous on its own, but both make a path mean different things to different
    // joiners, and a manifest has no reason to contain either.
    if path.ends_with('/') || path.contains("//") {
        return false;
    }

    for segment in path.split('/') {
        // A bare "." segment. `rm -rf root/.` is refused by most implementations of rm,
        // but that is their good manners rather than a guarantee, and `a/./b` and `a/b`
        // are the same file under two different keys, which quietly breaks the map that
        // decides what is already up to date.
        if segment == "." {
            return false;
        }
        // Windows strips a trailing dot or space from a filename, so `a.` and `a` are the
        // same file while being different manifest entries.
        if segment.ends_with('.') || segment.ends_with(' ') {
            return false;
        }
        if is_reserved_device_name(segment) {
            return false;
        }
    }
    true
}

/// The DOS device names, which are still real on Windows.
///
/// `con` is the console, `nul` discards, `com1` is a serial port. Opening one by accident
/// does not fail - it succeeds and does something else entirely, so a manifest entry named
/// `nul` would appear to be written and then fail its checksum for no visible reason. The
/// extension does not help: `con.txt` is still the console.
pub fn is_reserved_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    let lower = stem.to_ascii_lowercase();
    matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || matches!(lower.strip_prefix("com").and_then(|n| n.parse::<u8>().ok()), Some(1..=9))
        || matches!(lower.strip_prefix("lpt").and_then(|n| n.parse::<u8>().ok()), Some(1..=9))
}

/// Only an Echo VR app media directory may reach `rm -rf`.
///
/// Mirrors `^/sdcard/Android/media/com\.readyatdawn\.[A-Za-z0-9]+$`.
pub fn target_is_safe(target: &str) -> bool {
    match target.strip_prefix(TARGET_PREFIX) {
        Some(rest) => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric()),
        None => false,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn rejects_dot_segments() {
        // `rm -rf root/.` usually refuses, but that is rm being careful, not a guard here.
        assert!(!path_is_safe("."));
        assert!(!path_is_safe("./a"));
        assert!(!path_is_safe("a/."));
        // And `a/./b` is the same file as `a/b` under a different key, which breaks the
        // map that decides what is already current.
        assert!(!path_is_safe("a/./b"));
    }

    #[test]
    fn rejects_windows_device_names() {
        // These do not fail to open on Windows; they succeed and are something else.
        for name in ["con", "CON", "nul", "prn", "aux", "com1", "LPT9", "con.txt", "nul.bin"] {
            assert!(!path_is_safe(name), "{name} should be rejected");
            assert!(!path_is_safe(&format!("sub/{name}")), "sub/{name} should be rejected");
        }
        // Names that merely start the same way are ordinary files.
        for name in ["console.dll", "com0", "com10", "conf.json", "nullify.bin"] {
            assert!(path_is_safe(name), "{name} should be allowed");
        }
    }

    #[test]
    fn rejects_names_windows_would_silently_rename() {
        // Windows strips a trailing dot or space, so these are a second name for a file
        // that already has one.
        assert!(!path_is_safe("a."));
        assert!(!path_is_safe("a "));
        assert!(!path_is_safe("dir/a."));
    }

    #[test]
    fn still_accepts_what_the_real_manifests_contain() {
        for p in [
            "libstdc++-6.dll",
            "asset_patches/manifest.json",
            "sourcedb/rad15/json/r15/config.json",
            "asset_patches/posters/poster_b.desc",
            ".hidden",
        ] {
            assert!(path_is_safe(p), "{p} should be allowed");
        }
    }
    use super::*;

    /// Trimmed from the live PC manifest, 2026-08-27. Includes the line that separates
    /// path and hash with a single space rather than two, and `libstdc++-6.dll`, which is
    /// the entire reason `+` is allowed in a path.
    const PC: &str = "\
# Echo VR update manifest - 2026-08-27
# Base URL: https://files.echovr.de/updates

add  asset_patches/manifest.json  72b46e875ca605f8fedca712c1ce26a1bc80133db2c7adf64c0976c392e3f799
add  asset_patches/posters/poster_a.dds  425b224749bad06221cc9d7d94cea9d3879f092e1a639f429788d61e8c84dea0
add  libstdc++-6.dll  be7d2ca81c1a991476976610d5e8c9bdbcdd10e528d4e0384b1cc2b3671ba75a
add  LibOVRPlatformImpl64_1.dll f1669abbee3943000284c5341ebce4ba25d9ae25e65fef73e1937e2b14042aa3
";

    /// Trimmed from the live Quest manifest, 2026-08-27.
    const QUEST: &str = "\
# Echo VR Quest update manifest -- 2026-08-27
# Base URL: https://files.echovr.de/updates/quest
# BASE_APK: echo_quest_27-08-2026.001.apk 0a7fa5f9cfc173013e152a75fac2ded7ca4f66b8d8530f598c0c2530b5cf0973
# Target:  /sdcard/Android/media/com.readyatdawn.r15

add  asset_patches/489bb35d53ca50e9/2dfe2e7610506f03  d5f216f7df4194345187abd6ffb91de4c463e983fbac1e08173177302db2cf37
del  asset_patches/stale_thing
";

    const H: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn parse(body: &str) -> Result<Manifest, Error> {
        Manifest::parse(body, "https://files.echovr.de/updates/update.manifest")
    }

    // ---------------------------------------------------------------- happy paths

    #[test]
    fn parses_the_live_pc_manifest() {
        let m = parse(PC).unwrap();
        assert_eq!(m.entries().len(), 4);
        assert_eq!(m.adds().count(), 4);
        assert_eq!(m.dels().count(), 0);
        // A PC manifest carries neither header.
        assert!(m.base_apk().is_none());
        assert!(m.target_root().is_none());
    }

    /// The real file has one line with a single space before the hash. `split_whitespace`
    /// has to be doing the work, not a fixed-width split.
    #[test]
    fn tolerates_single_space_separation() {
        let m = parse(PC).unwrap();
        let e = m.adds().find(|e| e.path == "LibOVRPlatformImpl64_1.dll").unwrap();
        assert_eq!(
            e.sha256.as_deref(),
            Some("f1669abbee3943000284c5341ebce4ba25d9ae25e65fef73e1937e2b14042aa3")
        );
    }

    #[test]
    fn accepts_plus_in_a_path_because_libstdcpp_ships_with_one() {
        let m = parse(PC).unwrap();
        assert!(m.adds().any(|e| e.path == "libstdc++-6.dll"));
    }

    #[test]
    fn parses_the_live_quest_headers() {
        let m = Manifest::parse(QUEST, "https://files.echovr.de/updates/quest/update.manifest").unwrap();
        let apk = m.base_apk().expect("BASE_APK header");
        assert_eq!(apk.name, "echo_quest_27-08-2026.001.apk");
        assert_eq!(apk.sha256, "0a7fa5f9cfc173013e152a75fac2ded7ca4f66b8d8530f598c0c2530b5cf0973");
        assert_eq!(m.target_root(), Some("/sdcard/Android/media/com.readyatdawn.r15"));
    }

    #[test]
    fn del_entries_carry_no_hash() {
        let m = Manifest::parse(QUEST, "https://x/y/update.manifest").unwrap();
        let d: Vec<_> = m.dels().collect();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "asset_patches/stale_thing");
        assert!(d[0].sha256.is_none());
    }

    #[test]
    fn resolves_urls_against_the_manifest_location() {
        let m = parse(PC).unwrap();
        assert_eq!(m.base_url(), "https://files.echovr.de/updates");
        let e = m.adds().find(|e| e.path == "libstdc++-6.dll").unwrap();
        assert_eq!(m.url_for(e), "https://files.echovr.de/updates/libstdc++-6.dll");
    }

    #[test]
    fn hashes_are_normalised_to_lowercase() {
        let m = parse(&format!("add x.dll {}", "AB".repeat(32))).unwrap();
        assert_eq!(m.adds().next().unwrap().sha256.as_deref(), Some("ab".repeat(32).as_str()));
    }

    // ---------------------------------------------------------------- refusals

    #[test]
    fn refuses_directory_traversal() {
        assert_eq!(
            parse(&format!("add ../../etc/passwd {H}")),
            Err(Error::UnsafePath("../../etc/passwd".into()))
        );
        assert_eq!(
            parse(&format!("add a/../../b {H}")),
            Err(Error::UnsafePath("a/../../b".into()))
        );
    }

    /// The first character may not be `/`, so a manifest can never name an absolute path.
    #[test]
    fn refuses_absolute_paths() {
        assert_eq!(
            parse(&format!("add /etc/passwd {H}")),
            Err(Error::UnsafePath("/etc/passwd".into()))
        );
    }

    /// The subtle one. `rm -rf` handed a file named `-rf` is a different command, so a
    /// leading dash has to be refused even though dashes are fine elsewhere.
    #[test]
    fn refuses_a_leading_dash_so_a_path_cannot_become_a_flag() {
        assert_eq!(parse(&format!("add -rf {H}")), Err(Error::UnsafePath("-rf".into())));
        assert!(parse(&format!("add plugins/some-file.dll {H}")).is_ok());
    }

    #[test]
    fn refuses_shell_metacharacters() {
        for bad in ["a;rm", "a&&b", "a|b", "a$b", "a`b`", "a*b", "a?b", "a>b", "a'b", "a\"b", "a(b)"] {
            assert!(
                matches!(parse(&format!("add {bad} {H}")), Err(Error::UnsafePath(_))),
                "should have refused {bad:?}"
            );
        }
    }

    #[test]
    fn refuses_empty_segments_and_trailing_slash() {
        assert!(matches!(parse(&format!("add a//b {H}")), Err(Error::UnsafePath(_))));
        assert!(matches!(parse(&format!("add a/b/ {H}")), Err(Error::UnsafePath(_))));
    }

    #[test]
    fn refuses_an_overlong_path() {
        let long = "a".repeat(MAX_PATH_LEN + 1);
        assert!(matches!(parse(&format!("add {long} {H}")), Err(Error::UnsafePath(_))));
    }

    #[test]
    fn refuses_an_add_without_a_hash() {
        assert_eq!(parse("add x.dll"), Err(Error::MissingHash("x.dll".into())));
    }

    #[test]
    fn refuses_a_malformed_hash() {
        assert!(matches!(parse("add x.dll deadbeef"), Err(Error::BadHash { .. })));
        assert!(matches!(parse(&format!("add x.dll {}", "z".repeat(64))), Err(Error::BadHash { .. })));
    }

    #[test]
    fn refuses_an_unknown_action() {
        assert_eq!(parse("frobnicate x.dll"), Err(Error::UnknownAction("frobnicate".into())));
    }

    /// Only an Echo VR app media directory may ever reach `rm -rf`.
    #[test]
    fn refuses_a_target_outside_the_echo_media_dir() {
        for bad in [
            "/sdcard",
            "/sdcard/Android/media",
            "/sdcard/Android/media/com.example.evil",
            "/sdcard/Android/media/com.readyatdawn.r15/../../..",
            "/sdcard/Android/media/com.readyatdawn.",
            "/sdcard/Android/media/com.readyatdawn.r15 ; rm -rf /",
        ] {
            let body = format!("# Target: {bad}\n");
            assert!(
                matches!(parse(&body), Err(Error::UnsafeTarget(_))),
                "should have refused target {bad:?}"
            );
        }
        assert!(parse("# Target: /sdcard/Android/media/com.readyatdawn.r15\n").is_ok());
    }

    /// Silently ignoring a broken BASE_APK would leave a Quest install with no version
    /// gate at all, which is worse than refusing to run.
    #[test]
    fn refuses_a_malformed_base_apk_header() {
        for bad in [
            "# BASE_APK:",
            "# BASE_APK: only_a_name.apk",
            "# BASE_APK: name.apk not_a_hash",
            "# BASE_APK: ../escape.apk 0000000000000000000000000000000000000000000000000000000000000000",
            "# BASE_APK: sub/dir.apk 0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert!(
                matches!(parse(&format!("{bad}\n")), Err(Error::MalformedBaseApk(_))),
                "should have refused {bad:?}"
            );
        }
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let m = parse("\n# just a comment\n\n   \n# Base URL: whatever\n").unwrap();
        assert!(m.entries().is_empty());
    }

    /// Header matching is case insensitive here, unlike the Java's exact-case regex. A
    /// deliberate loosening: a manifest that writes `# base_apk:` is a typo, not an
    /// attack, and silently dropping the version gate is the worse outcome.
    #[test]
    fn header_names_are_case_insensitive() {
        let body = format!("# base_apk: a.apk {H}\n# target: /sdcard/Android/media/com.readyatdawn.r15\n");
        let m = parse(&body).unwrap();
        assert_eq!(m.base_apk().unwrap().name, "a.apk");
        assert!(m.target_root().is_some());
    }
}
