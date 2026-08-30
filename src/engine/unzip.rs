// SPDX-License-Identifier: GPL-3.0-or-later
//! Zip extraction, with the path guard the original does not have.
//!
//! `UnzipFile.java` builds its destination as `destDirectory + File.separator +
//! entry.getName()` and writes there, unvalidated. An archive whose entries are named
//! `../../something` therefore writes outside the destination directory. That is
//! reachable rather than theoretical: the licence-patch flow accepts a download URL the
//! user pastes, so the archive is not necessarily one of Echo's.
//!
//! Two guards here. Entry names go through the zip crate's `enclosed_name`, which refuses
//! anything that escapes the root or is absolute, and symlink entries are refused
//! outright. The second matters because `enclosed_name` alone does not stop the two-step
//! version of the attack: extract a symlink `dir -> /etc`, then extract `dir/passwd`
//! through it.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::engine::Cancel;

/// Does this entry name claim to be absolute?
///
/// The zip crate's `enclosed_name` does guarantee containment, but it gets there by
/// *normalising* a leading `/` or a `C:` prefix away rather than refusing it, despite its
/// documentation saying "It can't be an absolute path". Containment is the property that
/// matters for safety, so that alone would be enough to be safe.
///
/// This check exists for honesty rather than safety: without it, an archive naming
/// `/etc/passwd` gets quietly relocated to `<dest>/etc/passwd` and nobody is told. An
/// absolute name means the archive is malformed or hostile, and either is worth reporting.
fn name_is_absolute(name: &str) -> bool {
    let bytes = name.as_bytes();
    if name.starts_with('/') || name.starts_with('\\') {
        return true;
    }
    // Drive-qualified, as in `C:\Windows\...`.
    matches!(bytes, [c, b':', ..] if c.is_ascii_alphabetic())
}

/// S_IFLNK. A zip entry can carry unix permissions, and a symlink is not something a
/// game archive needs.
const MODE_SYMLINK: u32 = 0o120_000;
const MODE_TYPE_MASK: u32 = 0o170_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Zip(zip::result::ZipError),
    /// Entry name escapes the destination, or is absolute.
    UnsafeEntry(String),
    Symlink(String),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Zip(e) => write!(f, "{e}"),
            Error::UnsafeEntry(n) => {
                write!(f, "archive contains an entry that escapes the target folder: {n}")
            }
            Error::Symlink(n) => write!(f, "archive contains a symlink, which is refused: {n}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        Error::Zip(e)
    }
}

/// Total uncompressed size, read from the central directory. Cheap, and it gives the
/// progress bar a denominator before any byte is written.
pub fn uncompressed_size(archive: &Path) -> Result<u64, Error> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
    let mut total = 0u64;
    for i in 0..zip.len() {
        total += zip.by_index_raw(i)?.size();
    }
    Ok(total)
}

/// Extracts `archive` into `dest`, reporting (bytes done, bytes total).
pub fn extract(
    archive: &Path,
    dest: &Path,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<Summary, Error> {
    let total = uncompressed_size(archive)?;
    let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
    fs::create_dir_all(dest)?;

    let mut done = 0u64;
    let mut files = 0usize;
    on_progress(0, total);

    for i in 0..zip.len() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut entry = zip.by_index(i)?;
        let raw_name = entry.name().to_string();

        if let Some(mode) = entry.unix_mode() {
            if mode & MODE_TYPE_MASK == MODE_SYMLINK {
                return Err(Error::Symlink(raw_name));
            }
        }

        if name_is_absolute(&raw_name) {
            return Err(Error::UnsafeEntry(raw_name));
        }
        // enclosed_name returns None for anything that escapes the root. A `..` that stays
        // inside, as in `foo/../bar`, is fine and is resolved rather than refused.
        let relative: PathBuf = match entry.enclosed_name() {
            Some(p) => p,
            None => return Err(Error::UnsafeEntry(raw_name)),
        };
        // A DOS device name is not a path escape, but on Windows it is worse than an
        // error: `File::create` on `nul` succeeds and writes nowhere, so the entry appears
        // to extract and simply is not there.
        if relative
            .components()
            .any(|c| crate::engine::manifest::is_reserved_device_name(&c.as_os_str().to_string_lossy()))
        {
            return Err(Error::UnsafeEntry(raw_name));
        }
        let out = dest.join(&relative);

        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }

        let mode = entry.unix_mode();
        let mut file = io::BufWriter::new(File::create(&out)?);
        // Not io::copy: this needs a cancel check and a progress tick per chunk.
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let read = io::Read::read(&mut entry, &mut buf)?;
            if read == 0 {
                break;
            }
            io::Write::write_all(&mut file, &buf[..read])?;
            done += read as u64;
            on_progress(done, total);
        }
        io::Write::flush(&mut file)?;
        drop(file);
        // Carry the executable bit across. Without this an extracted adb is not runnable,
        // which is a confusing way to discover that a zip stores permissions and this did
        // not apply them.
        apply_mode(&out, mode)?;
        files += 1;
    }

    Ok(Summary { files, bytes: done })
}

