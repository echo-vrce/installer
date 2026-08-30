// SPDX-License-Identifier: GPL-3.0-or-later
//! Revive setup, for playing Echo through SteamVR.
//!
//! The interesting part is `revive.vrmanifest`. It is Revive's own file, listing every
//! Oculus app it knows how to launch, and this adds one entry to it. That means reading
//! JSON somebody else wrote, changing one element, and writing it back with everything
//! else untouched, which is why it goes through a real parser rather than string surgery.
//!
//! The library ID is the part that cannot be invented. Every entry carries a
//! `/library <id>` in its arguments, the same id for all of them, and it comes from the
//! user's own Oculus install. If the manifest has no entry to read it from, there is
//! nothing to guess and the caller is told so.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

pub const APP_ID: &str = "ready-at-dawn-echo-arena";
pub const APP_KEY: &str = "revive.app.ready-at-dawn-echo-arena";
pub const INJECTOR: &str = "ReviveInjector.exe";
pub const VRMANIFEST: &str = "revive.vrmanifest";
/// Named once: the flow checks for this file to see whether an elevated run made it.
pub const SHORTCUT_NAME: &str = "Echo VR (Revive).lnk";
pub const DEFAULT_DIR: &str = r"C:\Program Files\Revive";

/// Where Meta keeps store art. Revive points its tile at a file in here.
pub const STORE_ASSETS: &str =
    r"C:\Program Files\Meta Horizon\CoreData\Software\StoreAssets\ready-at-dawn-echo-arena_assets";
const IMAGE_PATH: &str = "C:/Program Files/Meta Horizon/CoreData/Software/StoreAssets/\
                          ready-at-dawn-echo-arena_assets/cover_landscape_image_large.png";

/// The placeholder Revive ships. Reading it as a real id would produce an entry that
/// launches nothing.
const PLACEHOLDER_LIBRARY: &str = "put-library-ID-here";

/// Asked for the current release rather than pinning a version. The original hardcodes
/// 3.1.1, which is two releases behind; a link that goes stale silently is worse than one
/// that is looked up.
const RELEASES_API: &str = "https://api.github.com/repos/LibreVR/Revive/releases/latest";
/// Used only when the API cannot be reached. Known good, and better than nothing.
const FALLBACK_INSTALLER: &str =
    "https://github.com/LibreVR/Revive/releases/download/3.2.0/ReviveInstaller.exe";

/// Where the game artwork used to be published.
///
/// It is gone: 404 on every mirror, and the whole `stuff/` tree with it. Recorded here
/// because the original still offers "fix game artwork" with the box **ticked by default**,
/// and the download throwing takes its entire Revive chain down with it. Worth re-checking
/// before building anything on top of it.
pub const ARTWORK_URL: &str =
    "https://files.echovr.de/stuff/patches/ready-at-dawn-echo-arena_assets.zip";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A new entry was appended.
    Added,
    /// An existing Echo entry was refreshed.
    Updated,
}

