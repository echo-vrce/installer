// SPDX-License-Identifier: GPL-3.0-or-later
//! Downloads: ranged, resumable, verified as they stream, and cancellable.
//!
//! Differences from `Downloader.java` that are deliberate rather than incidental:
//!
//! - Bytes land in a `.part` file and are renamed into place only after verification, so
//!   a truncated file can never be mistaken for a finished one. The Java streams straight
//!   into the destination and decides whether to resume by comparing the local size to
//!   the remote size, which cannot tell "half a download" from "a different build".
//! - The hash is computed while streaming instead of by reading the finished file again.
//!   On the 4.68 GiB client archive that is a whole second pass saved.
//! - 1 MiB buffers rather than 1 KiB. The Java's buffer costs roughly five million read
//!   calls on that archive, and it repainted the UI on every one of them.
//! - A 404 from a signed Discord CDN link is reported as a likely expiry, because that is
//!   what it almost always is: the link is signed and good for exactly 24 hours. DOCS.md,
//!   under "The Discord licence patch", has the timestamps it was read off.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::engine::hash::{self, Rolling};
use crate::engine::Cancel;

const CHUNK: usize = 1024 * 1024;
/// Progress is data, but it is not worth more than about ten updates a second. The Java
/// set a label and repainted the frame on every 1 KiB, from the download thread.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Spec {
    pub url: String,
    /// Final location. Work happens in a sibling `.part` file.
    pub dest: PathBuf,
    /// Lowercase hex. When present it is enforced, and a matching file already at `dest`
    /// short-circuits the whole download.
    pub expected_sha256: Option<String>,
    pub resume: bool,
}

impl Spec {
    pub fn new(url: impl Into<String>, dest: impl Into<PathBuf>) -> Self {
        Spec {
            url: url.into(),
            dest: dest.into(),
            expected_sha256: None,
            resume: true,
        }
    }

    pub fn with_sha256(mut self, sha: impl Into<String>) -> Self {
        self.expected_sha256 = Some(sha.into());
        self
    }

