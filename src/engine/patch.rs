// SPDX-License-Identifier: GPL-3.0-or-later
//! Getting a personalised patch link from the Discord bot.
//!
//! The shape is fixed by the other end and is not ours to change: the browser is sent to
//! Discord's consent screen, Discord redirects to `http://127.0.0.1:53124/callback`, and
//! the code that arrives is exchanged with the server for a download URL. That port and
//! path are registered against the client ID in Discord's developer portal.
//!
//! Three things here that the original does not do, each learned by probing the live
//! service (see `docs/findings.md`):
//!
//! - **The listener does not take the first connection it gets.** Browsers speculatively
//!   open sockets; Chrome preconnects the moment the address bar resolves, and that socket
//!   often carries no bytes at all. Treating it as the callback loses the real one. This
//!   was observed, not theorised: it is what happened on the first run of the probe.
//! - **A `state` parameter is generated and checked.** Without it, any page you visit can
//!   redirect to the loopback callback with *its* code, and the installer would fetch and
//!   apply somebody else's patch file.
//! - **The error body is read.** The server answers with `error`, `message` and `invite`,
//!   and the invite in particular is worth following rather than hardcoding.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use crate::endpoints;

/// Registered against the client ID; neither this nor the path can move.
pub const CALLBACK_PORT: u16 = 53124;
const CLIENT_ID: &str = "1326594571584409650";
/// Long enough for a login, short enough that a rate-limited attempt fails rather than
/// hanging: Discord's throttle page never redirects back, so silence is the only signal.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
/// Generating a patch takes about nine seconds of bot time, measured.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(120);