#[derive(Debug)]
pub enum Error {
    NotInstalled,
    NoManifest(PathBuf),
    /// The manifest has no entry to read a library id from, so there is nothing to copy.
    /// Recoverable: the user installs any free Oculus title and Revive fills it in.
    NoLibraryId,
    Json(String),
    /// The user dismissed the elevation prompt.
    ElevationDeclined,
    WindowsOnly,
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotInstalled => write!(f, "Revive is not installed"),
            Error::NoManifest(p) => write!(f, "{} was not found", p.display()),
            Error::NoLibraryId => write!(
                f,
                "No Meta library could be found. The Meta app records one when it is set \
                 up, so if it is installed, open it once and then try again."
            ),
            Error::Json(m) => write!(f, "Revive's manifest could not be read: {m}"),
            Error::ElevationDeclined => write!(
                f,
                "The Revive installer needs administrator rights and the prompt was dismissed."
            ),
            Error::WindowsOnly => write!(f, "Revive is Windows only"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl Error {
    pub fn needs_elevation(&self) -> bool {
        matches!(self, Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied)
    }
}

/// The current ReviveInstaller download, or the pinned fallback.
pub fn installer_url() -> String {
    fn from_api() -> Option<String> {
        let body = crate::engine::download::fetch_text(RELEASES_API).ok()?;
        let json: Value = serde_json::from_str(&body).ok()?;
        json.get("assets")?
            .as_array()?
            .iter()
            .filter_map(|a| a.get("browser_download_url")?.as_str())
            .find(|u| u.to_ascii_lowercase().ends_with(".exe"))
            .map(str::to_string)
    }
    from_api().unwrap_or_else(|| FALLBACK_INSTALLER.to_string())
}

/// Launches an installer that asks for elevation.
///
/// Through ShellExecute with the `runas` verb, not CreateProcess. ReviveInstaller requests
/// elevation in its own manifest, and a plain spawn fails outright with error 740; `runas`
/// is what makes Windows show the prompt. No privilege broker is involved: the OS does the
/// elevating, and this process stays unprivileged.
#[cfg(windows)]
pub fn run_installer(exe: &Path) -> Result<(), Error> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;

    let wide = |s: &std::ffi::OsStr| -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    };
    let verb = wide(std::ffi::OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    // /S is NSIS silent install, so completion is deterministic rather than depending on
    // someone clicking through a wizard inside a window they may not be looking at.
    let params = wide(std::ffi::OsStr::new("/S"));

    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    // ShellExecuteW returns a value above 32 on success. Below that it is an error code,
    // and 5 specifically means the user declined the prompt.
    match result as isize {
        n if n > 32 => Ok(()),
        5 => Err(Error::ElevationDeclined),
        n => Err(Error::Io(std::io::Error::other(format!(
            "could not start the Revive installer (code {n})"
        )))),
    }
}

#[cfg(not(windows))]
pub fn run_installer(_exe: &Path) -> Result<(), Error> {
    Err(Error::WindowsOnly)
}

/// Creates the desktop shortcut that launches Echo through Revive's injector.
///
/// Written as a script to a temp file rather than passed as one long `-Command` string.
/// The original builds that string by hand and doubles quotes inside it; the paths here
/// contain spaces and backslashes, and getting one level of escaping wrong produces a
/// shortcut that silently points at nothing.
#[cfg(windows)]
pub fn create_shortcut(revive_dir: &Path, exe: &Path) -> Result<PathBuf, Error> {
    let injector = revive_dir.join(INJECTOR);
    let desktop = dirs_desktop().ok_or_else(|| {
        Error::Io(std::io::Error::other("could not locate the Desktop folder"))
    })?;
    let link = desktop.join(SHORTCUT_NAME);

    let arguments = format!(
        "\"{}\" -nosymbollookup /app {APP_ID}",
        exe.display()
    );
    let script = format!(
        "$ws = New-Object -ComObject WScript.Shell
         $s = $ws.CreateShortcut('{}')
         $s.TargetPath = '{}'
         $s.Arguments = '{}'
         $s.WorkingDirectory = '{}'
         $s.IconLocation = '{},0'
         $s.Save()
",
        ps_quote(&link.to_string_lossy()),
        ps_quote(&injector.to_string_lossy()),
        ps_quote(&arguments),
        ps_quote(&revive_dir.to_string_lossy()),
        ps_quote(&injector.to_string_lossy()),
    );

    let script_path = std::env::temp_dir().join(format!("evrce_shortcut_{}.ps1", std::process::id()));
    std::fs::write(&script_path, script)?;
    let status = crate::engine::hide_console(&mut std::process::Command::new("powershell"))
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script_path)
        .status();
    let _ = std::fs::remove_file(&script_path);
    status?;

    if link.is_file() {
        Ok(link)
    } else {
        Err(Error::Io(std::io::Error::other("the shortcut was not created")))
    }
}

#[cfg(not(windows))]
pub fn create_shortcut(_revive_dir: &Path, _exe: &Path) -> Result<PathBuf, Error> {
    Err(Error::WindowsOnly)
}

/// PowerShell single-quoted strings escape a quote by doubling it, and nothing else needs
/// escaping inside them. That is the whole rule, and it is why single quotes are used.
pub fn ps_quote(value: &str) -> String {
    value.replace('\'', "\'\'")
}

