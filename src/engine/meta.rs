// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding an Echo VR that the Meta client installed.
//!
//! The folder has been renamed twice: `Program Files\Oculus`, then `Program Files\Meta\`,
//! and now `Program Files\Meta Horizon\`. Meta's own help pages still document the middle
//! one. Anything hardcoded here would be wrong again within a release.
//!
//! What has *not* changed is the registry key. The client still writes its install base to
//! the legacy `Oculus VR, LLC\Oculus` key - the uninstall entry is even still called
//! `Oculus` - so reading it covers all three names, and covers a base the user moved
//! somewhere else during setup, which no list of guesses can.
//!
//! The literal paths below are a fallback for when the registry says nothing, in the order
//! the user would want: newest first.

use std::path::{Path, PathBuf};

use crate::engine::install;

/// Games sit under `<base>\Software\Software\<app>`. Two `Software` directories is not a
/// typo: the client's own folder, then its library inside it.
const LIBRARY: [&str; 2] = ["Software", "Software"];

/// Bases to try when the registry has nothing, newest naming first.
const KNOWN_BASES: [&str; 3] = [
    r"C:\Program Files\Meta Horizon",
    r"C:\Program Files\Meta",
    r"C:\Program Files\Oculus",
];

/// How a path was arrived at. Shown to the user, because a suggestion whose reasoning is
/// invisible is indistinguishable from the app deciding for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Registry,
    KnownPath,
}

impl Source {
    /// For a folder Echo was found in.
    pub fn describe(self) -> &'static str {
        match self {
            Source::Registry => "found from the Meta client's own settings",
            Source::KnownPath => "found where the Meta client installs by default",
        }
    }

    /// For a folder Echo belongs in but need not occupy yet.
    pub fn describe_library(self) -> &'static str {
        match self {
            Source::Registry => "your Meta library, from the Meta client's own settings",
            Source::KnownPath => "the usual Meta library location",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// An install root in this app's sense: the folder that *contains*
    /// `ready-at-dawn-echo-arena`, not the game folder itself.
    pub root: PathBuf,
    pub source: Source,
}

/// The Meta client's install base, as the client itself recorded it.
///
/// `WOW6432Node` because the client is a 32-bit registration; this process is 64-bit, so
/// the redirected path has to be named explicitly rather than left to the view.
#[cfg(windows)]
pub fn client_base() -> Option<PathBuf> {
    read_registry_string(
        r"SOFTWARE\WOW6432Node\Oculus VR, LLC\Oculus",
        "Base",
    )
    .map(PathBuf::from)
}

#[cfg(not(windows))]
pub fn client_base() -> Option<PathBuf> {
    None
}

/// The folder Echo VR belongs in, whether or not it is there yet.
///
/// This is what an *install* wants, and it is deliberately not the same question as
/// [`echo_root`]. Requiring the game to be present would make the suggestion useless in
/// exactly the two cases that reach the install flow: someone who does not own it never has
/// it in a Meta library, and someone who does has just been told to delete that folder
/// before installing. The check that felt careful would have guaranteed it never fired.
///
/// So the test is the library, not the game.
pub fn library_root() -> Option<Detected> {
    if let Some(base) = client_base() {
        let root = library_of(&base);
        if root.is_dir() {
            return Some(Detected { root, source: Source::Registry });
        }
    }
    for base in KNOWN_BASES {
        let root = library_of(Path::new(base));
        if root.is_dir() {
            return Some(Detected { root, source: Source::KnownPath });
        }
    }
    None
}

/// Where Echo VR is, if the Meta client has it.
///
/// Only answers when `echovr.exe` is actually there. A folder that merely exists is not a
/// better suggestion than the neutral one, and offering it would be the app guessing out
/// loud rather than reporting something it found.
pub fn echo_root() -> Option<Detected> {
    if let Some(base) = client_base() {
        let root = library_of(&base);
        if install::exe_path(&root).is_file() {
            return Some(Detected { root, source: Source::Registry });
        }
    }
    for base in KNOWN_BASES {
        let root = library_of(Path::new(base));
        if install::exe_path(&root).is_file() {
            return Some(Detected { root, source: Source::KnownPath });
        }
    }
    None
}

/// Where the Meta client would put Echo VR, whether or not it has.
///
/// For telling someone which folder to delete, which has to be answerable before the folder
/// exists. Real when the client is installed and the registry can be read; otherwise the
/// current default, which is at least the right shape to recognise.
pub fn expected_echo_dir() -> (PathBuf, Source) {
    match client_base() {
        Some(base) => (library_of(&base).join(crate::engine::install::ARENA_DIR), Source::Registry),
        None => (
            library_of(Path::new(KNOWN_BASES[0])).join(crate::engine::install::ARENA_DIR),
            Source::KnownPath,
        ),
    }
}