/// What the patch is for. The server takes this verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// pnsovr.dll, for PC.
    Dll,
    /// A repacked Quest APK.
    Apk,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Dll => "dll",
            Kind::Apk => "apk",
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// The port is held, usually by an attempt that has not finished releasing it.
    PortBusy(String),
    NoBrowser(String),
    /// No redirect arrived in time.
    TimedOut,
    /// Discord returned an error instead of a code, e.g. the user pressed Cancel.
    Denied(String),
    /// The callback did not carry the state we sent. Someone else's redirect.
    StateMismatch,
    /// Not a member of the patcher server. Carries the server's own invite.
    NotInGuild { message: String, invite: String },
    /// The bot is generating something else.
    Busy(String),
    Server { status: u16, message: String },
    Network(String),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::PortBusy(m) => write!(f, "{m}"),
            Error::NoBrowser(url) => write!(
                f,
                "Could not open a browser. Open this address by hand:\n{url}"
            ),
            Error::TimedOut => write!(
                f,
                "Discord did not finish the authorisation in time. If the browser showed \
                 \"Service got rate limited\", that is Discord throttling: wait a minute and \
                 try again."
            ),
            Error::Denied(e) => write!(f, "Discord returned \"{e}\" instead of an authorisation."),
            Error::StateMismatch => write!(
                f,
                "The authorisation that came back was not the one this installer started. \
                 Nothing was downloaded. Try again."
            ),
            Error::NotInGuild { message, .. } => write!(f, "{message}"),
            Error::Busy(m) => write!(f, "{m}"),
            Error::Server { status, message } => write!(f, "Server error {status}: {message}"),
            Error::Network(m) => write!(f, "{m}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy)]
pub enum Progress {
    /// The browser is open and the consent screen is up.
    WaitingForBrowser,
    /// A code arrived; the bot is building the file.
    Generating,
}

/// Runs the whole flow and returns a download URL.
///
/// Blocking, and long: it waits on a human. The caller owns the thread.
pub fn obtain(
    kind: Kind,
    cancel: &crate::engine::Cancel,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<String, Error> {
    let listener = bind()?;
    listener.set_nonblocking(true).ok();

    let state = random_state();
    let redirect = format!("http://127.0.0.1:{CALLBACK_PORT}/callback");
    let auth_url = format!(
        "https://discord.com/api/oauth2/authorize?client_id={CLIENT_ID}&redirect_uri={}\
         &response_type=code&scope=identify%20guilds&state={state}",
        urlencode(&redirect)
    );

    if !open_browser(&auth_url) {
        return Err(Error::NoBrowser(auth_url));
    }
    on_progress(Progress::WaitingForBrowser);

    let code = wait_for_callback(&listener, &state, cancel)?;
    on_progress(Progress::Generating);
    exchange(&code, kind)
}

/// Binds the loopback listener, retrying briefly so a socket still in TIME_WAIT from a
/// previous attempt does not surface as a hard failure.
fn bind() -> Result<TcpListener, Error> {
    let mut last = String::new();
    for _ in 0..5 {
        match TcpListener::bind(("127.0.0.1", CALLBACK_PORT)) {
            Ok(l) => return Ok(l),
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    Err(Error::PortBusy(format!(
        "Could not open the Discord callback port ({CALLBACK_PORT}). Another authorisation \
         may still be finishing; wait a moment and try again. ({last})"
    )))
}

/// Waits for the redirect that actually carries an authorisation, answering and discarding
/// everything else that touches the port.
fn wait_for_callback(
    listener: &TcpListener,
    expected_state: &str,
    cancel: &crate::engine::Cancel,
) -> Result<String, Error> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    while Instant::now() < deadline {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let Some(path) = read_request_path(&mut stream) else {
                    // A speculative connection carrying nothing. Keep waiting.
                    continue;
                };
                let code = query(&path, "code");
                let error = query(&path, "error");
                if code.is_none() && error.is_none() {
                    let _ = respond(&mut stream, "404 Not Found", "text/plain", "not the callback");
                    continue;
                }
                let _ = respond(&mut stream, "200 OK", "text/html; charset=UTF-8", PAGE);

                if let Some(e) = error {
                    return Err(Error::Denied(e));
                }
                // Checked after answering the browser, so the tab still closes cleanly.
                if query(&path, "state").as_deref() != Some(expected_state) {
                    return Err(Error::StateMismatch);
                }
                return code.ok_or(Error::TimedOut);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => return Err(Error::Network(e.to_string())),
        }
    }
    Err(Error::TimedOut)
}

const PAGE: &str = "<html><body style=\"background:#131619;color:#E6E9ED;font-family:sans-serif;\
                    padding:3rem\"><h2>Authorisation captured.</h2><p>You can close this tab \
                    and go back to the installer.</p></body></html>";

/// Reads the request line and drains the headers. None when the peer sent nothing before
/// the read timeout, which is the preconnect case.
///
/// The read timeout matters: an accepted socket does not inherit the listener's
/// non-blocking flag, so a silent peer would otherwise block here indefinitely.
fn read_request_path(stream: &mut std::net::TcpStream) -> Option<String> {
    stream.set_read_timeout(Some(Duration::from_millis(1500))).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return None,
        Ok(_) => {}
    }
    if line.trim().is_empty() {
        return None;
    }
    let mut header = String::new();
    while reader.read_line(&mut header).unwrap_or(0) > 2 {
        header.clear();
    }
    line.split_whitespace().nth(1).map(str::to_string)
}

fn respond(
    stream: &mut std::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// Exchanges the authorisation code for a download URL.
fn exchange(code: &str, kind: Kind) -> Result<String, Error> {
    let body = format!(
        "{{\"code\":\"{}\",\"type\":\"{}\"}}",
        escape_json(code),
        kind.as_str()
    );
    let response = ureq::post(endpoints::PATCH_EXCHANGE)
        .header("Content-Type", "application/json")
        .config()
        // The reason lives in the body, and ureq discards it for a non-2xx by default.
        .http_status_as_error(false)
        .timeout_global(Some(EXCHANGE_TIMEOUT))
        .build()
        .send(&body)
        .map_err(|e| Error::Network(e.to_string()))?;

    let status = response.status().as_u16();
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|e| Error::Network(e.to_string()))?;
    interpret(status, &text)
}

/// Turns a status and body into either a URL or a specific, actionable error.
pub fn interpret(status: u16, body: &str) -> Result<String, Error> {
    if status == 200 {
        return match field(body, "patchUrl") {
            Some(url) => Ok(url),
            None => Err(Error::Server {
                status,
                message: "the reply carried no download link".into(),
            }),
        };
    }

    let code = field(body, "error").unwrap_or_default();
    let message = field(body, "message").unwrap_or_else(|| {
        if code.is_empty() {
            body.trim().to_string()
        } else {
            code.clone()
        }
    });

    match code.as_str() {
        "not_in_guild" => Err(Error::NotInGuild {
            message: if message.is_empty() {
                "You must join the Echo VR Patcher server first.".into()
            } else {
                message
            },
            // The server supplies its own invite, so a changed invite is followed rather
            // than going stale in this file.
            invite: field(body, "invite")
                .unwrap_or_else(|| endpoints::DISCORD_PATCHER.to_string()),
        }),
        "busy" => Err(Error::Busy(if message.is_empty() {
            "The bot is generating another file. Try again in half a minute.".into()
        } else {
            message
        })),
        _ => Err(Error::Server { status, message }),
    }
}

/// Minimal JSON string-field read. The replies are three fields deep; a parser would be
/// more machinery than the shape justifies.
fn field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after = &json[json.find(&needle)? + needle.len()..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let rest = after.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 128 bits of process-local randomness, hex encoded. Not a secret: it only has to be
/// unguessable by a page trying to feed us its own callback.
/// The CSRF token, from the operating system's random source.
///
/// This is the only thing standing between the loopback listener and a code someone else's
/// page hands it, so it should not come from a hash of the clock. `BCryptGenRandom` on
/// Windows and `/dev/urandom` elsewhere; the hasher below is a last resort for when neither
/// answers, which is better than refusing to run but is not equivalent.
fn random_state() -> String {
    let mut bytes = [0u8; 16];
    if os_random(&mut bytes) {
        let mut out = String::with_capacity(32);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
        }
        return out;
    }
    weak_state()
}

#[cfg(windows)]
fn os_random(buf: &mut [u8]) -> bool {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        ) == 0
    }
}