#[cfg(windows)]
fn dirs_desktop() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Desktop"))
}

/// Finds an installed Revive, verifying by the injector's presence rather than trusting a
/// registry entry that may be stale.
/// How a Revive was arrived at. Same shape as adb's, because it answers the same question:
/// "which one is this?", which is the first thing anyone asks when it misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A folder the user chose. Always wins.
    Configured,
    /// Found through the uninstall entry Revive's own installer writes.
    Registry,
    /// Found where the installer puts it by default.
    DefaultPath,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Configured => "chosen by you",
            Source::Registry => "found from its installation record",
            Source::DefaultPath => "found in the default location",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub dir: PathBuf,
    pub source: Source,
}

/// Finds Revive, preferring a folder the user named.
///
/// A configured path that no longer holds an injector is reported as missing rather than
/// silently ignored: someone who chose a folder should be told their choice stopped being
/// valid, not quietly moved onto a different copy.
pub fn locate(configured: Option<&Path>) -> Option<Located> {
    if let Some(dir) = configured {
        return verify(dir).map(|dir| Located { dir, source: Source::Configured });
    }
    if !cfg!(windows) {
        return None;
    }
    if let Some(dir) = registry_install_location().and_then(|d| verify(&d)) {
        return Some(Located { dir, source: Source::Registry });
    }
    verify(Path::new(DEFAULT_DIR)).map(|dir| Located { dir, source: Source::DefaultPath })
}

/// Downloads the current Revive installer and runs it.
///
/// Blocking, and it ends with a Windows elevation prompt: ReviveInstaller asks for
/// administrator rights in its own manifest, so this cannot complete unattended.
pub fn install(
    cancel: &crate::engine::Cancel,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<PathBuf, Error> {
    let url = installer_url();
    let dest = crate::config::dir().join("staging").join("ReviveInstaller.exe");
    let spec = crate::engine::download::Spec::new(url, dest.clone());
    crate::engine::download::fetch(&spec, cancel, &mut |s| {
        on_progress(s.done, s.total)
    })
    .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    run_installer(&dest)?;
    Ok(dest)
}

pub fn find_dir() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    if let Some(dir) = registry_install_location().and_then(|d| verify(&d)) {
        return Some(dir);
    }
    verify(Path::new(DEFAULT_DIR))
}

fn verify(dir: &Path) -> Option<PathBuf> {
    dir.join(INJECTOR).is_file().then(|| dir.to_path_buf())
}

