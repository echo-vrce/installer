// SPDX-License-Identifier: GPL-3.0-or-later
//! What an Echo VR install looks like on disk, and what we can honestly say about a
//! folder the user typed.
//!
//! Nothing here resolves, corrects or guesses a path. The original walks up and down from
//! whatever folder was picked looking for the game, then silently rewrites the field with
//! what it found. This does the opposite: the user owns the path, and the app only reports
//! what it sees at exactly that location.

use std::fs;
use std::path::{Path, PathBuf};

/// The client always lives at `<root>/ready-at-dawn-echo-arena/bin/win10/echovr.exe`.
pub const ARENA_DIR: &str = "ready-at-dawn-echo-arena";
const EXE_NAME: &str = "echovr.exe";

/// Where PC update files are placed, relative to the install root.
pub fn bin_dir(root: &Path) -> PathBuf {
    root.join(ARENA_DIR).join("bin").join("win10")
}

/// The install root for any path that names part of an install.
///
/// The field asks for a folder and people give the folder the game is in, which is the
/// sensible reading of the question and the wrong answer to it: everything here is built
/// from a *root*, the folder that contains `ready-at-dawn-echo-arena`. Someone who pastes
/// the path of `echovr.exe`, or the `win10` folder it sits in, was not being careless.
///
/// So all of these resolve to the same root, and anything else is left exactly as typed:
///
/// - `D:\Games`                                              (already a root)
/// - `D:\Games\ready-at-dawn-echo-arena`
/// - `D:\Games\ready-at-dawn-echo-arena\bin`
/// - `D:\Games\ready-at-dawn-echo-arena\bin\win10`
pub fn root_of(path: &Path) -> Option<PathBuf> {
    if exe_path(path).is_file() {
        return Some(path.to_path_buf());
    }
    // Walk up while the exe turns up at the root that would imply. Three is the depth of
    // `<arena>/bin/win10`; stopping there keeps this from wandering up a whole drive.
    let mut candidate = path.to_path_buf();
    for _ in 0..3 {
        candidate = candidate.parent()?.to_path_buf();
        if exe_path(&candidate).is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn exe_path(root: &Path) -> PathBuf {
    bin_dir(root).join(EXE_NAME)
}

/// Everything the path step can tell the user, and nothing it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    /// Is there an Echo install at exactly this root?
    pub has_echo: bool,
    /// Does the game folder exist, whatever is inside it?
    ///
    /// Separate from `has_echo` on purpose, and it is the one that governs deleting. An
    /// install that was interrupted, cancelled, corrupted, or a folder someone made by
    /// hand all have the directory and not the executable - and those are exactly the cases
    /// most likely to be installed over. Asking only when a *valid* install is present
    /// meant the folder was removed without anyone being asked.
    pub arena_exists: bool,
    /// Does the folder exist at all? Distinguishes "wrong path" from "not installed yet".
    pub root_exists: bool,
    /// Can we actually write there? Tested by writing, because file metadata does not
    /// answer this reliably on Windows, and a wrong answer here means a failure halfway
    /// through an update instead of before it starts.
    pub writable: bool,
    /// Free space on the containing volume. `None` when it could not be determined; the
    /// UI omits the line rather than guessing.
    pub free_bytes: Option<u64>,
}

pub fn inspect(root: &Path) -> Inspection {
    let root_exists = root.is_dir();
    Inspection {
        has_echo: exe_path(root).is_file(),
        arena_exists: root.join(ARENA_DIR).is_dir(),
        root_exists,
        writable: root_exists && is_writable(root),
        free_bytes: free_space(root),
    }
}

/// Writes and removes a probe file. The only honest test.
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".evrce_write_probe");
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Free space on the volume holding `dir`.
///
/// Walks up to the nearest directory that exists first, because the interesting case is a
/// folder the user has typed but not created yet: `C:\\EchoVR` before a first install still
/// has a meaningful answer, and both platform calls fail outright on a missing path.
fn free_space(dir: &Path) -> Option<u64> {
    let mut probe = dir;
    loop {
        if probe.is_dir() {
            return volume_free(probe);
        }
        probe = probe.parent()?;
    }
}

#[cfg(unix)]
fn volume_free(dir: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    // f_bavail, not f_bfree: the former is what an unprivileged process may actually use,
    // the latter includes the reserve only root can touch.
    (stat.f_bavail as u64).checked_mul(stat.f_frsize as u64)
}