/// The Oculus library id that covers `install`, or the default one.
///
/// Revive launches a title by library id plus a path relative to it, and the id is a GUID
/// the Meta client assigns to each install location. It lives in the registry, under the
/// *current user* rather than the machine, because libraries are configured per account:
///
/// ```text
/// HKCU\Software\Oculus VR, LLC\Oculus\Libraries
///     DefaultLibrary  = <guid>
///   \<guid>
///     OriginalPath    = C:\Program Files\Meta Horizon\Software
/// ```
///
/// Reading it here is what makes a first-time setup work. The original installer copies the
/// id out of some other app's entry in Revive's manifest, so it can only work once Revive
/// has already seen a library - which is why it tells people to install any free title and
/// start SteamVR before trying. There is no need to ask that of anyone.
#[cfg(windows)]
pub fn library_id_for(install: &Path) -> Option<String> {
    let out = crate::engine::hide_console(&mut std::process::Command::new("powershell"))
        .args([
            "-NoProfile",
            "-Command",
            // Single quotes throughout: the command is already inside a Rust string, and
            // nesting double quotes here is how this stops compiling.
            concat!(
                "$r='HKCU:\\Software\\Oculus VR, LLC\\Oculus\\Libraries'; ",
                "$d=(Get-ItemProperty -Path $r -EA SilentlyContinue).DefaultLibrary; ",
                "Write-Output ('default=' + $d); ",
                // One line per library: its id, then the folder it covers. Parsed rather
                // than filtered here, so the choice between them stays in Rust.
                "Get-ChildItem -Path $r -EA SilentlyContinue | ForEach-Object { ",
                "  Write-Output ($_.PSChildName + '=' + ",
                "    (Get-ItemProperty -Path $_.PSPath).OriginalPath) }",
            ),
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    Some(pick_library(&text, install)?)
}

#[cfg(not(windows))]
pub fn library_id_for(_install: &Path) -> Option<String> {
    None
}

/// Chooses among the libraries listed, given where the install actually is.
///
/// Split from the registry read so the choice can be tested: someone with two drives has
/// two libraries, and picking the wrong one produces an entry that launches nothing.
#[cfg_attr(not(windows), allow(dead_code))]
fn pick_library(listing: &str, install: &Path) -> Option<String> {
    let install = install.to_string_lossy().to_lowercase();
    let mut default = None;
    let mut best: Option<(usize, String)> = None;

    for line in listing.lines() {
        let Some((key, value)) = line.trim().split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if key.eq_ignore_ascii_case("default") {
            if !value.is_empty() {
                default = Some(value.to_string());
            }
            continue;
        }
        if key.is_empty() || value.is_empty() {
            continue;
        }
        // The library whose folder contains the install, and the longest such if more than
        // one nests inside another.
        let path = value.to_lowercase();
        if contains_path(&path, &install) && best.as_ref().is_none_or(|(n, _)| path.len() > *n) {
            best = Some((path.len(), key.to_string()));
        }
    }
    best.map(|(_, id)| id).or(default)
}

/// Is `inner` inside `folder`, as folders rather than as text?
///
/// A plain prefix test says `C:\\Meta` contains `C:\\MetaOther\\Software`, because it does as
/// a string and does not as a directory. Getting that wrong here picks the wrong library id
/// and produces an entry that launches nothing.
fn contains_path(folder: &str, inner: &str) -> bool {
    let folder = folder.trim_end_matches(['\\', '/']);
    if folder.is_empty() {
        return false;
    }
    let Some(rest) = inner.strip_prefix(folder) else { return false };
    rest.is_empty() || rest.starts_with('\\') || rest.starts_with('/')
}

fn library_of(base: &Path) -> PathBuf {
    LIBRARY.iter().fold(base.to_path_buf(), |p, part| p.join(part))
}

#[cfg(windows)]
fn read_registry_string(subkey: &str, value: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    };
    let (sub, val) = (wide(subkey), wide(value));

    unsafe {
        // Asked for the size first: the value is a path and there is no sensible fixed
        // buffer for one.
        let mut size: u32 = 0;
        let rc = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );
        if rc != ERROR_SUCCESS || size == 0 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        let rc = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            &mut size,
        );
        if rc != ERROR_SUCCESS {
            return None;
        }
        // RegGetValueW guarantees the terminator; trim it and anything past it.
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let text = String::from_utf16_lossy(&buf[..end]).trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The listing the registry read produces, in the shape it produces it.
    const TWO_LIBRARIES: &str = "default=aaaa-1111\n\
        aaaa-1111=C:\\Program Files\\Meta Horizon\\Software\n\
        bbbb-2222=D:\\Games\\Meta\\Software\n";

    #[test]
    fn the_library_containing_the_install_wins_over_the_default() {
        // Two drives means two libraries, and the wrong one produces an entry that launches
        // nothing at all.
        let id = pick_library(
            TWO_LIBRARIES,
            Path::new(r"D:\Games\Meta\Software\Software\ready-at-dawn-echo-arena"),
        );
        assert_eq!(id.as_deref(), Some("bbbb-2222"));
    }

    #[test]
    fn an_install_outside_every_library_falls_back_to_the_default() {
        // Somebody who installed to a folder of their own still gets a usable id rather
        // than nothing.
        let id = pick_library(TWO_LIBRARIES, Path::new(r"E:\Elsewhere\echovr.exe"));
        assert_eq!(id.as_deref(), Some("aaaa-1111"));
    }

    #[test]
    fn matching_ignores_case_the_way_windows_does() {
        let id = pick_library(TWO_LIBRARIES, Path::new(r"d:\GAMES\meta\software\Software\x"));
        assert_eq!(id.as_deref(), Some("bbbb-2222"));
    }

    #[test]
    fn a_library_is_not_matched_by_a_name_that_merely_starts_the_same() {
        // As text, C:\\Meta is a prefix of C:\\MetaOther. As folders it is not, and the
        // difference is an id that launches nothing.
        let listing = "default=fallback\n\
            meta=C:\\Meta\n\
            other=C:\\MetaOther\n";
        let id = pick_library(listing, Path::new(r"C:\MetaOther\Software\Software\game"));
        assert_eq!(id.as_deref(), Some("other"), "the wrong library launches nothing");
    }

    #[test]
    fn a_trailing_separator_on_a_library_path_changes_nothing() {
        // The registry has been seen writing both, and it is not the user's problem.
        let listing = "default=x\nx=C:\\Meta Horizon\\Software\\\n";
        let id = pick_library(
            listing,
            Path::new(r"C:\Meta Horizon\Software\Software\ready-at-dawn-echo-arena"),
        );
        assert_eq!(id.as_deref(), Some("x"));
    }

    #[test]
    fn the_longest_matching_library_wins() {
        // One library nested inside another: the specific one is the right answer.
        let listing = "default=outer\n\
            outer=C:\\Meta\n\
            inner=C:\\Meta\\Extra\\Software\n";
        let id = pick_library(listing, Path::new(r"C:\Meta\Extra\Software\Software\game"));
        assert_eq!(id.as_deref(), Some("inner"));
    }

    #[test]
    fn nothing_recorded_means_nothing_claimed() {
        assert_eq!(pick_library("", Path::new(r"C:\x")), None);
        assert_eq!(pick_library("default=\n", Path::new(r"C:\x")), None);
    }

    #[test]
    fn a_library_is_suggested_even_with_no_game_in_it() {
        // The case that broke the first version of this: installing is exactly when Echo is
        // not there. A new player never has it in a Meta library, and an owner has just been
        // told to delete Meta's copy. Requiring the game made the suggestion unreachable.
        let dir = std::env::temp_dir().join(format!("evrce-lib-{}", std::process::id()));
        let lib = library_of(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&lib).unwrap();

        assert!(lib.is_dir(), "an empty library still counts");
        assert!(
            !crate::engine::install::exe_path(&lib).is_file(),
            "and it is empty, which is the whole point"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_two_questions_are_worded_differently() {
        // "found from the Meta client's settings" is a claim about the game; a library that
        // may be empty needs different words or it reads as a promise the folder does not
        // keep.
        for s in [Source::Registry, Source::KnownPath] {
            assert_ne!(s.describe(), s.describe_library());
            assert!(s.describe_library().contains("library"), "got {}", s.describe_library());
        }
    }

    #[test]
    fn the_library_is_two_levels_below_the_base() {
        // Getting this wrong by one level is the easy mistake: the root this app wants is
        // the folder that contains ready-at-dawn-echo-arena, not the game folder.
        let root = library_of(Path::new(r"C:\Program Files\Meta Horizon"));
        assert!(root.ends_with(Path::new("Software").join("Software")), "got {root:?}");
        assert!(
            install::exe_path(&root)
                .to_string_lossy()
                .ends_with(&format!("{}", Path::new(install::ARENA_DIR).join("bin").join("win10").join("echovr.exe").display())),
            "the exe must sit under the app folder inside the library"
        );
    }

    #[test]
    fn nothing_is_suggested_when_echo_is_not_there() {
        // On a machine with no Meta client and no Echo, this must stay quiet rather than
        // offer a path that does not exist.
        assert_eq!(echo_root(), None);
    }

    #[test]
    fn every_source_can_explain_itself() {
        for s in [Source::Registry, Source::KnownPath] {
            assert!(!s.describe().is_empty());
        }
    }
}
