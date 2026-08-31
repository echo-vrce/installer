// SPDX-License-Identifier: GPL-3.0-or-later
//! Support tools: collecting logs off a headset, and clearing what the installer cached.
//!
//! The log bundle exists because the first thing anyone is asked in a help channel is to
//! post their logs, and the answer is usually a folder they cannot find on a device with no
//! file manager. This gathers them into one zip that can be dragged into Discord.
//!
//! What goes in was decided by looking at a headset rather than by copying the original,
//! which pulls two hardcoded directories, one of which no longer exists. Alongside the
//! game's own logs it collects the asset-patch log and the install marker, because "which
//! build is this and how did it get here" is the question the logs cannot answer on their
//! own.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::engine::quest::{self, Quest};

/// Where Echo writes its client logs. Confirmed on a Quest 2; the second path the original
/// pulls from belongs to an older layout and is not there any more.
const LOG_DIR: &str = "/sdcard/r14logs";
const ASSET_LOG: &str = "/sdcard/Android/media/com.readyatdawn.r15/asset_patches/assetpatch.log";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub path: PathBuf,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug)]
pub enum Error {
    Device(quest::Error),
    Io(std::io::Error),
    Zip(String),
    /// Nothing was found to collect, which is itself the answer.
    Empty,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Device(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Zip(m) => write!(f, "could not build the bundle: {m}"),
            Error::Empty => write!(
                f,
                "No logs on the headset yet. Echo writes them when it runs, so start it once \
                 and try again."
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<quest::Error> for Error {
    fn from(e: quest::Error) -> Self {
        Error::Device(e)
    }
}

/// Pulls logs off the headset and zips them, alongside a note of what was collected.
pub fn collect_logs(
    quest: &Quest<'_>,
    dest_dir: &Path,
    on_step: &mut dyn FnMut(&str),
) -> Result<Bundle, Error> {
    std::fs::create_dir_all(dest_dir)?;
    let scratch = dest_dir.join("collecting");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;

    on_step("Pulling client logs");
    // Pulled as a directory. Log filenames contain brackets and parentheses, which is
    // exactly why every adb call here is an argv and never a command string.
    let _ = quest.exec(&["pull", LOG_DIR, &scratch.to_string_lossy()]);

    on_step("Pulling the asset patch log");
    let _ = quest.exec(&["pull", ASSET_LOG, &scratch.to_string_lossy()]);

    on_step("Reading the install record");
    if let Some(marker) = quest.read_marker() {
        std::fs::write(scratch.join("install_marker.txt"), marker.serialize())?;
    }

    on_step("Noting what this headset is");
    let mut about = String::new();
    about.push_str(&format!("collected_by=echo-vrce-installer {}\n", crate::app::VERSION));
    for (label, prop) in [
        ("model", "ro.product.model"),
        ("device", "ro.product.device"),
        ("os_version", "ro.build.version.release"),
        ("build", "ro.build.display.id"),
    ] {
        if let Ok(value) = quest.exec(&["shell", "getprop", prop]) {
            about.push_str(&format!("{label}={}\n", value.trim()));
        }
    }
    if let Some(path) = quest.installed_apk_path() {
        about.push_str(&format!("apk_path={path}\n"));
    }
    if let Some(code) = quest.version_code() {
        about.push_str(&format!("version_code={code}\n"));
    }
    std::fs::write(scratch.join("headset.txt"), about)?;

    // Half of any problem is on this side of the cable. Without this the bundle explains
    // the headset perfectly and says nothing about what the installer did to it.
    on_step("Adding this installer's own log");
    if let Some(own) = crate::log::path() {
        let _ = std::fs::copy(&own, scratch.join("installer.log"));
    }

    on_step("Packing");
    let stamp = timestamp();
    let zip_path = dest_dir.join(format!("echo-logs-{stamp}.zip"));
    let (files, bytes) = zip_dir(&scratch, &zip_path)?;
    let _ = std::fs::remove_dir_all(&scratch);

    if files == 0 {
        let _ = std::fs::remove_file(&zip_path);
        return Err(Error::Empty);
    }
    Ok(Bundle { path: zip_path, files, bytes })
}

/// Zips a directory tree. Returns how many files went in and their uncompressed size.
fn zip_dir(src: &Path, dest: &Path) -> Result<(usize, u64), Error> {
    let file = std::fs::File::create(dest)?;
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path
                .strip_prefix(src)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            let data = std::fs::read(&path)?;
            // Carried across from the file, because a bundle whose every entry is dated
            // 1980 is a bundle nobody can order by time. Zip's default epoch is 1980.
            let options = match modified_at(&path) {
                Some(when) => options.last_modified_time(when),
                None => options,
            };
            zip.start_file(name, options).map_err(|e| Error::Zip(e.to_string()))?;
            zip.write_all(&data)?;
            files += 1;
            bytes += data.len() as u64;
        }
    }
    zip.finish().map_err(|e| Error::Zip(e.to_string()))?;
    Ok((files, bytes))
}

/// A file's modification time, in the shape zip stores.
fn modified_at(path: &Path) -> Option<zip::DateTime> {
    let secs = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let (y, m, d) = crate::fmt::civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    zip::DateTime::from_date_and_time(
        y as u16,
        m as u8,
        d as u8,
        (tod / 3600) as u8,
        ((tod % 3600) / 60) as u8,
        (tod % 60) as u8,
    )
    .ok()
}

/// Filesystem-safe, sorts chronologically, and readable at a glance.
/// UTC, and it says so with a trailing Z.
///
/// This name ends up in someone else's downloads folder in another timezone, so UTC is the
/// more useful stamp. The Z is there because a bare `2347` on a clock reading `0147` looks
/// like a bug to the person who just made the file.
fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = crate::fmt::civil_from_days(days);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}Z", tod / 3600, (tod % 3600) / 60)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheReport {
    pub entries: Vec<(PathBuf, u64)>,
    pub total: u64,
}

