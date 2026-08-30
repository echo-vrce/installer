// SPDX-License-Identifier: GPL-3.0-or-later
//! A real HTTP/1.1 server for tests.
//!
//! Range handling, resume and multi-file update runs are the parts most likely to be
//! subtly wrong, and they are only worth testing against something that actually speaks
//! the protocol. Shared by the download and update tests.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Default, Clone)]
pub(crate) struct Opts {
    /// When false the server answers 200 with the whole body even if a Range was asked
    /// for, which is what some real servers and proxies do.
    pub honour_range: bool,
    /// Slows the response so a cancel has a window to land in.
    pub chunk_delay: Duration,
    /// Answer every request with this status instead of serving.
    pub force_status: Option<u16>,
    /// Send only this many bytes of the body, then hang up. Simulates a dropped
    /// connection, which otherwise looks exactly like a finished one to the client.
    pub truncate_after: Option<u64>,
    /// Stop truncating once this many requests have been served. Lets a test model the
    /// case retrying exists for: a connection that drops and then works.
    pub heal_after: Option<u64>,
    /// Advertised ETag. When set, an If-Range naming a different tag is answered with a
    /// full 200 rather than a 206, which is what a server does when its copy changed.
    pub etag: Option<String>,
}

impl Opts {
    pub(crate) fn ranged() -> Self {
        Opts { honour_range: true, ..Default::default() }
    }
}

pub(crate) struct Server {
    pub(crate) base: String,
    pub(crate) requests: Arc<AtomicU64>,
    pub(crate) served: Arc<AtomicU64>,
}

impl Server {
    pub(crate) fn start(routes: HashMap<String, Vec<u8>>, opts: Opts) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicU64::new(0));
        let served = Arc::new(AtomicU64::new(0));
        let (rq, sv) = (requests.clone(), served.clone());
        let state = Arc::new((routes, opts));
        let counter = requests.clone();

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                rq.fetch_add(1, Ordering::Relaxed);
                let state = state.clone();
                let sv = sv.clone();
                let seen = counter.load(Ordering::Relaxed);
                thread::spawn(move || serve_one(stream, state, sv, seen));
            }
        });

        Server { base: format!("http://127.0.0.1:{port}"), requests, served }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

fn serve_one(
    mut stream: std::net::TcpStream,
    state: Arc<(HashMap<String, Vec<u8>>, Opts)>,
    served: Arc<AtomicU64>,
    request_number: u64,
) {
    let (routes, opts) = &*state;
    let truncate = match opts.heal_after {
        Some(n) if request_number > n => None,
        _ => opts.truncate_after,
    };
    let Ok(clone) = stream.try_clone() else { return };
    let mut reader = io::BufReader::new(clone);

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();

    let mut from = 0u64;
    let mut if_range: Option<String> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) <= 2 {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("range:") {
            if let Some(spec) = v.trim().strip_prefix("bytes=") {
                if let Some(start) = spec.split('-').next() {
                    from = start.trim().parse().unwrap_or(0);
                }
            }
        }
        if lower.starts_with("if-range:") {
            if_range = Some(h[("if-range:".len())..].trim().to_string());
        }
    }

    // The resource moved on since the partial download started: ignore the Range.
    if let (Some(client_tag), Some(ours)) = (&if_range, &opts.etag) {
        if client_tag != ours {
            from = 0;
        }
    }

    if let Some(code) = opts.force_status {
        let _ = write!(
            stream,
            "HTTP/1.1 {code} X\r\nContent-Length: 5\r\nConnection: close\r\n\r\nnope!"
        );
        return;
    }

    let Some(body) = routes.get(&path) else {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\nConnection: close\r\n\r\n404"
        );
        return;
    };
    let total = body.len() as u64;

    if from >= total && from > 0 {
        let _ = write!(
            stream,
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }

    let tag_header = opts
        .etag
        .as_ref()
        .map(|t| format!("ETag: {t}\r\n"))
        .unwrap_or_default();

    let slice: &[u8] = if opts.honour_range && from > 0 {
        let _ = write!(
            stream,
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
             Content-Range: bytes {}-{}/{}\r\n{}Connection: close\r\n\r\n",
            total - from,
            from,
            total - 1,
            total,
            tag_header
        );
        &body[from as usize..]
    } else {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\
             Accept-Ranges: bytes\r\n{tag_header}Connection: close\r\n\r\n"
        );
        &body[..]
    };

    // Deliberately short: announce the full length in the header, then hang up early.
    let slice = match truncate {
        Some(limit) => &slice[..(limit as usize).min(slice.len())],
        None => slice,
    };

    for piece in slice.chunks(128 * 1024) {
        if stream.write_all(piece).is_err() {
            return;
        }
        served.fetch_add(piece.len() as u64, Ordering::Relaxed);
        if !opts.chunk_delay.is_zero() {
            thread::sleep(opts.chunk_delay);
        }
    }
    let _ = stream.flush();
}

/// Deterministic pseudo-random body, so a test can assert on exact bytes.
pub(crate) fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + i / 7) % 251) as u8).collect()
}

pub(crate) fn sha_of(bytes: &[u8]) -> String {
    let mut r = crate::engine::hash::Rolling::new();
    r.update(bytes);
    r.finish()
}

pub(crate) fn tmpdir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("evrce_t_{}_{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}
