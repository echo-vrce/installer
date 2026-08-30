// SPDX-License-Identifier: GPL-3.0-or-later
//! SHA-256 over files.
//!
//! Streamed rather than read whole: the PC client archive is 4.68 GiB and the Quest APK is
//! about 96 MB, so nothing here may assume a file fits in memory. The original Java did
//! `Files.readAllBytes` in one of its two hashing paths, which is a 4.68 GiB allocation
//! waiting to happen.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

/// 1 MiB. Large enough that the syscall overhead disappears, small enough to stay out of
/// the way. The Java used 8 KiB, which costs about 600k reads on the client archive.
const CHUNK: usize = 1024 * 1024;

/// Lowercase, zero padded hex digest of a file.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Case insensitive comparison against an expected digest. A `None` expectation is not
/// "matches"; callers that have no hash to check against should not be calling this.
pub fn sha256_matches(path: &Path, expected: &str) -> io::Result<bool> {
    Ok(sha256_file(path)?.eq_ignore_ascii_case(expected))
}

/// Incremental hasher, so a download can be verified as it streams instead of costing a
/// second full read of the file afterwards.
pub struct Rolling(Sha256);

impl Rolling {
    pub fn new() -> Self {
        Rolling(Sha256::new())
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// Feeds the first `len` bytes of an existing file into the hasher. Needed when a
    /// partial download is resumed: the bytes already on disk still have to be hashed.
    pub fn absorb_prefix(&mut self, path: &Path, len: u64) -> io::Result<()> {
        let mut file = File::open(path)?;
        let mut remaining = len;
        let mut buf = vec![0u8; CHUNK];
        while remaining > 0 {
            let want = remaining.min(CHUNK as u64) as usize;
            let read = file.read(&mut buf[..want])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial file is shorter than its recorded length",
                ));
            }
            self.0.update(&buf[..read]);
            remaining -= read as u64;
        }
        Ok(())
    }

    pub fn finish(self) -> String {
        hex(&self.0.finalize())
    }
}

impl Default for Rolling {
    fn default() -> Self {
        Self::new()
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// NIST vectors, so a broken wiring of the crate shows up rather than a
    /// self-consistent wrong answer.
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn temp(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("evrce_hash_{}_{}", std::process::id(), name));
        let mut f = File::create(&p).unwrap();
        f.write_all(contents).unwrap();
        p
    }

    #[test]
    fn hashes_an_empty_file() {
        let p = temp("empty", b"");
        assert_eq!(sha256_file(&p).unwrap(), EMPTY);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn hashes_a_known_vector() {
        let p = temp("abc", b"abc");
        assert_eq!(sha256_file(&p).unwrap(), ABC);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn matches_is_case_insensitive() {
        let p = temp("case", b"abc");
        assert!(sha256_matches(&p, ABC).unwrap());
        assert!(sha256_matches(&p, &ABC.to_uppercase()).unwrap());
        assert!(!sha256_matches(&p, EMPTY).unwrap());
        std::fs::remove_file(p).ok();
    }

    /// The point of Rolling: hashing in pieces has to equal hashing in one go, including
    /// across a chunk larger than the read buffer.
    #[test]
    fn rolling_equals_whole_file() {
        let data: Vec<u8> = (0..(CHUNK * 2 + 1234)).map(|i| (i % 251) as u8).collect();
        let p = temp("rolling", &data);

        let mut r = Rolling::new();
        r.update(&data[..1000]);
        r.update(&data[1000..CHUNK + 7]);
        r.update(&data[CHUNK + 7..]);
        assert_eq!(r.finish(), sha256_file(&p).unwrap());
        std::fs::remove_file(p).ok();
    }

    /// The resume case: absorb what is on disk, then carry on with the rest.
    #[test]
    fn absorb_prefix_then_continue() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 97) as u8).collect();
        let whole = temp("whole", &data);
        let part = temp("part", &data[..2048]);

        let mut r = Rolling::new();
        r.absorb_prefix(&part, 2048).unwrap();
        r.update(&data[2048..]);
        assert_eq!(r.finish(), sha256_file(&whole).unwrap());

        std::fs::remove_file(whole).ok();
        std::fs::remove_file(part).ok();
    }

    #[test]
    fn absorb_prefix_rejects_a_short_file() {
        let p = temp("short", b"12345");
        let mut r = Rolling::new();
        assert!(r.absorb_prefix(&p, 500).is_err());
        std::fs::remove_file(p).ok();
    }
}