/// What the installer has left lying around: staged downloads and partial transfers.
///
/// Reported before anything is removed. The original offers a "delete cache" button that
/// walks a hardcoded list of paths and tells you afterwards what it managed to delete.
/// Where cached files live, and how much of that place may be deleted.
///
/// The distinction is the whole point. A staging directory is disposable by definition: it
/// exists only to hold things on their way somewhere else. An install root is not - it is
/// the game - and the only thing in it this app may remove is the archive it put there and
/// left behind. Sweeping that folder would delete somebody's install.
pub enum Cache<'a> {
    /// Everything in it may go.
    Disposable(&'a Path),
    /// Only these filenames, and their `.part` companions, may go.
    OnlyNamed { dir: &'a Path, names: &'a [&'a str] },
}

impl Cache<'_> {
    fn dir(&self) -> &Path {
        match self {
            Cache::Disposable(d) => d,
            Cache::OnlyNamed { dir, .. } => dir,
        }
    }

    /// Whether one entry of this directory is ours to remove.
    fn may_remove(&self, name: &str) -> bool {
        match self {
            Cache::Disposable(_) => true,
            Cache::OnlyNamed { names, .. } => names
                .iter()
                .any(|n| name == *n || name == format!("{n}.part") || name == format!("{n}.etag")),
        }
    }
}

/// The places this app leaves large files, given where it keeps its own data and where the
/// user last installed to.
///
/// The install root is included because the PC archive is downloaded into it, not into
/// staging: that avoids needing 4.68 GB twice and moving it across volumes, but it means a
/// failed install leaves the archive somewhere this app would otherwise never look.
pub fn caches<'a>(staging: &'a Path, install_root: Option<&'a Path>) -> Vec<Cache<'a>> {
    let mut out = vec![Cache::Disposable(staging)];
    if let Some(root) = install_root {
        out.push(Cache::OnlyNamed { dir: root, names: &[crate::endpoints::PC_ARCHIVE] });
    }
    out
}

pub fn cache_report(caches: &[Cache<'_>]) -> CacheReport {
    let mut report = CacheReport::default();
    for cache in caches {
        let Ok(entries) = std::fs::read_dir(cache.dir()) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !cache.may_remove(&name) {
                continue;
            }
            let size = if path.is_dir() {
                dir_size(&path)
            } else {
                entry.metadata().map(|m| m.len()).unwrap_or(0)
            };
            report.total += size;
            report.entries.push((path, size));
        }
    }
    report.entries.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    report
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(m) = entry.metadata() {
                total += m.len();
            }
        }
    }
    total
}