#[cfg(windows)]
fn registry_install_location() -> Option<PathBuf> {
    // Read through PowerShell rather than linking a registry crate: it is one query, it
    // needs no elevation, and it keeps a dependency out of the tree for a single lookup.
    let out = crate::engine::hide_console(&mut std::process::Command::new("powershell"))
        .args([
            "-NoProfile",
            "-Command",
            "Get-ItemProperty \
             'HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*',\
             'HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*' \
             -ErrorAction SilentlyContinue | \
             Where-Object { $_.DisplayName -like '*Revive*' } | \
             Select-Object -ExpandProperty InstallLocation -First 1",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

#[cfg(not(windows))]
fn registry_install_location() -> Option<PathBuf> {
    None
}

/// Is Echo already in Revive's manifest?
///
/// For checking what an elevated run actually achieved. Its results cannot be handed back
/// across a process boundary as data, and inventing a summary from "it exited zero" would
/// be reporting rather than knowing.
pub fn has_entry(revive_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(revive_dir.join(VRMANIFEST)) else { return false };
    let Ok(root) = serde_json::from_str::<Value>(&text) else { return false };
    root.get("applications")
        .and_then(Value::as_array)
        .is_some_and(|apps| {
            apps.iter().any(|a| a.get("app_key").and_then(Value::as_str) == Some(APP_KEY))
        })
}

/// Where the desktop shortcut would be, whether or not it is there.
pub fn shortcut_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        dirs_desktop().map(|d| d.join(SHORTCUT_NAME))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Adds or refreshes Echo's entry in `revive.vrmanifest`.
pub fn patch_manifest(revive_dir: &Path, exe: &Path) -> Result<Outcome, Error> {
    let path = revive_dir.join(VRMANIFEST);
    if !path.is_file() {
        return Err(Error::NoManifest(path));
    }
    let text = std::fs::read_to_string(&path)?;
    // A freshly installed Revive leaves this file empty - zero bytes, verified on a clean
    // Windows. Parsed as JSON that is a syntax error, and reporting one would blame the
    // file for being malformed when it is simply new. Treated as "no entries yet" instead,
    // which falls through to NoLibraryId and its actual explanation.
    let mut root: Value = if text.trim().is_empty() {
        json!({ "source": "revive", "applications": [] })
    } else {
        serde_json::from_str(&text).map_err(|e| Error::Json(e.to_string()))?
    };

    let apps = root
        .get_mut("applications")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Error::Json("no applications list".into()))?;

    // The client's own record first. Copying it out of another app's entry, as the
    // original does, only works once Revive has already seen a library - which is why that
    // installer tells people to install a free title and start SteamVR before trying. The
    // registry knows on a machine where none of that has happened.
    let library = crate::engine::meta::library_id_for(exe)
        .or_else(|| detect_library_id(apps))
        .ok_or(Error::NoLibraryId)?;
    let entry = echo_entry(&library, exe);

    let outcome = match apps.iter_mut().find(|a| {
        a.get("app_key").and_then(Value::as_str) == Some(APP_KEY)
    }) {
        Some(existing) => {
            // Merged rather than replaced, so anything Revive added of its own survives.
            if let (Some(dst), Some(src)) = (existing.as_object_mut(), entry.as_object()) {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
            Outcome::Updated
        }
        None => {
            apps.push(entry);
            Outcome::Added
        }
    };

    let out = serde_json::to_string_pretty(&root).map_err(|e| Error::Json(e.to_string()))?;
    // Written through a temp file: a half-written vrmanifest would take out every app
    // Revive knows about, not just Echo's.
    let tmp = path.with_extension("vrmanifest.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &path)?;
    Ok(outcome)
}

/// Reads the shared library id out of any existing entry's arguments.
pub fn detect_library_id(apps: &[Value]) -> Option<String> {
    apps.iter()
        .filter_map(|a| a.get("arguments").and_then(Value::as_str))
        .filter_map(library_id_in)
        .find(|id| id != PLACEHOLDER_LIBRARY)
}

/// Pulls the token after `/library` out of an arguments string.
fn library_id_in(arguments: &str) -> Option<String> {
    let mut tokens = arguments.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "/library" {
            return tokens.next().map(str::to_string);
        }
    }
    None
}

fn echo_entry(library_id: &str, exe: &Path) -> Value {
    json!({
        "action_manifest_path": "Input/action_manifest.json",
        "app_key": APP_KEY,
        "arguments": format!(
            "/app {APP_ID} /library {library_id} \
             \"Software\\ready-at-dawn-echo-arena\\bin\\win10\\echovr.exe\" -nosymbollookup"
        ),
        "binary_path_windows": INJECTOR,
        "image_path": IMAGE_PATH,
        "launch_type": "binary",
        "strings": { "en_us": { "name": APP_ID } },
        // Recorded so the exe the entry was built against is visible; Revive ignores it.
        "_echo_vrce_exe": exe.to_string_lossy(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_recognised_by_its_app_key() {
        // This is how an elevated run's result is learned: by looking, not by being told.
        let dir = tmpdir("has_entry");
        std::fs::write(dir.join(VRMANIFEST), manifest_with(json!([]))).unwrap();
        assert!(!has_entry(&dir), "an empty list holds nothing");

        std::fs::write(
            dir.join(VRMANIFEST),
            manifest_with(json!([{ "app_key": APP_KEY }])),
        )
        .unwrap();
        assert!(has_entry(&dir));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_missing_or_broken_manifest_holds_no_entry() {
        // Never a panic and never a false yes: this decides whether the user is told the
        // setup worked.
        let dir = tmpdir("has_entry_bad");
        assert!(!has_entry(&dir), "no file at all");
        std::fs::write(dir.join(VRMANIFEST), "").unwrap();
        assert!(!has_entry(&dir), "empty file");
        std::fs::write(dir.join(VRMANIFEST), "{not json").unwrap();
        assert!(!has_entry(&dir), "not json");
        std::fs::write(dir.join(VRMANIFEST), r#"{"applications": "nope"}"#).unwrap();
        assert!(!has_entry(&dir), "wrong shape");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_empty_manifest_is_new_not_broken() {
        // What a freshly installed Revive actually leaves on disk: zero bytes. Parsing that
        // as JSON fails, and the user would be told their manifest is corrupt when the real
        // answer is that Revive has not seen their library yet.
        let dir = std::env::temp_dir().join(format!("evrce-revive-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(VRMANIFEST), "").unwrap();

        let err = patch_manifest(&dir, Path::new("C:\\Echo\\echovr.exe")).unwrap_err();
        assert!(
            matches!(err, Error::NoLibraryId),
            "an empty manifest should ask for a library, not report bad JSON: {err}"
        );
        // And the message has to say what to do about it.
        let text = err.to_string();
        assert!(text.to_lowercase().contains("library"), "got {text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn whitespace_only_counts_as_empty_too() {
        let dir = std::env::temp_dir().join(format!("evrce-revive-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(VRMANIFEST), "  \r\n\t ").unwrap();
        assert!(matches!(
            patch_manifest(&dir, Path::new("C:\\Echo\\echovr.exe")).unwrap_err(),
            Error::NoLibraryId
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn manifest_with(entries: Value) -> String {
        serde_json::to_string_pretty(&json!({
            "source": "builtin",
            "applications": entries
        }))
        .unwrap()
    }

    fn other_app(library: &str) -> Value {
        json!({
            "app_key": "revive.app.some-other-game",
            "arguments": format!("/app some-other-game /library {library} \"x.exe\""),
            "launch_type": "binary",
            "some_field_we_do_not_know_about": 42
        })
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("evrce_revive_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The one escaping rule that matters for the shortcut. Getting it wrong produces a
    /// shortcut that points at nothing and says nothing.
    #[test]
    fn powershell_single_quotes_are_escaped_by_doubling() {
        assert_eq!(ps_quote(r"C:\Program Files\Revive"), r"C:\Program Files\Revive");
        assert_eq!(ps_quote("it's"), "it''s");
        // Backslashes are literal inside single quotes, so they are left alone.
        assert_eq!(ps_quote(r"C:\a\b"), r"C:\a\b");
        assert_eq!(ps_quote(""), "");
    }

    #[test]
    fn reads_the_library_id_out_of_an_existing_entry() {
        let apps = vec![other_app("a1b2c3d4")];
        assert_eq!(detect_library_id(&apps).as_deref(), Some("a1b2c3d4"));
    }

    /// Revive ships a placeholder. Reading it as real would produce an entry that launches
    /// nothing, and nothing would say why.
    #[test]
    fn ignores_revives_placeholder_library_id() {
        let apps = vec![other_app(PLACEHOLDER_LIBRARY)];
        assert_eq!(detect_library_id(&apps), None);
        // A real one further down the list is still found.
        let apps = vec![other_app(PLACEHOLDER_LIBRARY), other_app("real123")];
        assert_eq!(detect_library_id(&apps).as_deref(), Some("real123"));
    }

    #[test]
    fn survives_entries_with_no_arguments_at_all() {
        let apps = vec![json!({"app_key": "x"}), other_app("found")];
        assert_eq!(detect_library_id(&apps).as_deref(), Some("found"));
        assert_eq!(detect_library_id(&[json!({"app_key": "x"})]), None);
    }

    #[test]
    fn parses_the_library_token_from_a_full_argument_string() {
        assert_eq!(
            library_id_in("/app foo /library abc123 \"C:\\x.exe\" -nosymbollookup").as_deref(),
            Some("abc123")
        );
        assert_eq!(library_id_in("/app foo -nosymbollookup"), None);
        // Trailing /library with nothing after it is not an id.
        assert_eq!(library_id_in("/app foo /library"), None);
    }

    #[test]
    fn adds_an_entry_and_leaves_the_rest_of_the_file_alone() {
        let dir = tmpdir("add");
        std::fs::write(
            dir.join(VRMANIFEST),
            manifest_with(json!([other_app("lib42")])),
        )
        .unwrap();

        let outcome = patch_manifest(&dir, Path::new("C:/EchoVR/x/echovr.exe")).unwrap();
        assert_eq!(outcome, Outcome::Added);

        let text = std::fs::read_to_string(dir.join(VRMANIFEST)).unwrap();
        let root: Value = serde_json::from_str(&text).unwrap();
        let apps = root["applications"].as_array().unwrap();
        assert_eq!(apps.len(), 2);

        // Everything that was already there is still there, including fields this code
        // knows nothing about.
        assert_eq!(root["source"], "builtin");
        assert_eq!(apps[0]["some_field_we_do_not_know_about"], 42);

        let echo = &apps[1];
        assert_eq!(echo["app_key"], APP_KEY);
        assert!(echo["arguments"].as_str().unwrap().contains("/library lib42"));
        assert_eq!(echo["binary_path_windows"], INJECTOR);
        assert_eq!(echo["strings"]["en_us"]["name"], APP_ID);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn refreshes_an_existing_entry_instead_of_duplicating_it() {
        let dir = tmpdir("update");
        let stale = json!({
            "app_key": APP_KEY,
            "arguments": "/app ready-at-dawn-echo-arena /library OLD \"old.exe\"",
            "a_field_revive_added": true
        });
        std::fs::write(
            dir.join(VRMANIFEST),
            manifest_with(json!([other_app("lib42"), stale])),
        )
        .unwrap();

        assert_eq!(
            patch_manifest(&dir, Path::new("C:/new/echovr.exe")).unwrap(),
            Outcome::Updated
        );

        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(VRMANIFEST)).unwrap()).unwrap();
        let apps = root["applications"].as_array().unwrap();
        assert_eq!(apps.len(), 2, "must not have added a second Echo entry");
        let echo = apps.iter().find(|a| a["app_key"] == APP_KEY).unwrap();
        assert!(echo["arguments"].as_str().unwrap().contains("/library lib42"));
        assert_eq!(
            echo["a_field_revive_added"], true,
            "fields Revive owns must survive an update"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// Nothing to copy an id from is a recoverable state with a specific fix, so it gets
    /// its own error rather than a generic failure.
    #[test]
    fn refuses_when_there_is_no_library_id_to_copy() {
        let dir = tmpdir("nolib");
        std::fs::write(dir.join(VRMANIFEST), manifest_with(json!([]))).unwrap();
        let err = patch_manifest(&dir, Path::new("x")).unwrap_err();
        assert!(matches!(err, Error::NoLibraryId), "got {err:?}");
        // The advice used to be "install a free title and start SteamVR", because the id
        // was copied out of another entry. It is read from the Meta client's own record
        // now, so the only thing left to say is to open that client once.
        let text = err.to_string();
        assert!(text.contains("Meta app"), "the fix should be in the message: {text}");
        assert!(!text.contains("free title"), "that advice is no longer true: {text}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reports_a_missing_manifest_by_name() {
        let dir = tmpdir("nomanifest");
        assert!(matches!(patch_manifest(&dir, Path::new("x")), Err(Error::NoManifest(_))));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn refuses_a_manifest_that_is_not_json() {
        let dir = tmpdir("badjson");
        std::fs::write(dir.join(VRMANIFEST), "this is not json at all").unwrap();
        assert!(matches!(patch_manifest(&dir, Path::new("x")), Err(Error::Json(_))));
        std::fs::remove_dir_all(dir).ok();
    }

    /// A half-written vrmanifest would take out every app Revive knows about, so the write
    /// goes through a temp file and leaves nothing behind.
    #[test]
    fn writing_leaves_no_temp_file() {
        let dir = tmpdir("atomic");
        std::fs::write(dir.join(VRMANIFEST), manifest_with(json!([other_app("l")]))).unwrap();
        patch_manifest(&dir, Path::new("x")).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");
        std::fs::remove_dir_all(dir).ok();
    }
}