    pub fn part_path(&self) -> PathBuf {
        let mut name = self.dest.file_name().unwrap_or_default().to_os_string();
        name.push(".part");
        self.dest.with_file_name(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub done: u64,
    pub total: Option<u64>,
    pub bytes_per_sec: f64,
    /// 0 on the first go, 1 upward after a transient failure was retried. Carried here so
    /// both the window and the terminal can say "retrying" without a second callback.
    pub attempt: u32,
}

impl Snapshot {
    pub fn fraction(&self) -> Option<f32> {
        match self.total {
            Some(t) if t > 0 => Some((self.done as f64 / t as f64) as f32),
            _ => None,
        }
    }

    /// None when there is no total, or no measured rate to divide by.
    pub fn eta(&self) -> Option<Duration> {
        let total = self.total?;
        if self.bytes_per_sec <= 0.0 || self.done > total {
            return None;
        }
        let remaining = (total - self.done) as f64 / self.bytes_per_sec;
        Some(Duration::from_secs_f64(remaining.min(u32::MAX as f64)))
    }
}

#[derive(Debug)]
pub enum Error {
    Network(String),
    Status { code: u16, likely_expired: bool },
    Io(io::Error),
    HashMismatch { expected: String, actual: String },
    /// The connection ended before the announced number of bytes arrived. Without this the
    /// short file would be renamed into place and, for a payload with no published hash,
    /// nothing downstream would ever notice.
    Truncated { got: u64, expected: u64 },
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // ureq prefixes its own messages, so "network error: io: ..." was routine.
            // The transport's wording is kept because it does distinguish DNS from refused
            // from timed out, but only once and without the stutter.
            Error::Network(m) => {
                let m = m.trim_start_matches("io: ").trim();
                // Already retried several times by the time anyone reads this, so the
                // useful part is that nothing was lost and starting again is cheap.
                write!(
                    f,
                    "could not reach the server: {m}\n\nCheck the connection and try \
                     again. Whatever downloaded is kept, so it will carry on rather than \
                     start over."
                )
            }
            Error::Status { code, likely_expired: true } => write!(
                f,
                "the download link is gone (HTTP {code}). Discord patch links expire after \
                 24 hours, so generate a new one."
            ),
            // 404 on a payload means the file moved or the mirror is incomplete, and no
            // amount of retrying fixes either. Another mirror might have it, so that is
            // what to say.
            Error::Status { code: 404, .. } => write!(
                f,
                "the server does not have that file (HTTP 404). It may have moved, or that \
                 mirror may be incomplete. Try again to pick a different server; if it \
                 keeps happening, ask on the EchoVRCE Discord."
            ),
            Error::Status { code, .. } if (500..600).contains(code) => write!(
                f,
                "the server had a problem (HTTP {code}). That is on their side, so waiting \
                 and trying again is usually all it takes."
            ),
            Error::Status { code, .. } => write!(f, "server returned HTTP {code}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::HashMismatch { .. } => write!(
                f,
                "the downloaded file does not match its expected checksum, so it was discarded"
            ),
            Error::Truncated { got, expected } => write!(
                f,
                "the download ended early: {got} of {expected} bytes. Check the connection \
                 and try again; it will resume."
            ),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Downloaded,
    /// `dest` was already there with the expected hash, so nothing was fetched.
    AlreadyPresent,
}

/// A 404 on a URL carrying Discord's signed CDN parameters means the signature expired far
/// more often than it means the file never existed: `ex`/`is`/`hm` are valid for exactly
/// 24 hours, and an expired or tampered signature answers 404 rather than 403. Reporting a
/// bare "not found" sends people looking for the wrong problem.
fn is_signed_cdn_url(url: &str) -> bool {
    url.contains("cdn.discordapp.com") && url.contains("ex=") && url.contains("hm=")
}

/// Performs the GET and hands back only what the caller needs, so ureq's types stay in
/// this one function.
/// What the server said the resource was when a partial download started.
///
/// Kept beside the `.part` so a resume days later can ask "is this still the same file?".
/// The archive has no published hash, so without this a build published mid-download would
/// be spliced onto the old bytes and nothing would catch it.
fn tag_path(part: &Path) -> PathBuf {
    let mut name = part.file_name().unwrap_or_default().to_os_string();
    name.push(".etag");
    part.with_file_name(name)
}

/// An open response: the status, what it says about length, its validator, and the body.
///
/// Named rather than a four-tuple because the resume path has to replace one of these
/// wholesale, and doing that through `fresh.0`, `fresh.1`, `fresh.2`, `fresh.3` was already
/// the kind of line nobody can check by reading.
struct Stream {
    code: u16,
    total: Option<u64>,
    etag: Option<String>,
    body: Box<dyn Read + Send>,
}

fn open_stream(url: &str, from: u64) -> Result<Stream, Error> {
    open_stream_tagged(url, from, None)
}

fn open_stream_tagged(url: &str, from: u64, if_range: Option<&str>) -> Result<Stream, Error> {
    let mut builder = ureq::get(url)
        .config()
        // Statuses are data here: a 404 needs its own message, and an error body may
        // carry a reason worth showing.
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        // No cap on body time. This has to move multi-gigabyte archives, and stopping
        // early is what Cancel is for.
        .timeout_recv_body(None)
        .build();
    if from > 0 {
        builder = builder.header("Range", format!("bytes={from}-"));
        // If-Range is the standard way to say "only honour that Range if the resource is
        // still what I think it is". A server whose copy changed answers 200 with the whole
        // body instead of 206, which the restart path below already handles.
        if let Some(tag) = if_range {
            builder = builder.header("If-Range", tag);
        }
    }

    let response = builder.call().map_err(|e| Error::Network(e.to_string()))?;
    let code = response.status().as_u16();

    // Content-Range is authoritative for a 206; content-length alone only describes the
    // slice being sent.
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    let total = header("content-range")
        .and_then(|v| v.rsplit('/').next().and_then(|t| t.trim().parse::<u64>().ok()))
        .or_else(|| {
            header("content-length")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|len| if code == 206 { from + len } else { len })
        });

    let etag = header("etag");
    Ok(Stream { code, total, etag, body: Box::new(response.into_body().into_reader()) })
}

/// Cap on a text fetch. A manifest is a couple of KB; an unbounded read of whatever a
/// misconfigured host decides to answer with is a memory hazard.
const MAX_TEXT_BYTES: u64 = 1024 * 1024;

/// Fetches a small text resource in full. Manifests, and nothing larger.
pub fn fetch_text(url: &str) -> Result<String, Error> {
    fetch_text_reporting(url, &mut |_, _| {})
}

/// As [`fetch_text`], but says when it is about to wait and try again.
///
/// Without this a caller shows one line and then blocks for the whole backoff, which reads
/// as a hang rather than as a retry. The engine cannot print, so it hands the fact back.
pub fn fetch_text_reporting(
    url: &str,
    on_retry: &mut dyn FnMut(u32, &Error),
) -> Result<String, Error> {
    fetch_text_cancellable(url, &Cancel::new(), on_retry)
}

/// As above, but a cancel actually reaches the backoff.
///
/// It was the one download nothing could stop. That matters more than its size suggests:
/// it is the first thing an update does, and with retries it can sit there for seconds
/// while Cancel does nothing.
pub fn fetch_text_cancellable(
    url: &str,
    cancel: &Cancel,
    on_retry: &mut dyn FnMut(u32, &Error),
) -> Result<String, Error> {
    let mut attempt = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match fetch_text_once(url) {
            Err(e) if is_transient(&e) && attempt < RETRIES => {
                crate::log::line(&format!("{url}: {e} - retrying ({}/{RETRIES})", attempt + 1));
                on_retry(attempt + 1, &e);
                if !backoff(attempt, cancel) {
                    return Err(Error::Cancelled);
                }
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn fetch_text_once(url: &str) -> Result<String, Error> {
    let stream = open_stream(url, 0)?;
    if stream.code != 200 {
        return Err(Error::Status {
            code: stream.code,
            likely_expired: stream.code == 404 && is_signed_cdn_url(url),
        });
    }
    let mut buf = Vec::new();
    stream
        .body
        .take(MAX_TEXT_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Network(e.to_string()))?;
    String::from_utf8(buf).map_err(|_| Error::Network("response was not valid UTF-8".into()))
}

/// Announced length of a resource, without downloading it.
///
/// A real HEAD, not a GET that gets dropped: on a 4.68 GiB archive the difference is
/// whether asking "how big is this?" moves any bytes at all.
pub fn head_len(url: &str) -> Option<u64> {
    let response = ureq::head(url)
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .call()
        .ok()?;
    if response.status().as_u16() != 200 {
        return None;
    }
    response
        .headers()
        .get("content-length")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// How many times a transient failure is retried before giving up.
///
/// Three is chosen for the case this exists for: a connection that drops for a few seconds
/// mid-download. Retrying is nearly free because a retry is a resume - the bytes already on
/// disk are kept - so the cost of trying again is a request, not a re-download.
pub const RETRIES: u32 = 3;

/// Worth another go, as opposed to worth reporting.
///
/// A checksum mismatch or an HTTP status is a real answer from the world and will be the
/// same answer next time; only a broken connection gets retried.
fn is_transient(e: &Error) -> bool {
    matches!(e, Error::Network(_) | Error::Truncated { .. })
}

/// Sleeps between attempts, but stays responsive to a cancel.
fn backoff(attempt: u32, cancel: &Cancel) -> bool {
    let total = Duration::from_secs(2u64.pow(attempt.min(4)));
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total {
        if cancel.is_cancelled() {
            return false;
        }
        std::thread::sleep(step);
        slept += step;
    }
    true
}

/// Downloads `spec.url` to `spec.dest`, retrying a dropped connection.
///
/// Blocking on purpose: the caller owns the thread. That keeps this testable and keeps an
/// async runtime out of the dependency tree.
pub fn fetch(
    spec: &Spec,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(Snapshot),
) -> Result<Outcome, Error> {
    let mut attempt = 0;
    loop {
        // The attempt number is stamped on every snapshot, so a caller drawing a progress
        // bar can say which try this is without being told separately.
        let result = fetch_once(spec, cancel, &mut |mut snap| {
            snap.attempt = attempt;
            on_progress(snap);
        });
        match result {
            Err(e) if is_transient(&e) && attempt < RETRIES && !cancel.is_cancelled() => {
                crate::log::line(&format!(
                    "{}: {e} - retrying ({}/{RETRIES})",
                    spec.url,
                    attempt + 1
                ));
                if !backoff(attempt, cancel) {
                    return Err(Error::Cancelled);
                }
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn fetch_once(
    spec: &Spec,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(Snapshot),
) -> Result<Outcome, Error> {
    if let Some(expected) = &spec.expected_sha256 {
        if spec.dest.exists() && hash::sha256_matches(&spec.dest, expected)? {
            return Ok(Outcome::AlreadyPresent);
        }
    }
    if let Some(parent) = spec.dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let part = spec.part_path();
    if !spec.resume {
        let _ = fs::remove_file(&part);
    }
    let mut have = match fs::metadata(&part) {
        Ok(m) if spec.resume => m.len(),
        _ => 0,
    };

    let saved_tag = if have > 0 { fs::read_to_string(tag_path(&part)).ok() } else { None };
    let mut stream = open_stream_tagged(&spec.url, have, saved_tag.as_deref())?;

    // 416 means the partial file is at or past the end of the resource: usually a complete
    // download whose rename never happened. Start clean rather than guess.
    if stream.code == 416 && have > 0 {
        let _ = fs::remove_file(&part);
        have = 0;
        stream = open_stream(&spec.url, 0)?;
    }
    // Struct until here, because the line above replaces one wholesale. Loose from here,
    // because the rest of this function reads them one at a time.
    let Stream { code, total, etag, body: mut reader } = stream;
    // A server that ignores Range answers 200 with the whole body, so the bytes on disk
    // are not a prefix of what is arriving.
    if have > 0 && code == 200 {
        have = 0;
    }
    if code != 200 && code != 206 {
        return Err(Error::Status {
            code,
            likely_expired: code == 404 && is_signed_cdn_url(&spec.url),
        });
    }

    let mut file = if have > 0 {
        let mut f = OpenOptions::new().write(true).open(&part)?;
        f.seek(SeekFrom::Start(have))?;
        f
    } else {
        let f = File::create(&part)?;
        // Written at the start of a fresh download, so a later resume has something to
        // hand to If-Range.
        match &etag {
            Some(tag) => {
                let _ = fs::write(tag_path(&part), tag);
            }
            None => {
                let _ = fs::remove_file(tag_path(&part));
            }
        }
        f
    };

    let mut hasher = spec.expected_sha256.as_ref().map(|_| Rolling::new());
    if let Some(h) = hasher.as_mut() {
        if have > 0 {
            h.absorb_prefix(&part, have)?;
        }
    }

    let mut done = have;
    let started = Instant::now();
    let mut last_tick = Instant::now();
    let mut buf = vec![0u8; CHUNK];

    let snapshot = |done: u64, total: Option<u64>, elapsed: Duration| Snapshot {
        done,
        total,
        attempt: 0,
        bytes_per_sec: {
            let secs = elapsed.as_secs_f64();
            if secs > 0.05 {
                (done.saturating_sub(have)) as f64 / secs
            } else {
                0.0
            }
        },
    };
    on_progress(snapshot(done, total, started.elapsed()));

    loop {
        if cancel.is_cancelled() {
            // The .part is left behind on purpose: that is what makes a retry a resume.
            let _ = file.flush();
            return Err(Error::Cancelled);
        }
        let read = reader.read(&mut buf).map_err(|e| Error::Network(e.to_string()))?;
        if read == 0 {
            break;
        }
        file.write_all(&buf[..read])?;
        if let Some(h) = hasher.as_mut() {
            h.update(&buf[..read]);
        }
        done += read as u64;

        if last_tick.elapsed() >= PROGRESS_INTERVAL {
            last_tick = Instant::now();
            on_progress(snapshot(done, total, started.elapsed()));
        }
    }
    file.flush()?;
    drop(file);
    on_progress(snapshot(done, total, started.elapsed()));

    // A dropped connection ends the read loop exactly like a finished one does. When the
    // server announced a length, compare against it: for a payload with no published hash
    // this is the only thing standing between a truncated file and the destination.
    if let Some(expected) = total {
        if done < expected {
            return Err(Error::Truncated { got: done, expected });
        }
    }

    if let (Some(h), Some(expected)) = (hasher, spec.expected_sha256.as_ref()) {
        let actual = h.finish();
        if !actual.eq_ignore_ascii_case(expected) {
            // A wrong file is worse than no file: keeping it would let a resume append to
            // corrupt bytes forever.
            let _ = fs::remove_file(&part);
            let _ = fs::remove_file(tag_path(&part));
            return Err(Error::HashMismatch {
                expected: expected.clone(),
                actual,
            });
        }
    }

    fs::rename(&part, &spec.dest)?;
    let _ = fs::remove_file(tag_path(&part));
    Ok(Outcome::Downloaded)
}

/// Picks the fastest of several mirror base URLs by timing a small ranged sample.
///
/// The Java downloads a 30 MiB test file in full from every mirror before every single
/// download, which on a slow line is minutes of doing nothing useful. Both mirrors support
/// `Accept-Ranges: bytes`, so a megabyte measures the same thing.
///
/// Returns the first base on the list if none of them answer, so a probe failure degrades
/// to "just try it" rather than blocking the download.
/// Which mirror to use, and how much confidence there is in the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub base: String,
    /// False when no mirror answered the probe and this is simply the first one.
    ///
    /// Not the same as "the download will fail": the probe file can be missing while the
    /// payload is fine, so the right move is to try anyway and say that is what happened.
    pub measured: bool,
    /// What each mirror said, for a message that names the problem instead of summarising
    /// it as "no server could be reached".
    pub failures: Vec<(String, String)>,
}

pub fn fastest_mirror(
    bases: &[String],
    probe_path: &str,
    sample_bytes: u64,
    cancel: &Cancel,
    on_probe: &mut dyn FnMut(&str, usize, usize),
) -> Option<Chosen> {
    let mut best: Option<(Duration, &String)> = None;
    let mut failures = Vec::new();

    for (i, base) in bases.iter().enumerate() {
        if cancel.is_cancelled() {
            break;
        }
        // Announced before it is tried, not after. This is a few seconds of nothing
        // happening on screen otherwise, and a stage name with no detail under it is how a
        // wait starts looking like a hang.
        on_probe(base, i + 1, bases.len());
        let url = format!("{}{}", base, probe_path);
        let started = Instant::now();
        let measured = (|| -> Result<(), Error> {
            let Stream { code, body: mut reader, .. } = open_stream(&url, 0)?;
            if code != 200 && code != 206 {
                return Err(Error::Status { code, likely_expired: false });
            }
            let mut sunk = 0u64;
            let mut buf = vec![0u8; 64 * 1024];
            while sunk < sample_bytes {
                let read = reader.read(&mut buf).map_err(|e| Error::Network(e.to_string()))?;
                if read == 0 {
                    break;
                }
                sunk += read as u64;
            }
            Ok(())
        })();

        match measured {
            Ok(()) => {
                let elapsed = started.elapsed();
                if best.is_none_or(|(b, _)| elapsed < b) {
                    best = Some((elapsed, base));
                }
            }
            Err(e) => failures.push((base.clone(), e.to_string())),
        }
    }

    if let Some((_, base)) = best {
        return Some(Chosen { base: base.clone(), measured: true, failures });
    }
    // Nothing answered. Still worth trying the first one - a missing probe file says
    // nothing about the payload - but the caller is told, so a later failure is not the
    // first the user hears of it.
    bases.first().map(|base| Chosen { base: base.clone(), measured: false, failures })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::testserver::{payload, sha_of, tmpdir, Opts, Server};
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::thread;

    const ROUTE: &str = "/file.bin";

    fn serving(body: &[u8], opts: Opts) -> Server {
        let mut routes = HashMap::new();
        routes.insert(ROUTE.to_string(), body.to_vec());
        Server::start(routes, opts)
    }

    // ---------------------------------------------------------------- pure logic

    #[test]
    fn part_file_sits_next_to_the_destination() {
        let s = Spec::new("http://x/y", "/tmp/a/b/echo.apk");
        assert_eq!(s.part_path(), PathBuf::from("/tmp/a/b/echo.apk.part"));
    }

    #[test]
    fn recognises_a_signed_discord_cdn_link() {
        assert!(is_signed_cdn_url(
            "https://cdn.discordapp.com/attachments/1/2/pnsovr.dll?ex=6a934471&is=6a91f2f1&hm=ab"
        ));
        // Unsigned CDN link, and an unrelated host: neither should claim expiry.
        assert!(!is_signed_cdn_url("https://cdn.discordapp.com/attachments/1/2/pnsovr.dll"));
        assert!(!is_signed_cdn_url("https://files.echovr.de/x?ex=1&hm=2"));
    }

    #[test]
    fn snapshot_reports_fraction_and_eta() {
        let s = Snapshot { done: 500, total: Some(1000), bytes_per_sec: 250.0, attempt: 0 };
        assert_eq!(s.fraction(), Some(0.5));
        assert_eq!(s.eta(), Some(Duration::from_secs(2)));
        // Unknown total, or no measured rate, means no honest estimate to give.
        assert_eq!(Snapshot { done: 1, total: None, bytes_per_sec: 9.0, attempt: 0 }.eta(), None);
        assert_eq!(Snapshot { done: 1, total: Some(9), bytes_per_sec: 0.0, attempt: 0 }.eta(), None);
    }

    // ---------------------------------------------------------------- over the wire

    #[test]
    fn downloads_and_verifies() {
        let body = payload(3 * 1024 * 1024 + 77);
        let srv = serving(&body, Opts::ranged());
        let dir = tmpdir("dl_ok");
        let dest = dir.join("file.bin");

        let mut ticks = Vec::new();
        let spec = Spec::new(srv.url(ROUTE), &dest).with_sha256(sha_of(&body));
        let out = fetch(&spec, &Cancel::new(), &mut |s| ticks.push(s)).unwrap();

        assert_eq!(out, Outcome::Downloaded);
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(!spec.part_path().exists(), ".part should be renamed away");
        let last = ticks.last().unwrap();
        assert_eq!(last.done, body.len() as u64);
        assert_eq!(last.total, Some(body.len() as u64));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_a_wrong_hash_and_discards_the_bytes() {
        let body = payload(64 * 1024);
        let srv = serving(&body, Opts::ranged());
        let dir = tmpdir("dl_badhash");
        let dest = dir.join("file.bin");

        let spec = Spec::new(srv.url(ROUTE), &dest).with_sha256("00".repeat(32));
        let err = fetch(&spec, &Cancel::new(), &mut |_| {}).unwrap_err();

        assert!(matches!(err, Error::HashMismatch { .. }), "got {err:?}");
        assert!(!dest.exists(), "a file that failed verification must not be published");
        assert!(!spec.part_path().exists(), "corrupt .part must not survive to be resumed");
        fs::remove_dir_all(dir).ok();
    }

    /// The point of resume: the bytes already on disk are not fetched again. Asserted by
    /// what the server actually sent, not by the result being correct.
    #[test]
    fn resumes_from_a_partial_file() {
        let body = payload(2 * 1024 * 1024);
        let srv = serving(&body, Opts::ranged());
        let dir = tmpdir("dl_resume");
        let dest = dir.join("file.bin");
        let spec = Spec::new(srv.url(ROUTE), &dest).with_sha256(sha_of(&body));

        fs::write(spec.part_path(), &body[..1_500_000]).unwrap();

        let out = fetch(&spec, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(out, Outcome::Downloaded);
        assert_eq!(fs::read(&dest).unwrap(), body);

        let served = srv.served.load(Ordering::Relaxed);
        assert!(
            served < body.len() as u64,
            "server sent {served} of {}, so nothing was resumed",
            body.len()
        );
        fs::remove_dir_all(dir).ok();
    }

    /// A server may ignore Range and send the whole body. Appending to the partial file
    /// would then corrupt it, so the local bytes have to be dropped. The wrong prefix here
    /// makes the difference detectable: if the code appended, the hash would fail.
    #[test]
    fn restarts_cleanly_when_the_server_ignores_range() {
        let body = payload(300 * 1024);
        let srv = serving(&body, Opts { honour_range: false, ..Default::default() });
        let dir = tmpdir("dl_norange");
        let dest = dir.join("file.bin");
        let spec = Spec::new(srv.url(ROUTE), &dest).with_sha256(sha_of(&body));
        fs::write(spec.part_path(), vec![0xAAu8; 100 * 1024]).unwrap();

        let out = fetch(&spec, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(out, Outcome::Downloaded);
        assert_eq!(fs::read(&dest).unwrap(), body);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn skips_entirely_when_the_file_is_already_there_and_correct() {
        let body = payload(4096);
        let srv = serving(&body, Opts::ranged());
        let dir = tmpdir("dl_present");
        let dest = dir.join("file.bin");
        fs::write(&dest, &body).unwrap();

        let spec = Spec::new(srv.url(ROUTE), &dest).with_sha256(sha_of(&body));
        let out = fetch(&spec, &Cancel::new(), &mut |_| {}).unwrap();

        assert_eq!(out, Outcome::AlreadyPresent);
        assert_eq!(srv.requests.load(Ordering::Relaxed), 0, "should not have asked the server");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn surfaces_a_404_without_claiming_expiry_for_an_unsigned_url() {
        let srv = serving(&payload(10), Opts { force_status: Some(404), ..Opts::ranged() });
        let dir = tmpdir("dl_404");

        let spec = Spec::new(srv.url("/gone.bin"), dir.join("gone.bin"));
        match fetch(&spec, &Cancel::new(), &mut |_| {}).unwrap_err() {
            Error::Status { code, likely_expired } => {
                assert_eq!(code, 404);
                assert!(!likely_expired);
            }
            other => panic!("got {other:?}"),
        }
        fs::remove_dir_all(dir).ok();
    }

    /// Cancelling must stop promptly and must leave the partial file behind, because that
    /// is what turns a retry into a resume.
    #[test]
    fn cancel_stops_and_keeps_the_partial_file() {
        let body = payload(8 * 1024 * 1024);
        let srv = serving(&body, Opts { chunk_delay: Duration::from_millis(25), ..Opts::ranged() });

        let dir = tmpdir("dl_cancel");
        let dest = dir.join("file.bin");
        let spec = Spec::new(srv.url(ROUTE), &dest);
        let cancel = Cancel::new();

        let c = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            c.cancel();
        });

        let err = fetch(&spec, &cancel, &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::Cancelled), "got {err:?}");
        assert!(!dest.exists());
        let part = spec.part_path();
        assert!(part.exists(), "the .part is what makes a retry a resume");
        assert!(fs::metadata(&part).unwrap().len() > 0);
        fs::remove_dir_all(dir).ok();
    }

    /// A connection that drops mid-body must never produce a published file.
    ///
    #[test]
    fn a_dropped_connection_is_retried_and_resumed() {
        // The server hangs up part way through the first request, then behaves. This is
        // the case retrying exists for, and the resume is what makes it nearly free.
        let body = payload(400 * 1024);
        let srv = serving(
            &body,
            Opts {
                truncate_after: Some(150 * 1024),
                heal_after: Some(1),
                ..Opts::ranged()
            },
        );
        let dir = tmpdir("dl_retry");
        let dest = dir.join("file.bin");
        let spec = Spec::new(srv.url(ROUTE), &dest);

        let mut attempts = Vec::new();
        let outcome = fetch(&spec, &Cancel::new(), &mut |s| attempts.push(s.attempt))
            .expect("a dropped connection should be retried, not reported");

        assert_eq!(outcome, Outcome::Downloaded);
        assert_eq!(fs::read(&dest).unwrap(), body, "the resumed file must be the whole file");
        assert!(!spec.part_path().exists(), "the .part is consumed on success");
        // The retry has to be visible, or a caller cannot say why it is taking longer.
        assert!(attempts.iter().any(|&a| a > 0), "the retry was never reported: {attempts:?}");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_checksum_mismatch_is_not_retried() {
        // Retrying an answer the world already gave is just a slower failure.
        let body = payload(64 * 1024);
        let srv = serving(&body, Opts::ranged());
        let dir = tmpdir("dl_noretry");
        let dest = dir.join("file.bin");
        let spec = Spec::new(srv.url(ROUTE), &dest).with_sha256("aa".repeat(32));

        let began = std::time::Instant::now();
        let err = fetch(&spec, &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::HashMismatch { .. }), "got {err:?}");
        assert!(began.elapsed() < Duration::from_secs(2), "it backed off over a real answer");
        fs::remove_dir_all(dir).ok();
    }

    /// It turns out ureq enforces Content-Length itself and surfaces this as a read error
    /// rather than a clean end-of-stream, so the explicit length check below it rarely
    /// fires. It stays as a backstop for the cases ureq cannot see: a server that lies
    /// about the length downwards, or a chunked body that terminates properly but short.
    /// What this test pins is the outcome the user experiences, whichever path produced it.
    #[test]
    fn refuses_a_body_that_ends_early() {
        let body = payload(400 * 1024);
        let srv = serving(
            &body,
            Opts { truncate_after: Some(150 * 1024), ..Opts::ranged() },
        );
        let dir = tmpdir("dl_short");
        let dest = dir.join("file.bin");
        let spec = Spec::new(srv.url(ROUTE), &dest);

        // Deliberately the single attempt: `fetch` retries, and against a server that
        // truncates a fixed number of bytes each time a retry resumes and eventually wins.
        // What is being pinned here is that one short body is never published.
        let err = fetch_once(&spec, &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(
            matches!(err, Error::Truncated { .. } | Error::Network(_)),
            "a short body must fail, got {err:?}"
        );
        assert!(!dest.exists(), "a short file must not be published");
        // Kept, because the retry is a resume: the bytes that did arrive are still good.
        let part = spec.part_path();
        assert!(part.exists(), "the partial bytes are worth keeping");
        assert!(fs::metadata(&part).unwrap().len() > 0);
        fs::remove_dir_all(dir).ok();
    }

    /// Resuming a download whose server-side copy changed would splice two different builds
    /// together. If-Range makes the server answer 200 instead of 206, and the whole file is
    /// fetched again.
    #[test]
    fn restarts_when_the_resource_changed_under_a_resume() {
        let old = payload(200 * 1024);
        let dir = tmpdir("dl_ifrange");
        let dest = dir.join("file.bin");
        let spec = Spec::new("about:blank", &dest).with_sha256(sha_of(&old));

        // Half an earlier download, plus the tag the server gave at the time.
        fs::write(spec.part_path(), &old[..80 * 1024]).unwrap();
        fs::write(tag_path(&spec.part_path()), "\"v1\"").unwrap();

        // The server has moved on: same route, new bytes, new tag.
        let new = payload(200 * 1024 + 33);
        let srv = serving(&new, Opts { etag: Some("\"v2\"".into()), ..Opts::ranged() });
        let spec = Spec::new(srv.url(ROUTE), &dest).with_sha256(sha_of(&new));

        let out = fetch(&spec, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(out, Outcome::Downloaded);
        assert_eq!(fs::read(&dest).unwrap(), new, "stale prefix was spliced in");
        fs::remove_dir_all(dir).ok();
    }

    /// The happy path for the same mechanism: unchanged tag, so the Range is honoured.
    #[test]
    fn resumes_when_the_tag_still_matches() {
        let body = payload(300 * 1024);
        let srv = serving(&body, Opts { etag: Some("\"same\"".into()), ..Opts::ranged() });
        let dir = tmpdir("dl_ifrange_ok");
        let dest = dir.join("file.bin");
        let spec = Spec::new(srv.url(ROUTE), &dest).with_sha256(sha_of(&body));

        fs::write(spec.part_path(), &body[..200 * 1024]).unwrap();
        fs::write(tag_path(&spec.part_path()), "\"same\"").unwrap();

        fetch(&spec, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(
            srv.served.load(Ordering::Relaxed) < body.len() as u64,
            "the matching tag should have allowed a real resume"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn tag_file_is_cleaned_up_on_success() {
        let body = payload(8192);
        let srv = serving(&body, Opts { etag: Some("\"x\"".into()), ..Opts::ranged() });
        let dir = tmpdir("dl_tagclean");
        let spec = Spec::new(srv.url(ROUTE), dir.join("file.bin"));
        fetch(&spec, &Cancel::new(), &mut |_| {}).unwrap();
        assert!(!tag_path(&spec.part_path()).exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn mirror_probe_prefers_the_faster_server_and_degrades_gracefully() {
        let body = payload(512 * 1024);
        let fast = serving(&body, Opts::ranged());
        let slow = serving(&body, Opts { chunk_delay: Duration::from_millis(40), ..Opts::ranged() });

        let bases = vec![format!("{}/", slow.base), format!("{}/", fast.base)];
        let pick = fastest_mirror(&bases, "file.bin", 256 * 1024, &Cancel::new(), &mut |_, _, _| {}).unwrap();
        assert_eq!(pick.base, format!("{}/", fast.base));
        assert!(pick.measured, "one of them answered, so this is a measurement");
        assert!(pick.failures.is_empty());

        // Nothing reachable: still hand back the first base rather than refusing to
        // download - a probe file can be missing while the payload is fine - but say so,
        // so a later failure is not the first anyone hears of it.
        let dead = vec!["http://127.0.0.1:1/".to_string()];
        let guess = fastest_mirror(&dead, "file.bin", 1024, &Cancel::new(), &mut |_, _, _| {}).unwrap();
        assert_eq!(guess.base, "http://127.0.0.1:1/");
        assert!(!guess.measured, "nothing answered, so this is a guess");
        assert_eq!(guess.failures.len(), 1, "the one that failed should be named");
        assert!(guess.failures[0].1.contains("could not reach"), "got {:?}", guess.failures);

        // A mirror that answers with a 404 for the probe is a failure worth naming too,
        // not a silent skip.
        let missing = serving(&body, Opts { force_status: Some(404), ..Opts::ranged() });
        let mixed = vec![format!("{}/", missing.base), format!("{}/", fast.base)];
        let pick = fastest_mirror(&mixed, "file.bin", 1024, &Cancel::new(), &mut |_, _, _| {}).unwrap();
        assert_eq!(pick.base, format!("{}/", fast.base));
        assert_eq!(pick.failures.len(), 1);
        assert!(pick.failures[0].1.contains("404"), "got {:?}", pick.failures);
    }
}
