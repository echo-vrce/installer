// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding out which part of the network is broken, rather than only that something is.
//!
//! "could not reach the server" is true and nearly useless. It is the same sentence for a
//! DNS block, a firewall, a dead corporate proxy and a mirror that is genuinely down, and
//! those have four different answers. So each host is tried in three stages, in the order
//! the machine tries them, and the first one that fails is the finding.
//!
//! Nothing here fixes anything. It exists so somebody can paste an answer into a help
//! channel instead of "it doesn't work", and so the answer names the host.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::endpoints;
use crate::engine::Cancel;

/// Kept short on purpose. This runs against several hosts one after another, and somebody
/// is watching it: a diagnostic that takes a minute to say "DNS is blocked" gets cancelled
/// before it answers.
const STAGE_TIMEOUT: Duration = Duration::from_secs(8);

/// One host the app cannot work without, and what it is needed for.
pub struct Target {
    pub what: &'static str,
    pub url: String,
}

/// Every host the app talks to, each named by the job it does rather than by its hostname.
///
/// A hostname tells somebody nothing about whether they can ignore the line. "The update
/// manifest" tells them that a red mark there stops an update and nothing else.
pub fn targets() -> Vec<Target> {
    let mut t =
        vec![Target { what: "The update manifest", url: endpoints::PC_MANIFEST.to_string() }];
    for (i, m) in endpoints::MIRRORS.iter().enumerate() {
        t.push(Target {
            what: match i {
                0 => "Download mirror 1",
                1 => "Download mirror 2",
                _ => "Download mirror 3",
            },
            // The archive itself, not the mirror's root. A root that 404s proves the host
            // is up and nothing else; this proves the mirror actually has the payload,
            // which is the thing that stops an install when it is not true.
            url: format!("{m}{}", endpoints::PC_ARCHIVE),
        });
    }
    t.push(Target {
        what: "The licence patch service",
        url: endpoints::PATCH_EXCHANGE.to_string(),
    });
    t.push(Target { what: "Installer updates", url: endpoints::UPDATE_VERSION.to_string() });
    t
}

/// Which stage failed, or how long the whole thing took when none did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Reached, with the status it answered and how long it took.
    Reached { code: u16, ms: u64 },
    /// The name did not resolve. A firewall, a DNS block, or no connection at all.
    NoDns(String),
    /// The name resolved but nothing accepted a connection on 443.
    NoConnect(String),
    /// Connected, but the request itself did not complete. Usually TLS.
    NoAnswer(String),
    Cancelled,
}

impl Outcome {
    /// Is this the answer somebody can stop worrying about?
    pub fn is_fine(&self) -> bool {
        // Any status at all means the host is there and talking. A 404 or a 405 on a probe
        // is not a network problem, and calling it one sends people to fix their router.
        matches!(self, Outcome::Reached { .. })
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The status is only worth printing when it is not the expected one, and even
            // then the sentence has to lead with the fact that the host is up: a green tick
            // next to "404" reads as a problem to somebody already looking for one.
            Outcome::Reached { code: 200 | 206, ms } => write!(f, "answered in {ms} ms"),
            Outcome::Reached { code, ms } => {
                write!(f, "the host answered in {ms} ms, with HTTP {code} to a HEAD request")
            }
            Outcome::NoDns(host) => write!(
                f,
                "{host} did not resolve, so nothing was tried. A DNS block, or no \
                 connection at all."
            ),
            Outcome::NoConnect(e) => {
                write!(f, "resolved, but port 443 refused the connection: {e}")
            }
            Outcome::NoAnswer(e) => write!(f, "connected, but the request failed: {e}"),
            Outcome::Cancelled => write!(f, "stopped"),
        }
    }
}

/// The host and port a URL points at, for the two stages that happen before HTTP.
fn host_port(url: &str) -> Option<(String, u16)> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?']).next()?;
    let port = if url.starts_with("http://") { 80 } else { 443 };
    match authority.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().unwrap_or(port))),
        None => Some((authority.to_string(), port)),
    }
}

/// A proxy in the environment, if there is one.
///
/// Worth its own line because a dead proxy looks exactly like a dead internet from inside
/// the app, and somebody who did not set it themselves has no reason to suspect it. The
/// value is shown as-is: it can carry credentials, so this is only ever drawn on the
/// machine it came from and never goes into a support bundle.
pub fn proxy_in_use() -> Option<String> {
    for name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY"] {
        if let Ok(v) = std::env::var(name) {
            if !v.trim().is_empty() {
                return Some(format!("{name}={}", v.trim()));
            }
        }
    }
    None
}