#[cfg(windows)]
fn volume_free(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut available: u64 = 0;
    // The first out-parameter is bytes available *to the caller*, which respects disk
    // quotas. The other two are the volume totals and are not what we want to show.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        None
    } else {
        Some(available)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_part_of_an_install_resolves_to_its_root() {
        // Pasting the folder echovr.exe is in is the obvious answer to "Echo VR folder",
        // and it used to be reported as "no echovr.exe here" while the user was staring at
        // the file.
        let root = tmpdir("root_of");
        std::fs::create_dir_all(bin_dir(&root)).unwrap();
        std::fs::write(exe_path(&root), b"game").unwrap();

        for inside in [
            root.clone(),
            root.join(ARENA_DIR),
            root.join(ARENA_DIR).join("bin"),
            bin_dir(&root),
        ] {
            assert_eq!(root_of(&inside).as_deref(), Some(root.as_path()), "from {inside:?}");
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_path_with_no_install_under_or_above_it_is_left_alone() {
        // Guessing here would be worse than saying nothing: it would move someone's chosen
        // folder somewhere they did not ask for.
        let dir = tmpdir("root_of_none");
        std::fs::create_dir_all(dir.join("a").join("b").join("c")).unwrap();
        assert_eq!(root_of(&dir.join("a").join("b").join("c")), None);
        assert_eq!(root_of(&dir), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn it_does_not_climb_past_the_depth_of_an_install() {
        // Three levels is exactly <arena>/bin/win10. More than that and a wrong path could
        // resolve to an install several folders away that has nothing to do with it.
        let root = tmpdir("root_of_depth");
        std::fs::create_dir_all(bin_dir(&root)).unwrap();
        std::fs::write(exe_path(&root), b"game").unwrap();
        let too_deep = bin_dir(&root).join("x").join("y");
        std::fs::create_dir_all(&too_deep).unwrap();
        assert_eq!(root_of(&too_deep), None, "that is further away than an install is deep");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_folder_without_the_game_is_still_a_folder() {
        // The two flags answer different questions, and conflating them is what let a
        // directory be deleted without anyone being asked about it.
        let dir = tmpdir("insp_arena");
        std::fs::create_dir_all(dir.join(ARENA_DIR)).unwrap();
        std::fs::write(dir.join(ARENA_DIR).join("foo.txt"), b"x").unwrap();

        let i = inspect(&dir);
        assert!(i.arena_exists, "the folder is there");
        assert!(!i.has_echo, "but it is not an install");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_real_install_answers_both() {
        let dir = tmpdir("insp_both");
        std::fs::create_dir_all(bin_dir(&dir)).unwrap();
        std::fs::write(exe_path(&dir), b"game").unwrap();

        let i = inspect(&dir);
        assert!(i.arena_exists && i.has_echo);
        std::fs::remove_dir_all(dir).ok();
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("evrce_install_{}_{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn derives_the_bin_directory() {
        let root = Path::new("/tmp/EchoVR");
        assert_eq!(bin_dir(root), PathBuf::from("/tmp/EchoVR/ready-at-dawn-echo-arena/bin/win10"));
        assert_eq!(
            exe_path(root),
            PathBuf::from("/tmp/EchoVR/ready-at-dawn-echo-arena/bin/win10/echovr.exe")
        );
    }

    #[test]
    fn reports_a_missing_folder_without_pretending_otherwise() {
        let missing = tmpdir("missing").join("nope");
        let i = inspect(&missing);
        assert!(!i.root_exists);
        assert!(!i.has_echo);
        assert!(!i.writable, "a folder that does not exist is not writable");
    }

    #[test]
    fn distinguishes_an_empty_folder_from_an_install() {
        let dir = tmpdir("empty");
        let i = inspect(&dir);
        assert!(i.root_exists);
        assert!(!i.has_echo, "empty folder is not an install");
        assert!(i.writable);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn finds_an_install_only_at_the_exact_root() {
        let dir = tmpdir("exact");
        let bin = bin_dir(&dir);
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join(EXE_NAME), b"MZ").unwrap();

        assert!(inspect(&dir).has_echo);
        // Deliberately not searched for: the parent, and the arena folder itself. The
        // original would have walked to either and rewritten the user's path.
        assert!(!inspect(dir.parent().unwrap()).has_echo);
        assert!(!inspect(&dir.join(ARENA_DIR)).has_echo);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reports_free_space_for_an_existing_directory() {
        let dir = tmpdir("free");
        let free = inspect(&dir).free_bytes.expect("temp dir should report free space");
        assert!(free > 0, "a writable temp dir with zero bytes free is not credible");
        fs::remove_dir_all(dir).ok();
    }

    /// The case that matters for a first install: the folder does not exist yet, but the
    /// volume it would live on still has an answer.
    #[test]
    fn reports_free_space_through_a_missing_directory() {
        let dir = tmpdir("free_missing");
        let nested = dir.join("not").join("created").join("yet");
        let i = inspect(&nested);
        assert!(!i.root_exists);
        assert!(i.free_bytes.is_some(), "should have walked up to an existing parent");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn write_probe_leaves_nothing_behind() {
        let dir = tmpdir("probe");
        assert!(inspect(&dir).writable);
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert!(leftovers.is_empty(), "probe file was not cleaned up");
        fs::remove_dir_all(dir).ok();
    }
}