#[cfg(not(windows))]
fn os_random(buf: &mut [u8]) -> bool {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .is_ok()
}

/// Only reached if the OS random source could not be read at all.
fn weak_state() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0),
        );
        h.write_usize(&out as *const String as usize);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

fn query(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    q.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "windows")]
    let (cmd, args): (&str, Vec<&str>) = ("rundll32", vec!["url.dll,FileProtocolHandler", url]);
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (cmd, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    crate::engine::hide_console(&mut std::process::Command::new(cmd)).args(args).spawn().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://cdn.discordapp.com/attachments/1/2/pnsovr.dll?ex=a&is=b&hm=c";

    #[test]
    fn reads_a_download_link_out_of_a_success() {
        let body = format!("{{\"patchUrl\": \"{URL}\"}}");
        assert_eq!(interpret(200, &body).unwrap(), URL);
        // Both spacings the server might use.
        assert_eq!(interpret(200, &format!("{{\"patchUrl\":\"{URL}\"}}")).unwrap(), URL);
    }

    #[test]
    fn a_success_with_no_link_is_an_error_not_a_silent_pass() {
        assert!(matches!(
            interpret(200, "{\"ok\":true}"),
            Err(Error::Server { .. })
        ));
    }

    /// The live 403, verbatim. All three fields are used rather than the code alone.
    #[test]
    fn reads_the_invite_out_of_a_not_in_guild_reply() {
        let body = "{\"error\": \"not_in_guild\", \"message\": \"You must join the Echo VR \
                    Patcher server first\", \"invite\": \"https://discord.gg/bMpsva6fmA\"}";
        match interpret(403, body).unwrap_err() {
            Error::NotInGuild { message, invite } => {
                assert!(message.contains("join"));
                assert_eq!(invite, "https://discord.gg/bMpsva6fmA");
            }
            other => panic!("got {other:?}"),
        }
    }

    /// The live 400, verbatim.
    #[test]
    fn reports_an_expired_code_with_the_servers_own_words() {
        let body = "{\"error\": \"Invalid or expired authorization code\"}";
        match interpret(400, body).unwrap_err() {
            Error::Server { status, message } => {
                assert_eq!(status, 400);
                assert!(message.contains("expired"), "got {message}");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn recognises_the_bot_being_busy() {
        let body = "{\"error\": \"busy\", \"message\": \"Bot is busy, try again\"}";
        assert!(matches!(interpret(409, body), Err(Error::Busy(_))));
    }

    #[test]
    fn parses_query_parameters() {
        let p = "/callback?code=ABC123&state=deadbeef";
        assert_eq!(query(p, "code").as_deref(), Some("ABC123"));
        assert_eq!(query(p, "state").as_deref(), Some("deadbeef"));
        assert_eq!(query(p, "missing"), None);
        assert_eq!(query("/callback", "code"), None);
    }

    #[test]
    fn state_is_long_and_different_every_time() {
        let a = random_state();
        let b = random_state();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "a fixed state would defeat the point of having one");
    }

    #[test]
    fn escapes_a_code_before_putting_it_in_json() {
        assert_eq!(escape_json("a\"b\\c"), "a\\\"b\\\\c");
    }

    #[test]
    fn encodes_the_redirect_uri() {
        assert_eq!(
            urlencode("http://127.0.0.1:53124/callback"),
            "http%3A%2F%2F127.0.0.1%3A53124%2Fcallback"
        );
    }

    /// The callback listener has to survive a browser that opens a socket and says nothing,
    /// and has to reject a redirect that carries somebody else's state.
    #[test]
    fn ignores_a_silent_connection_then_takes_the_real_redirect() {
        use std::io::Read;
        use std::net::TcpStream;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let handle = std::thread::spawn(move || {
            // A preconnect: opened, never written to, left to the read timeout.
            let _silent = TcpStream::connect(("127.0.0.1", port)).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            // Then the real one.
            let mut real = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(real, "GET /callback?code=GOOD&state=st123 HTTP/1.1\r\n\r\n").unwrap();
            let mut sink = Vec::new();
            let _ = real.read_to_end(&mut sink);
        });

        let code = wait_for_callback(&listener, "st123", &crate::engine::Cancel::new()).unwrap();
        assert_eq!(code, "GOOD");
        handle.join().unwrap();
    }

    #[test]
    fn refuses_a_callback_carrying_a_different_state() {
        use std::net::TcpStream;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let _ = write!(s, "GET /callback?code=EVIL&state=someone_else HTTP/1.1\r\n\r\n");
        });

        let err = wait_for_callback(&listener, "mine", &crate::engine::Cancel::new()).unwrap_err();
        assert!(matches!(err, Error::StateMismatch), "got {err:?}");
    }
}