/// Tries one host, stopping at the first stage that fails.
pub fn check(target: &Target, cancel: &Cancel) -> Outcome {
    if cancel.is_cancelled() {
        return Outcome::Cancelled;
    }
    let Some((host, port)) = host_port(&target.url) else {
        return Outcome::NoDns(target.url.clone());
    };
    let started = Instant::now();

    // 1. The name. Done separately from the request so a DNS block reads as a DNS block
    //    rather than as a mysterious failure to connect.
    let addrs: Vec<_> = match (host.as_str(), port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => return Outcome::NoDns(host),
    };
    let Some(addr) = addrs.first() else { return Outcome::NoDns(host) };
    if cancel.is_cancelled() {
        return Outcome::Cancelled;
    }

    // 2. The socket. Separately again, because "resolved but refused" is a firewall and
    //    "did not resolve" is not, and they are not fixed the same way.
    if let Err(e) = TcpStream::connect_timeout(addr, STAGE_TIMEOUT) {
        return Outcome::NoConnect(e.to_string());
    }
    if cancel.is_cancelled() {
        return Outcome::Cancelled;
    }

    // 3. The request, which is where TLS and any proxy actually come in. HEAD because the
    //    body is not the point and one of these URLs is 4.68 GB.
    match ureq::head(&target.url)
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(STAGE_TIMEOUT))
        .timeout_recv_response(Some(STAGE_TIMEOUT))
        .build()
        .call()
    {
        Ok(r) => Outcome::Reached {
            code: r.status().as_u16(),
            ms: started.elapsed().as_millis() as u64,
        },
        Err(e) => Outcome::NoAnswer(e.to_string().trim_start_matches("io: ").to_string()),
    }
}

/// Every target in turn, reporting each as it lands.
pub fn run(cancel: &Cancel, on_result: &mut dyn FnMut(&Target, Outcome)) {
    for target in targets() {
        let outcome = check(&target, cancel);
        crate::log::line(&format!("netcheck {}: {outcome}", target.url));
        let stop = outcome == Outcome::Cancelled;
        on_result(&target, outcome);
        if stop {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_gives_up_its_host_and_port() {
        assert_eq!(host_port("https://files.echovr.de/x/y"), Some(("files.echovr.de".into(), 443)));
        assert_eq!(host_port("https://files.echovr.de/"), Some(("files.echovr.de".into(), 443)));
        assert_eq!(host_port("http://example.com/a"), Some(("example.com".into(), 80)));
        assert_eq!(host_port("https://example.com:8443/a"), Some(("example.com".into(), 8443)));
        assert_eq!(host_port("not a url"), None);
    }

    #[test]
    fn every_host_the_app_depends_on_is_checked() {
        // The failure this exists for: somebody adds an endpoint, the app starts depending
        // on a new host, and the diagnostic keeps reporting all clear while that host is
        // the one that is down.
        let checked: Vec<String> =
            targets().iter().filter_map(|t| host_port(&t.url).map(|(h, _)| h)).collect();
        for url in [
            endpoints::PC_MANIFEST,
            endpoints::QUEST_MANIFEST,
            endpoints::PATCH_EXCHANGE,
            endpoints::UPDATE_VERSION,
            endpoints::UPDATE_ZIP,
            endpoints::UPDATE_SHA256,
        ] {
            let (host, _) = host_port(url).expect(url);
            assert!(checked.contains(&host), "{host} is used by the app but never checked");
        }
        for mirror in endpoints::MIRRORS {
            let (host, _) = host_port(mirror).expect(mirror);
            assert!(checked.contains(&host), "mirror {host} is never checked");
        }
    }

    #[test]
    fn any_status_at_all_counts_as_reachable() {
        // A probe that 404s has still proved the host is up, and reporting that as a network
        // fault sends somebody to reset a router that was never the problem.
        assert!(Outcome::Reached { code: 404, ms: 12 }.is_fine());
        let noisy = Outcome::Reached { code: 405, ms: 12 }.to_string();
        assert!(noisy.starts_with("the host answered"), "{noisy}");
        assert_eq!(Outcome::Reached { code: 200, ms: 12 }.to_string(), "answered in 12 ms");
        assert!(Outcome::Reached { code: 200, ms: 12 }.is_fine());
        assert!(!Outcome::NoDns("files.echovr.de".into()).is_fine());
        assert!(!Outcome::NoConnect("refused".into()).is_fine());
        assert!(!Outcome::NoAnswer("tls".into()).is_fine());
    }

    #[test]
    fn each_failure_says_which_stage_it_was() {
        // The whole point: three different sentences, because they have three different
        // answers. A single "could not reach the server" is what this replaces.
        let dns = Outcome::NoDns("files.echovr.de".into()).to_string();
        let conn = Outcome::NoConnect("refused".into()).to_string();
        let http = Outcome::NoAnswer("tls handshake".into()).to_string();
        assert!(dns.contains("did not resolve"), "{dns}");
        assert!(conn.contains("443"), "{conn}");
        assert!(http.contains("connected"), "{http}");
        assert_ne!(dns, conn);
        assert_ne!(conn, http);
    }
}