/// Applies the executable bit from a zip entry's stored unix mode. Everything else about
/// the mode is ignored: honouring, say, a stored setuid bit from an untrusted archive would
/// be an odd thing to volunteer for.
#[cfg(unix)]
fn apply_mode(path: &Path, mode: Option<u32>) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let Some(mode) = mode else { return Ok(()) };
    if mode & 0o111 == 0 {
        return Ok(());
    }
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: Option<u32>) -> Result<(), Error> {
    // Windows has no equivalent bit; whether a file runs is decided by its extension.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("evrce_unzip_{}_{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Builds an archive with the given (name, contents) entries, writing names verbatim
    /// so a traversal name can actually be produced.
    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn extracts_nested_entries() {
        let dir = tmpdir("ok");
        let zip_path = dir.join("a.zip");
        make_zip(&zip_path, &[("top.txt", b"one"), ("sub/deep/inner.bin", b"twotwo")]);

        let out = dir.join("out");
        let mut seen: Vec<(u64, u64)> = Vec::new();
        let s = extract(&zip_path, &out, &Cancel::new(), &mut |d, t| seen.push((d, t))).unwrap();

        assert_eq!(s.files, 2);
        assert_eq!(s.bytes, 9);
        assert_eq!(fs::read_to_string(out.join("top.txt")).unwrap(), "one");
        assert_eq!(fs::read_to_string(out.join("sub/deep/inner.bin")).unwrap(), "twotwo");
        // Progress is reported, starting with the total known up front.
        assert_eq!(seen.first().unwrap().1, 9);
        assert_eq!(seen.last().unwrap(), &(9, 9));
        fs::remove_dir_all(dir).ok();
    }

    /// The vulnerability the original has. The archive must be refused, and crucially
    /// nothing may exist outside the destination afterwards.
    #[test]
    fn refuses_directory_traversal_entries() {
        let dir = tmpdir("slip");
        let zip_path = dir.join("evil.zip");
        make_zip(&zip_path, &[("../escaped.txt", b"pwned")]);

        let out = dir.join("out");
        let err = extract(&zip_path, &out, &Cancel::new(), &mut |_, _| {}).unwrap_err();
        assert!(matches!(err, Error::UnsafeEntry(_)), "got {err:?}");
        assert!(!dir.join("escaped.txt").exists(), "wrote outside the destination");
        fs::remove_dir_all(dir).ok();
    }

    /// Absolute entry names have to be forged rather than written: `ZipWriter` normalises
    /// a leading slash away, though it happily preserves `..`. So the archive is built with
    /// a placeholder name and the bytes are patched afterwards, using a replacement of the
    /// same length so every stored offset stays valid.
    ///
    /// Worth testing even though a compliant writer will not produce it: the guard exists
    /// for archives that did not come from a compliant writer.
    #[test]
    fn refuses_absolute_entries() {
        const PLACEHOLDER: &[u8] = b"AAAAAAAAAAAA";
        const ABSOLUTE: &[u8] = b"/etc/passwdX";
        assert_eq!(PLACEHOLDER.len(), ABSOLUTE.len());

        let dir = tmpdir("abs");
        let zip_path = dir.join("abs.zip");
        make_zip(&zip_path, &[(std::str::from_utf8(PLACEHOLDER).unwrap(), b"x")]);

        let raw = fs::read(&zip_path).unwrap();
        let mut patched = Vec::with_capacity(raw.len());
        let mut i = 0;
        let mut hits = 0;
        while i < raw.len() {
            if raw[i..].starts_with(PLACEHOLDER) {
                patched.extend_from_slice(ABSOLUTE);
                i += PLACEHOLDER.len();
                hits += 1;
            } else {
                patched.push(raw[i]);
                i += 1;
            }
        }
        // Local header and central directory both carry the name.
        assert_eq!(hits, 2, "expected the name twice in the archive");
        fs::write(&zip_path, &patched).unwrap();

        let out = dir.join("out");
        let err = extract(&zip_path, &out, &Cancel::new(), &mut |_, _| {}).unwrap_err();
        assert!(matches!(err, Error::UnsafeEntry(_)), "got {err:?}");
        // Both halves matter: refused, and nothing written under the destination either.
        assert!(!Path::new("/etc/passwdX").exists());
        assert!(!out.join("etc/passwdX").exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn refuses_a_windows_drive_qualified_entry() {
        assert!(name_is_absolute("C:\\Windows\\evil.dll"));
        assert!(name_is_absolute("/etc/passwd"));
        assert!(name_is_absolute("\\\\server\\share\\x"));
        assert!(!name_is_absolute("plugins/NvrAssetPatches.dll"));
        assert!(!name_is_absolute("libstdc++-6.dll"));
    }

    /// A `..` that stays inside the archive root is legal and has to keep working: it
    /// appears in real archives, and refusing it would break extraction for no gain.
    #[test]
    fn allows_a_parent_component_that_stays_inside() {
        let dir = tmpdir("inner_parent");
        let zip_path = dir.join("p.zip");
        make_zip(&zip_path, &[("foo/../bar.txt", b"ok")]);

        let out = dir.join("out");
        let s = extract(&zip_path, &out, &Cancel::new(), &mut |_, _| {}).unwrap();
        assert_eq!(s.files, 1);
        assert_eq!(fs::read_to_string(out.join("bar.txt")).unwrap(), "ok");
        fs::remove_dir_all(dir).ok();
    }

    /// An extracted adb has to be runnable. The zip stores the bit; extraction has to
    /// carry it across.
    #[cfg(unix)]
    #[test]
    fn preserves_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("mode");
        let zip_path = dir.join("m.zip");

        let file = File::create(&zip_path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let exe: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);
        let plain: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o644);
        w.start_file("adb", exe).unwrap();
        w.write_all(b"#!/bin/sh\n").unwrap();
        w.start_file("NOTICE.txt", plain).unwrap();
        w.write_all(b"text").unwrap();
        w.finish().unwrap();

        let out = dir.join("out");
        extract(&zip_path, &out, &Cancel::new(), &mut |_, _| {}).unwrap();

        let adb = fs::metadata(out.join("adb")).unwrap().permissions().mode();
        assert!(adb & 0o111 != 0, "extracted binary is not executable: {adb:o}");
        let txt = fs::metadata(out.join("NOTICE.txt")).unwrap().permissions().mode();
        assert!(txt & 0o111 == 0, "a plain file should not have gained the bit: {txt:o}");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reports_total_before_extracting() {
        let dir = tmpdir("size");
        let zip_path = dir.join("s.zip");
        make_zip(&zip_path, &[("a", b"12345"), ("b", b"678")]);
        assert_eq!(uncompressed_size(&zip_path).unwrap(), 8);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cancels_between_chunks() {
        let dir = tmpdir("cancel");
        let zip_path = dir.join("c.zip");
        make_zip(&zip_path, &[("a", b"hello")]);
        let cancel = Cancel::new();
        cancel.cancel();
        let err = extract(&zip_path, &dir.join("out"), &cancel, &mut |_, _| {}).unwrap_err();
        assert!(matches!(err, Error::Cancelled));
        fs::remove_dir_all(dir).ok();
    }
}