/// Removes the staged downloads. Returns how much was freed.
pub fn clear_cache(caches: &[Cache<'_>]) -> Result<u64, Error> {
    let mut freed = 0;
    for cache in caches {
        let Ok(entries) = std::fs::read_dir(cache.dir()) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // Asked again here rather than trusting the report: the report and the removal
            // are two passes over a directory that anything could have changed in between,
            // and the cost of being wrong is somebody's install.
            if !cache.may_remove(&name) {
                continue;
            }
            let size =
                if path.is_dir() { dir_size(&path) } else { entry.metadata().map(|m| m.len()).unwrap_or(0) };
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
            freed += size;
        }
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one that matters. A bug here deletes somebody's game, not a cached download.
    #[test]
    fn clearing_never_touches_the_install_itself() {
        let root = tmpdir("install_root");
        // What the archive left behind, and what the game is.
        std::fs::write(root.join(crate::endpoints::PC_ARCHIVE), b"archive").unwrap();
        std::fs::write(root.join(format!("{}.part", crate::endpoints::PC_ARCHIVE)), b"half").unwrap();
        std::fs::write(root.join("echovr.exe"), b"the game").unwrap();
        std::fs::write(root.join("save.json"), b"someone's settings").unwrap();
        std::fs::create_dir_all(root.join("sourcedb")).unwrap();
        std::fs::write(root.join("sourcedb/data.bin"), b"assets").unwrap();

        let caches = [Cache::OnlyNamed { dir: &root, names: &[crate::endpoints::PC_ARCHIVE] }];

        let report = cache_report(&caches);
        let named: Vec<String> = report
            .entries
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(named.len(), 2, "only the archive and its part are cache: {named:?}");

        clear_cache(&caches).unwrap();

        assert!(!root.join(crate::endpoints::PC_ARCHIVE).exists());
        assert!(!root.join(format!("{}.part", crate::endpoints::PC_ARCHIVE)).exists());
        assert!(root.join("echovr.exe").exists(), "the game must survive");
        assert!(root.join("save.json").exists(), "so must anything else in there");
        assert!(root.join("sourcedb/data.bin").exists(), "including subdirectories");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_staging_directory_is_emptied_completely() {
        let d = tmpdir("staging_all");
        std::fs::write(d.join("anything.bin"), b"x").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/more.bin"), b"yy").unwrap();

        let caches = [Cache::Disposable(&d)];
        assert_eq!(cache_report(&caches).total, 3);
        clear_cache(&caches).unwrap();
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "the directory itself stays");
        let _ = std::fs::remove_dir_all(d);
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("evrce_tools_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn reports_nothing_for_a_missing_or_empty_cache() {
        let missing = tmpdir("cache_missing").join("gone");
        assert_eq!(cache_report(&[Cache::Disposable(&missing)]), CacheReport::default());
        let empty = tmpdir("cache_empty");
        assert_eq!(cache_report(&[Cache::Disposable(&empty)]).total, 0);
        std::fs::remove_dir_all(empty).ok();
    }

    #[test]
    fn reports_sizes_largest_first() {
        let dir = tmpdir("cache_sizes");
        std::fs::write(dir.join("small.part"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("big.apk"), vec![0u8; 5000]).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("nested"), vec![0u8; 700]).unwrap();

        let report = cache_report(&[Cache::Disposable(&dir)]);
        assert_eq!(report.total, 5800);
        assert_eq!(report.entries.len(), 3);
        assert!(report.entries[0].0.ends_with("big.apk"), "largest should be first");
        assert_eq!(report.entries[0].1, 5000);
        // Directories are summed, not counted as one entry of zero.
        let sub = report.entries.iter().find(|(p, _)| p.ends_with("sub")).unwrap();
        assert_eq!(sub.1, 700);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn clearing_empties_the_directory_but_keeps_it() {
        let dir = tmpdir("cache_clear");
        std::fs::write(dir.join("a"), vec![0u8; 300]).unwrap();
        std::fs::create_dir(dir.join("b")).unwrap();
        std::fs::write(dir.join("b").join("c"), vec![0u8; 200]).unwrap();

        assert_eq!(clear_cache(&[Cache::Disposable(&dir)]).unwrap(), 500);
        assert!(dir.is_dir(), "the staging directory itself should survive");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        // Clearing again is not an error.
        assert_eq!(clear_cache(&[Cache::Disposable(&dir)]).unwrap(), 0);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn zips_a_tree_and_preserves_relative_names() {
        let dir = tmpdir("zip");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("logs")).unwrap();
        std::fs::write(src.join("headset.txt"), b"model=Quest 2").unwrap();
        // A name with the brackets Echo actually uses.
        std::fs::write(src.join("logs").join("[r14(client)]-[x].log"), b"line").unwrap();

        let out = dir.join("bundle.zip");
        let (files, bytes) = zip_dir(&src, &out).unwrap();
        assert_eq!(files, 2);
        assert_eq!(bytes, 13 + 4);

        let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"headset.txt".to_string()));
        assert!(
            names.iter().any(|n| n == "logs/[r14(client)]-[x].log"),
            "bracketed names must survive: {names:?}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// A support bundle whose entries all claim 1980 cannot be ordered by time.
    #[test]
    fn zip_entries_carry_a_real_date() {
        let dir = tmpdir("zipdate");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.log"), b"x").unwrap();

        let out = dir.join("b.zip");
        zip_dir(&src, &out).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let year = zip.by_index(0).unwrap().last_modified().unwrap().year();
        assert!(year >= 2026, "entry is dated {year}, which is zip's default epoch");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn timestamps_sort_chronologically() {
        let t = timestamp();
        assert_eq!(t.len(), 14, "got {t}");
        assert_eq!(&t[8..9], "-");
        assert!(t.ends_with('Z'), "got {t}");
        assert!(t[..8].chars().all(|c| c.is_ascii_digit()));
        // Sorting the names has to sort the bundles, which is the whole point of the shape.
        assert!("20260101-0000Z" < "20260101-0001Z");
    }
}
