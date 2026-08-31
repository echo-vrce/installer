// SPDX-License-Identifier: GPL-3.0-or-later
//! The same code, without the window.
//!
//! Its own executable, `echo-vrce-cli`, rather than a flag on the window's.
//!
//! The window has to be a GUI subsystem binary or double-clicking it flashes a console. But
//! PowerShell does not record an exit code for a GUI subsystem process - `$LASTEXITCODE`
//! comes back empty - so every code below would be invisible to exactly the scripts they
//! exist for. Measured on Windows 10, not assumed. A subsystem is a field in the PE header,
//! decided when the binary is linked, so one file cannot be both.
//!
//! Everything here drives the same `engine` code the wizards drive; there is no second
//! implementation of anything, which is the only reason a CLI is cheap to have at all.
//!
//! Exit codes are part of the interface, because the point of a CLI is that something else
//! can branch on the result. See [`code`].

mod events;
mod style;

pub use events::Event;

use std::path::PathBuf;

use crate::engine::adb::{self, Adb};
use crate::engine::manifest::Manifest;
use crate::engine::quest::{self, Quest, Verdict};
use crate::engine::update::{self, Event as UpEvent};
use crate::engine::{download, install, pc_install, quest_install, quest_update, tools, Cancel};
use crate::fmt::{human_bytes, human_duration};
use crate::{config, endpoints, log};
use serde_json::json;
use style::Style;

/// Exit codes. Anything a script would want to branch on gets its own.
pub mod code {
    pub const OK: i32 = 0;
    pub const FAILED: i32 = 1;
    /// Bad arguments. Distinct from a failure, so a typo is not mistaken for a broken
    /// install.
    pub const USAGE: i32 = 2;
    /// The operation is right but the process lacks the rights to do it.
    pub const ELEVATION: i32 = 3;
    /// Nothing to talk to: no headset, or no adb.
    pub const NO_DEVICE: i32 = 4;
    /// Stopped with Ctrl+C. 130 is the shell convention for "interrupted", so a script
    /// that already handles that handles this.
    pub const CANCELLED: i32 = 130;
}

/// The cancel every command shares, flipped by Ctrl+C.
///
/// One for the whole process, because a signal handler has nowhere to send a message: all
/// it can safely do is set a flag, and the download loops are already checking one between
/// chunks. What they were checking was a fresh `Cancel` per command that nothing could ever
/// reach, so Ctrl+C killed the process outright instead of stopping the work.
static INTERRUPT: std::sync::OnceLock<Cancel> = std::sync::OnceLock::new();

pub fn interrupted() -> &'static Cancel {
    INTERRUPT.get_or_init(Cancel::new)
}

/// Turns Ctrl+C into a cancel rather than a killed process.
///
/// The difference matters for a downloader: killed halfway, the partial file survives but
/// nothing says so, and the exit looks like a crash. Cancelled, the work stops between
/// chunks, the partial file is kept on purpose, and the user is told it will carry on.
pub fn catch_interrupt() {
    let _ = interrupted();

    #[cfg(unix)]
    unsafe {
        extern "C" fn on_signal(_: libc::c_int) {
            // The only thing that is safe to do in a signal handler. Everything the user
            // sees is printed later, by the thread that notices.
            INTERRUPT.get().map(|c| c.cancel());
        }
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // windows-sys spells a Win32 BOOL as a plain i32; there is no BOOL alias here.
        unsafe extern "system" fn on_ctrl(_kind: u32) -> i32 {
            INTERRUPT.get().map(|c| c.cancel());
            // TRUE: handled, so Windows does not terminate the process and the work can
            // wind down on its own.
            1
        }
        SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
}

/// Makes a Windows console understand the escapes this prints.
///
/// Without it, on a console that does not have virtual terminal processing on by default,
/// the colour sequences arrive as literal text and the output is unreadable. The output
/// code page is set to UTF-8 in the same breath, though nothing printed needs it any more.
pub fn prepare_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        };
        SetConsoleOutputCP(65001);
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = GetStdHandle(which);
            let mut mode = 0u32;
            if GetConsoleMode(handle, &mut mode) != 0 {
                SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

/// Stops the run when a file appears.
///
/// The only channel back into an elevated process the parent did not start as itself.
/// Checked on a timer rather than waited on: the work being interrupted is a download loop
/// that only looks up between chunks anyway, so a faster answer would buy nothing.
fn watch_for_cancel(path: std::path::PathBuf) {
    std::thread::spawn(move || loop {
        if path.exists() {
            interrupted().cancel();
            let _ = std::fs::remove_file(&path);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
}

/// Makes `| head` behave.
///
/// Rust sets `SIGPIPE` to ignore at startup, which turns a closed pipe into a write error
/// and then a panic - so `--help | head` dies with a backtrace where every other command
/// exits quietly. Restoring the default is what a command line tool wants; it stays off for
/// the window, which has no pipe and would rather see the error.
pub fn restore_sigpipe() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

struct Args {
    /// Subcommand, then any subcommand of that. Never a modifier: those are all flags.
    command: Vec<String>,
    path: Option<String>,
    out: Option<String>,
    serial: Option<String>,
    quiet: bool,
    no_color: bool,
    yes: bool,
    keep_archive: bool,
    clear: bool,
    log: Option<String>,
    from: Option<String>,
    json: bool,
    events: bool,
    help: bool,
    version: bool,
    error: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            command: Vec::new(),
            path: None,
            out: None,
            serial: None,
            quiet: false,
            no_color: false,
            yes: false,
            keep_archive: false,
            clear: false,
            log: None,
            from: None,
            json: false,
            events: false,
            help: false,
            version: false,
            error: None,
        }
    }
}

/// Ordinary getopt conventions, hand-rolled rather than pulling in a crate for it.
///
/// What that means concretely, because these are the things people actually type:
/// `--path X` and `--path=X` are the same, short flags cluster (`-qy`), a short option that
/// takes a value can be `-p X` or `-pX`, and `--` ends option parsing so a path beginning
/// with a dash is still reachable.
fn parse(argv: &[String]) -> Args {
    let mut a = Args::default();
    let mut it = argv.iter().cloned().peekable();
    let mut only_positional = false;

    // Takes the value attached to an option, or the next argument if there is none.
    fn value(
        a: &mut Args,
        attached: Option<String>,
        it: &mut std::iter::Peekable<impl Iterator<Item = String>>,
        flag: &str,
    ) -> Option<String> {
        match attached.or_else(|| it.next()) {
            Some(v) => Some(v),
            None => {
                a.error = Some(format!("{flag} needs a value"));
                None
            }
        }
    }

    // A path option gets the same clean-up as a path typed into the window: someone who
    // copies a path from Explorer gets it quoted, and pasting that into a shell that does
    // not strip quotes - or into a batch file - keeps them. Same rule in both places, which
    // is the whole point of the two front ends sharing one engine.
    fn path_value(
        a: &mut Args,
        attached: Option<String>,
        it: &mut std::iter::Peekable<impl Iterator<Item = String>>,
        flag: &str,
    ) -> Option<String> {
        value(a, attached, it, flag)
            .map(|v| crate::engine::path_input::clean(&v).to_string())
    }

    while let Some(arg) = it.next() {
        if only_positional {
            a.command.push(arg);
            continue;
        }
        if arg == "--" {
            only_positional = true;
            continue;
        }

        if let Some(body) = arg.strip_prefix("--") {
            let (name, attached) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            match name {
                // Accepted and ignored: it used to be how the window's binary was put
                // into this mode, and old habits and old scripts still type it.
                "cli" => {}
                "help" => a.help = true,
                "version" => a.version = true,
                "quiet" => a.quiet = true,
                "yes" => a.yes = true,
                "clear" => a.clear = true,
                "json" => a.json = true,
                "events" => a.events = true,
                "keep-archive" => a.keep_archive = true,
                "no-color" | "no-colour" => a.no_color = true,
                "log" => a.log = value(&mut a, attached, &mut it, "--log"),
                "from" => a.from = value(&mut a, attached, &mut it, "--from"),
                "path" => a.path = path_value(&mut a, attached, &mut it, "--path"),
                "out" => a.out = path_value(&mut a, attached, &mut it, "--out"),
                "serial" => a.serial = value(&mut a, attached, &mut it, "--serial"),
                other => a.error = Some(format!("unknown option --{other}")),
            }
            continue;
        }

        // A bare "-" is a filename by convention, not an option.
        if arg.len() > 1 && arg.starts_with('-') {
            let chars: Vec<char> = arg.chars().skip(1).collect();
            let mut i = 0;
            while i < chars.len() {
                let c = chars[i];
                // Everything after a value-taking letter in the same argument is its value,
                // which is what makes `-pX` work.
                let rest: String = chars[i + 1..].iter().collect();
                let attached = if rest.is_empty() { None } else { Some(rest) };
                match c {
                    'c' => {}
                    'h' => a.help = true,
                    'V' => a.version = true,
                    'q' => a.quiet = true,
                    'y' => a.yes = true,
                    'p' => {
                        a.path = path_value(&mut a, attached, &mut it, "-p");
                        break;
                    }
                    'o' => {
                        a.out = value(&mut a, attached, &mut it, "-o");
                        break;
                    }
                    's' => {
                        a.serial = value(&mut a, attached, &mut it, "-s");
                        break;
                    }
                    other => {
                        a.error = Some(format!("unknown option -{other}"));
                        break;
                    }
                }
                i += 1;
            }
            continue;
        }

        a.command.push(arg);
    }
    a
}

pub fn run(argv: &[String]) -> i32 {
    let args = parse(argv);
    let st = Style::detect(args.no_color, args.quiet, args.json);

    // --log is how an elevated run reports back to the process that spawned it: the parent
    // names the file, then reads it as it fills.
    match &args.log {
        Some(p) => log::init_at(std::path::Path::new(p), false),
        None => log::init(&config::logs_dir(), false),
    };
    // Events go to the same file as the prose, so the parent has one thing to read.
    if args.events {
        if let Some(p) = &args.log {
            let log = std::path::PathBuf::from(p);
            events::to(log.clone());
            // And the same file, plus an extension, is how the parent asks this to stop.
            // Polled rather than signalled because the parent cannot signal an elevated
            // process it did not create as itself.
            watch_for_cancel(crate::engine::elevate::cancel_path(&log));
        }
    }
    log::install_panic_hook();

    if let Some(problem) = &args.error {
        st.err(problem);
        st.info("run with --help for the list");
        return fail(st, code::USAGE, problem);
    }
    // Flags win over subcommands, which is what every tool does: `foo bar --help` explains
    // rather than runs. It resolves to the most specific thing named, so `quest update
    // --help` is about that subcommand and not about the group or the program.
    if args.help {
        return help_for(st, &args.command);
    }
    if args.version {
        return version(st);
    }

    let verb = args.command.first().map(|s| s.as_str()).unwrap_or("help");
    let sub = args.command.get(1).map(|s| s.as_str());

    match (verb, sub) {
        // `get(1..)` and not `[1..]`: no command at all leaves the list empty, and slicing
        // an empty vector from 1 panics - which in a GUI binary is a silent death.
        ("help", _) => help_for(st, args.command.get(1..).unwrap_or(&[])),
        ("version", _) => version(st),
        ("devices", _) => devices(st),
        ("status", _) => pc_status(st, args.path.as_deref()),
        ("update", _) => pc_update(st, args.path.as_deref()),
        ("install", _) => pc_install_cmd(st, args.path.as_deref(), args.keep_archive, args.yes),
        ("quest", Some("status")) => quest_status(st, args.serial.as_deref()),
        ("quest", Some("update")) => quest_update_cmd(st, args.serial.as_deref()),
        ("quest", Some("install")) => quest_install_cmd(st, args.serial.as_deref(), args.yes),
        ("quest", Some("launch")) => quest_launch(st, args.serial.as_deref()),
        ("quest", sub) => sub_help(st, "quest", sub, &["status", "update", "install", "launch"]),
        ("patch", _) => patch(st, args.path.as_deref(), args.from.as_deref(), args.yes),
        ("deps", _) => deps(st),
        ("self-update", Some("check")) => self_update_check(st),
        ("self-update", Some("apply")) => self_update_apply(st, args.yes),
        ("self-update", sub) => sub_help(st, "self-update", sub, &["check", "apply"]),
        ("adb", Some("install")) => adb_install(st, args.yes),
        ("adb", Some("use")) => adb_use(st, args.path.as_deref()),
        ("adb", Some("forget")) => adb_forget(st),
        ("adb", sub) => sub_help(st, "adb", sub, &["install", "use", "forget"]),
        ("revive", Some("install")) => revive_install(st, args.yes),
        ("revive", Some("setup")) => revive_setup(st, args.path.as_deref(), args.yes),
        ("revive", Some("use")) => revive_use(st, args.path.as_deref()),
        ("revive", Some("forget")) => revive_forget(st),
        ("revive", sub) => sub_help(st, "revive", sub, &["install", "setup", "use", "forget"]),
        ("logs", _) => logs(st, args.out.as_deref(), args.serial.as_deref()),
        ("cache", Some(stray)) => {
            st.err(&format!("cache takes no subcommand, so `{stray}` was not understood"));
            st.info("did you mean: cache --clear");
            code::USAGE
        }
        ("cache", None) => cache(st, args.clear),
        (other, _) => {
            st.err(&format!("unknown command: {other}"));
            st.info("run with --help for the list");
            fail(st, code::USAGE, &format!("unknown command: {other}"))
        }
    }
}

/// Picks the most specific help for what was named, and says so when nothing matches.
///
/// Takes the whole command path rather than one word, because `quest update` is two and
/// resolving only the first would answer a question nobody asked.
fn help_for(st: Style, command: &[String]) -> i32 {
    let joined = command.join(" ");
    if joined.is_empty() {
        usage(st);
        return code::OK;
    }
    if let Some(c) = find_command(&joined) {
        command_help(st, c);
        return code::OK;
    }
    if joined == "quest" {
        quest_help(st);
        return code::OK;
    }
    // Real verbs with nothing worth a page of their own. Asking about something that exists
    // should never be a usage error, and it was: `version --help` exited 2.
    if matches!(joined.as_str(), "version" | "help") {
        usage(st);
        return code::OK;
    }
    // A bare `update` matched above; getting here means the name is not one of ours.
    st.err(&format!("no help for `{joined}`: not a command"));
    usage(st);
    code::USAGE
}

/// One option, described once. Both the global list and every per-command list read from
/// here, so a flag cannot end up documented two different ways.
struct Opt {
    flag: &'static str,
    what: &'static str,
}

const OPTIONS: &[Opt] = &[
    Opt { flag: "-p, --path <dir>", what: "install root (the folder Echo VR lives in)" },
    Opt { flag: "-o, --out <dir>", what: "where to write the support bundle" },
    Opt { flag: "-s, --serial <id>", what: "which headset, when more than one is attached" },
    Opt { flag: "-y, --yes", what: "go ahead without the confirmation" },
    Opt { flag: "    --clear", what: "actually remove the cached files" },
    Opt { flag: "    --keep-archive", what: "keep the 4.68 GB archive after extracting" },
    Opt { flag: "-q, --quiet", what: "only errors and warnings" },
    Opt { flag: "    --no-color", what: "plain output (NO_COLOR is honoured too)" },
    Opt { flag: "    --json", what: "one JSON object on stdout; diagnostics on stderr" },
    Opt {
        flag: "    --events",
        what: "with --log: add machine-readable progress lines to it",
    },
    Opt {
        flag: "    --from <file>",
        what: "with `patch`: apply a patch already downloaded, skipping Discord",
    },
    Opt { flag: "    --log <file>", what: "write this run's log here instead of the usual place" },
    Opt { flag: "-h, --help", what: "help for the command in front of it, or this text" },
    Opt { flag: "-V, --version", what: "print the version and exit" },
];

struct Command {
    name: &'static str,
    /// The grammar, minus the program name.
    usage: &'static str,
    summary: &'static str,
    /// What it does and what it will not do. This is the part worth reading twice.
    detail: &'static [&'static str],
    /// Flags that change this command's behaviour, by their entry in `OPTIONS`.
    opts: &'static [&'static str],
    examples: &'static [(&'static str, &'static str)],
    /// Exit codes beyond 0 and 2, which every command shares.
    exits: &'static [(i32, &'static str)],
}

const COMMANDS: &[Command] = &[
    Command {
        name: "status",
        usage: "status --path <dir>",
        summary: "inspect a PC install and say what an update would do",
        detail: &[
            "Reads the folder, fetches the manifest, and works out which files are already",
            "correct and which are not. Changes nothing on disk, so it is safe to run",
            "against an install someone is playing on.",
            "",
            "Having work to do is not an error: this exits 0 either way, so `status &&",
            "update` behaves. Look at the counts, not the exit code, to decide.",
        ],
        opts: &["-p, --path <dir>"],
        examples: &[
            ("echo-vrce-cli status --path 'D:\\Games\\Echo VR'", "what would an update change?"),
        ],
        exits: &[(code::FAILED, "the folder or the manifest could not be read")],
    },
    Command {
        name: "update",
        usage: "update --path <dir>",
        summary: "apply the current manifest to a PC install",
        detail: &[
            "Deletes what the manifest says to delete, then fetches what is missing or",
            "wrong, verifying every file against its sha256 before placing it. Files that",
            "already match are left alone, so re-running costs almost nothing.",
            "",
            "Interrupted downloads resume: partly fetched files are kept and continued from",
            "where they stopped, rather than started again.",
        ],
        opts: &["-p, --path <dir>"],
        examples: &[
            ("echo-vrce-cli update -p 'D:\\Games\\Echo VR'", "bring an install up to date"),
            ("echo-vrce-cli update -p . -q", "same, printing only problems"),
        ],
        exits: &[
            (code::FAILED, "a download or a file operation failed"),
            (code::ELEVATION, "the folder needs administrator rights"),
        ],
    },
    Command {
        name: "install",
        usage: "install --path <dir> [--yes] [--keep-archive]",
        summary: "download and install the PC client, then update it",
        detail: &[
            "Picks the fastest of the three mirrors, downloads the 4.68 GB archive with",
            "resume, extracts it, and then applies the current manifest on top. Free space",
            "is checked first, because running out at 90% wastes the whole download.",
            "",
            "Asks before starting unless --yes is given. With no terminal to ask on it",
            "declines rather than assuming consent.",
        ],
        opts: &["-p, --path <dir>", "-y, --yes", "    --keep-archive"],
        examples: &[
            ("echo-vrce-cli install -p 'D:\\Games\\Echo VR' -y", "unattended install"),
        ],
        exits: &[
            (code::FAILED, "no mirror answered, not enough space, or a download failed"),
            (code::ELEVATION, "the folder needs administrator rights"),
        ],
    },
    Command {
        name: "quest status",
        usage: "quest status [--serial <id>]",
        summary: "what is on the headset and whether it can be updated",
        detail: &[
            "Reports the model, the installed build, and whether this install is one the",
            "updater recognises. Touches nothing.",
            "",
            "A personalised APK is repacked, so its hash can never equal the manifest's.",
            "What makes an install recognisable is the record this installer writes on the",
            "headset; without it, only a stock install can be identified.",
        ],
        opts: &["-s, --serial <id>"],
        examples: &[("echo-vrce-cli quest status", "is the headset ready to update?")],
        exits: &[
            (code::FAILED, "the manifest could not be read"),
            (code::NO_DEVICE, "no headset, or adb not found"),
        ],
    },
    Command {
        name: "quest update",
        usage: "quest update [--serial <id>]",
        summary: "apply the current manifest to the headset",
        detail: &[
            "Asks the headset to hash what it already has, then pushes only what differs.",
            "",
            "Some headsets have no sha256sum, in which case nothing can be skipped and every",
            "file is pushed on every run. That is reported when it happens, rather than",
            "looking like the previous update did not take.",
        ],
        opts: &["-s, --serial <id>"],
        examples: &[("echo-vrce-cli quest update", "update the attached headset")],
        exits: &[
            (code::FAILED, "the manifest or a push failed"),
            (code::NO_DEVICE, "no headset, or adb not found"),
        ],
    },
    Command {
        name: "quest install",
        usage: "quest install [--serial <id>] [--yes]",
        summary: "sideload the APK and game data",
        detail: &[
            "Downloads the base APK named by the manifest and the game data, installs both,",
            "writes the install record, and then applies the current manifest. That last",
            "step is not optional: an install that stops before it has none of the asset",
            "patches, and nothing later will notice.",
            "",
            "Replaces whatever is on the headset. Asks first unless --yes is given.",
        ],
        opts: &["-s, --serial <id>", "-y, --yes"],
        examples: &[("echo-vrce-cli quest install -y", "unattended sideload")],
        exits: &[
            (code::FAILED, "a download or an adb step failed"),
            (code::NO_DEVICE, "no headset, or adb not found"),
        ],
    },
    Command {
        name: "patch",
        usage: "patch --path <dir> [--yes]",
        summary: "the licence patch, for someone who does not own Echo VR",
        detail: &[
            "Opens Discord in a browser so you can authorise it, then downloads a copy of",
            "the patch built for your account and places it beside echovr.exe.",
            "",
            "It waits on a person, so it is not something to run unattended. The link it",
            "gets back is signed and expires after 24 hours, and an expired one answers 404",
            "- indistinguishable from a missing file, which is why that case is called out",
            "by name when it happens.",
        ],
        opts: &["-p, --path <dir>", "    --from <file>", "-y, --yes"],
        examples: &[("echo-vrce-cli patch -p 'D:\\Games\\Echo VR'", "patch an install")],
        exits: &[(code::FAILED, "no install there, no browser, or Discord refused")],
    },
    Command {
        name: "deps",
        usage: "deps",
        summary: "what adb and Revive are, and where this app keeps its files",
        detail: &[
            "Reports and changes nothing. The same three things the Dependencies panel",
            "shows: which adb is in use and how it was found, whether Revive is installed,",
            "and where settings and logs live.",
        ],
        opts: &[],
        examples: &[("echo-vrce-cli deps", "what is set up?")],
        exits: &[],
    },
    Command {
        name: "self-update check",
        usage: "self-update check",
        summary: "ask whether a newer installer has been published",
        detail: &[
            "One request for a file naming the newest published version. Nothing",
            "identifying is sent and nothing is installed. The answer is remembered, so",
            "`--version` can mention it afterwards without going to the network.",
        ],
        opts: &[],
        examples: &[("echo-vrce-cli self-update check", "is there a newer one?")],
        exits: &[(code::FAILED, "the version could not be fetched")],
    },
    Command {
        name: "self-update apply",
        usage: "self-update apply [--yes]",
        summary: "download the newest installer and replace this one",
        detail: &[
            "Replaces both executables, because a window from one version driving a",
            "command line from another is a protocol mismatch waiting to happen. The two",
            "being replaced are renamed with .old beside the new ones, so going back is a",
            "rename away; they are removed the next time the new build starts.",
            "",
            "Refuses when the folder cannot be written to, which is what happens under",
            "C:\\Program Files: elevating would mean running the very file being replaced.",
        ],
        opts: &["-y, --yes"],
        examples: &[("echo-vrce-cli self-update apply -y", "install it")],
        exits: &[(code::FAILED, "the download failed, or this folder is read only")],
    },
    Command {
        name: "adb install",
        usage: "adb install [--yes]",
        summary: "download adb from Google and use that copy",
        detail: &[
            "Fetches platform-tools and unpacks it into this app's own folder, touching",
            "nothing else on the machine.",
            "",
            "Replacing an existing copy stops the adb server first, because a running one",
            "cannot be overwritten: that drops any headset connection. The copy you have is",
            "kept until the new one is unpacked and in place, so a failure leaves you with",
            "the adb that was working.",
        ],
        opts: &["-y, --yes"],
        examples: &[("echo-vrce-cli adb install -y", "unattended")],
        exits: &[(code::FAILED, "the download or the replacement failed")],
    },
    Command {
        name: "adb use",
        usage: "adb use --path <file>",
        summary: "use an adb you already have",
        detail: &[
            "Takes priority over anything found automatically. It is run once before being",
            "stored, so a path that does not work is refused here rather than at the next",
            "thing that reaches for a headset.",
            "",
            "`adb forget` undoes it.",
        ],
        opts: &["-p, --path <dir>"],
        examples: &[
            ("echo-vrce-cli adb use -p C:\\platform-tools\\adb.exe", "use your own"),
            ("echo-vrce-cli adb forget", "go back to finding it automatically"),
        ],
        exits: &[(code::FAILED, "that file is not an adb that runs")],
    },
    Command {
        name: "revive install",
        usage: "revive install [--yes]",
        summary: "download and run Revive's installer",
        detail: &[
            "Only needed to play through SteamVR; a headset over USB does not use it.",
            "",
            "This runs somebody else's installer, and it asks for administrator rights, so",
            "the Windows prompt has to be answered on screen. `--yes` skips this app's",
            "question, not that one.",
        ],
        opts: &["-y, --yes"],
        examples: &[("echo-vrce-cli revive install", "install Revive")],
        exits: &[(code::FAILED, "the download failed or the prompt was dismissed")],
    },
    Command {
        name: "revive setup",
        usage: "revive setup --path <dir>",
        summary: "add Echo VR to Revive and to SteamVR",
        detail: &[
            "Makes a desktop shortcut that launches Echo through Revive's injector, and adds",
            "an entry to Revive's vrmanifest so it appears in SteamVR.",
            "",
            "The vrmanifest entry uses a path relative to a Meta library, because that is how",
            "Revive resolves it. An Echo VR installed anywhere else gets a SteamVR entry",
            "pointing at nothing; the desktop shortcut works either way.",
            "",
            "So when the registry does not place the install in a Meta library, this stops and",
            "writes nothing rather than producing an entry that cannot launch. The window puts",
            "a confirmation in the way at the same point; --yes is how you give it here.",
        ],
        opts: &["-p, --path <dir>", "-y, --yes"],
        examples: &[
            ("echo-vrce-cli revive setup -p 'D:\\Games\\Echo VR'", "wire it up"),
            (
                "echo-vrce-cli revive setup -p C:\\EchoVR --yes",
                "outside a Meta library, and you want the entry anyway",
            ),
        ],
        exits: &[
            (code::FAILED, "Revive is not installed, or the manifest could not be read"),
            (code::FAILED, "outside a Meta library and --yes was not given"),
        ],
    },
    Command {
        name: "revive use",
        usage: "revive use --path <dir>",
        summary: "use a Revive you already have",
        detail: &[
            "The folder holding ReviveInjector.exe. Takes priority over anything found",
            "automatically, and is checked before being stored.",
            "",
            "`revive forget` undoes it.",
        ],
        opts: &["-p, --path <dir>"],
        examples: &[
            ("echo-vrce-cli revive use -p 'C:\\Program Files\\Revive'", "point at your own"),
            ("echo-vrce-cli revive forget", "go back to finding it automatically"),
        ],
        exits: &[(code::FAILED, "no ReviveInjector.exe in that folder")],
    },
    Command {
        name: "quest launch",
        usage: "quest launch [--serial <id>]",
        summary: "start Echo VR on the headset",
        detail: &[
            "Starts the game by naming its activity outright.",
            "",
            "Echo's package declares no launcher category, so it does not appear in the",
            "headset's own library and cannot be started from there. Naming the activity is",
            "how it gets started at all; it is not a shortcut around anything.",
        ],
        opts: &["-s, --serial <id>"],
        examples: &[("echo-vrce-cli quest launch", "start it on the attached headset")],
        exits: &[
            (code::FAILED, "not installed, or the headset refused"),
            (code::NO_DEVICE, "no headset, or adb not found"),
        ],
    },
    Command {
        name: "devices",
        usage: "devices",
        summary: "list headsets adb can see",
        detail: &[
            "Prints which adb is being used and where it came from, then every attached",
            "device and its state. A headset that has not authorised this computer is called",
            "out separately, because that one has a specific fix: put it on and accept the",
            "prompt.",
        ],
        opts: &[],
        examples: &[("echo-vrce-cli devices", "which headsets are attached?")],
        exits: &[(code::NO_DEVICE, "nothing attached, or adb not found")],
    },
    Command {
        name: "logs",
        usage: "logs [--out <dir>] [--serial <id>]",
        summary: "collect a support bundle from the headset",
        detail: &[
            "Pulls Echo's own logs, the asset patch log, the install record, a summary of",
            "what the headset is, and this installer's log, into one zip. This is the file",
            "to attach when asking anyone for help.",
            "",
            "The path of the zip is printed on its own line on stdout, even under --quiet,",
            "so a script can capture exactly one thing.",
        ],
        opts: &["-o, --out <dir>", "-s, --serial <id>"],
        examples: &[
            ("echo-vrce-cli logs", "write the bundle to the app data folder"),
            ("bundle=$(echo-vrce-cli logs -q -o .)", "capture just the path"),
        ],
        exits: &[
            (code::FAILED, "nothing could be collected"),
            (code::NO_DEVICE, "no headset, or adb not found"),
        ],
    },
    Command {
        name: "cache",
        usage: "cache [--clear]",
        summary: "show cached downloads, and optionally remove them",
        detail: &[
            "Lists partly finished downloads and staged files with their sizes. Nothing is",
            "removed unless --clear is given, and the listing is printed before the removal",
            "either way, so the numbers can be read rather than trusted.",
            "",
            "Removing them is safe: anything still needed is fetched again, and a partly",
            "finished download resumes rather than restarting.",
        ],
        opts: &["    --clear"],
        examples: &[
            ("echo-vrce-cli cache", "what is taking up room?"),
            ("echo-vrce-cli cache --clear", "remove it"),
        ],
        exits: &[(code::FAILED, "something could not be deleted")],
    },
];

fn find_command(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

fn opt_line(st: Style, flag: &str) -> Option<String> {
    let o = OPTIONS.iter().find(|o| o.flag == flag)?;
    Some(format!("  {}  {}", st.dim(&format!("{:<18}", o.flag)), o.what))
}

/// The overview: what commands exist, and where to look next.
/// The version, however it was asked for.
///
/// One function because there are two spellings of the same question - the flag and the
/// verb - and they were answering it differently: `--version --json` produced an object and
/// `version --json` produced a line of prose.
fn version(st: Style) -> i32 {
    if st.json {
        return st.emit(
            code::OK,
            json!({
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
                "licence": env!("CARGO_PKG_LICENSE"),
                "latest_seen": crate::config::Settings::load().update_latest_seen,
            }),
        );
    }
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    // From what the last check already found, never a request of its own: `--version` is a
    // thing scripts call, and it has no business reaching the network to answer.
    let s = crate::config::Settings::load();
    if let Some(latest) = s.update_latest_seen.as_deref() {
        if crate::engine::selfupdate::is_newer(latest, crate::engine::selfupdate::current()) {
            println!("{latest} is available: {}", crate::endpoints::RELEASE_LATEST);
        }
    }
    // The same notice the window shows on its About screen. GPL-3.0 section 5(d) asks for
    // it wherever the program talks to a person, and a command line is one of those places.
    println!("Copyright (C) 2026 kekt8c.");
    println!("Licence {}: <{}>", env!("CARGO_PKG_LICENSE"), crate::endpoints::LICENCE);
    println!("This program comes with absolutely no warranty.");
    println!("It is free software, and you are welcome to redistribute it.");
    code::OK
}

fn usage(st: Style) {
    // Not silenced by --quiet or --json. Those say "spare me the running commentary"; asking
    // for the help and getting nothing back is not that, it is a broken program.
    let st = Style { quiet: false, ..st };
    println!("\n  {} {}", st.bold("Echo VRCE Installer"), st.dim(env!("CARGO_PKG_VERSION")));
    println!("  {}", st.dim("the same code, without the window"));

    st.heading("USAGE");
    println!("  echo-vrce-cli <command> [options]");
    println!(
        "\n  {}",
        st.dim("The window is echo-vrce-installer. This is the same code without it.")
    );

    st.heading("COMMANDS");
    // Wide enough for the longest name there is, worked out rather than guessed: a hard
    // coded width silently breaks the column the first time somebody adds a longer command.
    let w = COMMANDS.iter().map(|c| c.name.len()).max().unwrap_or(14);
    for c in COMMANDS {
        println!("  {}  {}", st.accent(&format!("{:<w$}", c.name)), c.summary);
    }
    println!("\n  {}", st.dim("echo-vrce-cli <command> --help  for any of them"));

    st.heading("OPTIONS");
    for o in OPTIONS {
        println!("  {}  {}", st.dim(&format!("{:<18}", o.flag)), o.what);
    }
    println!("\n  {}", st.dim("--opt=value and --opt value are the same; short flags cluster (-qy)"));
    println!("  {}", st.dim("-- ends option parsing, for a path that begins with a dash"));

    st.heading("EXIT CODES");
    for (n, what) in [
        (code::OK, "success, including \"nothing to do\""),
        (code::FAILED, "the operation failed"),
        (code::USAGE, "bad arguments"),
        (code::ELEVATION, "needs administrator rights"),
        (code::NO_DEVICE, "no headset, or adb not found"),
        (code::CANCELLED, "stopped with Ctrl+C; partial downloads are kept"),
    ] {
        println!("  {}  {}", st.accent(&format!("{n:<14}")), what);
    }

    st.heading("FILES");
    println!("  {}  {}", st.dim(&format!("{:<18}", "app data")), config::dir().display());
    println!("  {}  {}", st.dim(&format!("{:<18}", "logs")), config::logs_dir().display());
    println!(
        "\n  {}",
        st.dim("ECHO_VRCE_HOME moves all of that somewhere else, for a portable install")
    );
    println!();
}

/// Help for one command. Same sections as the overview, so nothing has to be relearned.
fn command_help(st: Style, c: &Command) {
    let st = Style { quiet: false, ..st };
    println!("\n  {}  {}", st.bold(c.name), st.dim(c.summary));

    st.heading("USAGE");
    println!("  echo-vrce-cli {}", c.usage);

    if !c.detail.is_empty() {
        st.heading("DESCRIPTION");
        for line in c.detail {
            // An empty paragraph break must not carry the indent with it, or the block
            // ends up with trailing whitespace on the separator lines.
            if line.is_empty() {
                println!();
            } else {
                println!("  {line}");
            }
        }
    }

    if !c.opts.is_empty() {
        st.heading("OPTIONS");
        for flag in c.opts {
            if let Some(line) = opt_line(st, flag) {
                println!("{line}");
            }
        }
        println!("\n  {}", st.dim("plus the global options; see --help on its own"));
    }

    if !c.examples.is_empty() {
        st.heading("EXAMPLES");
        for (i, (cmd, why)) in c.examples.iter().enumerate() {
            if i > 0 {
                println!();
            }
            println!("  {}", st.dim(&format!("# {why}")));
            println!("  {}", st.accent(cmd));
        }
    }

    st.heading("EXIT CODES");
    // Sorted, because a list that reads 0, 1, 3, 2 looks like a mistake even when the
    // entries are right.
    let mut exits = vec![(code::OK, "success"), (code::USAGE, "bad arguments")];
    exits.extend(c.exits.iter().copied());
    exits.sort_by_key(|(n, _)| *n);
    for (n, what) in exits {
        println!("  {}  {}", st.accent(&format!("{n:<14}")), what);
    }
    println!();
}

/// Help for `quest` on its own, which is a group rather than a command.
fn quest_help(st: Style) {
    let st = Style { quiet: false, ..st };
    println!("\n  {}  {}", st.bold("quest"), st.dim("everything that talks to a headset"));
    st.heading("USAGE");
    println!("  echo-vrce-cli quest <status|update|install> [options]");
    st.heading("SUBCOMMANDS");
    for c in COMMANDS.iter().filter(|c| c.name.starts_with("quest ")) {
        let short = c.name.trim_start_matches("quest ");
        println!("  {}  {}", st.accent(&format!("{short:<14}")), c.summary);
    }
    println!("\n  {}", st.dim("echo-vrce-cli quest <subcommand> --help  for detail"));
    println!();
}

// ---------------------------------------------------------------- shared helpers

fn need_path(st: Style, path: Option<&str>) -> Result<PathBuf, i32> {
    match path {
        Some(p) if !p.is_empty() => {
            let typed = PathBuf::from(p);
            // The same reading the window does: someone who gives the folder echovr.exe is
            // in has answered the question, just not in the units it was asked in. Said out
            // loud, because the path a command acted on should never be a surprise.
            match install::root_of(&typed) {
                Some(root) if root != typed => {
                    st.info(&format!("that is inside an install; using {}", root.display()));
                    Ok(root)
                }
                _ => Ok(typed),
            }
        }
        _ => {
            st.err("this command needs --path <install root>");
            Err(fail(st, code::USAGE, "this command needs --path <install root>"))
        }
    }
}

fn fetch_manifest(st: Style, url: &str) -> Result<Manifest, i32> {
    // Labelled before it blocks. This call retries a dropped connection with a backoff, so
    // without a line here a bad network looks like the command hung.
    st.info("fetching the manifest");
    let text = match download::fetch_text_reporting(url, &mut |n, _| {
        // Short on purpose. The full reason is printed once, at the end, if it never
        // succeeds; repeating the same paragraph on every attempt buries it.
        st.warn(&format!("no answer - trying again ({n}/{})", download::RETRIES));
    }) {
        Ok(t) => t,
        Err(e) => {
            // Not prefixed: the error already says it could not reach anything, and
            // "could not fetch the manifest: could not reach the server:" says it twice.
            st.err(&format!("{e}"));
            return Err(fail(st, code::FAILED, &e.to_string()));
        }
    };
    match Manifest::parse(&text, url) {
        Ok(m) => Ok(m),
        Err(e) => {
            st.err(&format!("the manifest was rejected: {e}"));
            Err(fail(st, code::FAILED, &format!("the manifest was rejected: {e}")))
        }
    }
}

/// Locates adb and picks a device, reporting the specific reason it could not.
fn open_device(st: Style, serial: Option<&str>) -> Result<(Adb, adb::Device), i32> {
    let Some(found) = adb::locate(config::Settings::load().adb_path.as_deref()) else {
        st.err("adb was not found. Set it in the app under Dependencies, or put it on PATH.");
        return Err(fail(st, code::NO_DEVICE, "adb was not found"));
    };
    let adb = Adb::at(&found.path);
    let devices = match adb.devices() {
        Ok(d) => d,
        Err(e) => {
            st.err(&format!("adb would not answer: {e}"));
            return Err(fail(st, code::NO_DEVICE, &format!("adb would not answer: {e}")));
        }
    };
    let ready: Vec<_> = devices.iter().filter(|d| d.state == adb::State::Ready).collect();

    if let Some(want) = serial {
        return match ready.iter().find(|d| d.serial == want) {
            Some(d) => Ok((adb, (*d).clone())),
            None => {
                st.err(&format!("no ready headset with serial {want}"));
                Err(fail(st, code::NO_DEVICE, &format!("no ready headset with serial {want}")))
            }
        };
    }
    match ready.len() {
        0 => {
            // Unauthorised is the common case and has a specific fix, so it is worth
            // saying rather than folding into "no devices".
            if devices.iter().any(|d| d.state == adb::State::Unauthorized) {
                st.err("a headset is attached but has not authorised this computer. \
                        Put it on and accept the prompt.");
                Err(fail(st, code::NO_DEVICE, "a headset is attached but not authorised"))
            } else {
                st.err("no headset detected");
                Err(fail(st, code::NO_DEVICE, "no headset detected"))
            }
        }
        1 => Ok((adb, ready[0].clone())),
        n => {
            st.err(&format!("{n} headsets attached; choose one with --serial"));
            for d in ready {
                let model = d.model.clone().unwrap_or_else(|| "unknown model".into());
                st.plain(&format!("    -s {:<18}  {model}", d.serial));
            }
            Err(fail(st, code::NO_DEVICE, &format!("{n} headsets attached; choose one")))
        }
    }
}

/// Renders update progress, and returns whether anything was reported off a terminal so
/// milestones can be printed instead of a bar.
fn on_update_event(st: Style, e: &UpEvent) {
    match e {
        UpEvent::Deleting { rel, index, of } => {
            st.progress_done();
            st.info(&format!("[{index}/{of}] removing {rel}"));
            events::emit(&Event::Item { name: rel.clone(), index: *index, of: *of });
        }
        UpEvent::Fetching { rel, index, of, snapshot } => {
            if snapshot.done == 0 {
                st.progress_done();
                st.info(&format!("[{index}/{of}] {rel}"));
                events::emit(&Event::Item { name: rel.clone(), index: *index, of: *of });
            }
            st.download(snapshot);
            events::emit(&Event::Progress {
                what: rel.clone(),
                done: snapshot.done,
                total: snapshot.total,
            });
        }
        UpEvent::Placed { .. } => {}
    }
}

// ---------------------------------------------------------------- commands

fn devices(st: Style) -> i32 {
    let Some(found) = adb::locate(config::Settings::load().adb_path.as_deref()) else {
        st.err("adb was not found");
        return st.emit(code::NO_DEVICE, json!({"ok": false, "error": "adb was not found"}));
    };
    st.heading("ADB");
    st.field("binary", &found.path.display().to_string());
    st.field("source", found.source.describe());

    let adb = Adb::at(&found.path);
    match adb.devices() {
        Ok(list) if list.is_empty() => {
            st.heading("DEVICES");
            st.info("none attached");
            st.emit(
                code::NO_DEVICE,
                json!({"ok": false, "adb": adb_json(&found), "devices": []}),
            )
        }
        Ok(list) => {
            st.heading("DEVICES");
            for d in &list {
                let name = d.model.clone().unwrap_or_else(|| "unknown model".into());
                let line = format!("{:<18}  {name}", d.serial);
                match d.state {
                    adb::State::Ready => st.ok(&line),
                    adb::State::Unauthorized => st.warn(&format!("{line}  (not authorised)")),
                    _ => st.info(&format!("{line}  ({:?})", d.state)),
                }
            }
            let devices: Vec<_> = list
                .iter()
                .map(|d| json!({
                    "serial": d.serial,
                    "model": d.model,
                    "ready": d.state == adb::State::Ready,
                    "state": format!("{:?}", d.state).to_lowercase(),
                }))
                .collect();
            st.emit(code::OK, json!({"ok": true, "adb": adb_json(&found), "devices": devices}))
        }
        Err(e) => {
            st.err(&format!("adb would not answer: {e}"));
            st.emit(code::NO_DEVICE, json!({"ok": false, "error": e.to_string()}))
        }
    }
}

/// The failure shape, identical everywhere: something parsing this should never have to
/// learn a second one.
///
/// A cancel is not one of these. If Ctrl+C was pressed, whatever error came back is a
/// consequence of stopping rather than a fault, and reporting it in red under a failing
/// exit code would be both wrong and alarming.
fn fail(st: Style, code: i32, message: &str) -> i32 {
    if interrupted().is_cancelled() {
        st.info("stopped. What downloaded is kept, so running this again carries on.");
        return st.emit(
            code::CANCELLED,
            json!({"ok": false, "cancelled": true, "code": code::CANCELLED}),
        );
    }
    st.emit(code, json!({"ok": false, "error": message, "code": code}))
}

fn adb_json(found: &adb::Located) -> serde_json::Value {
    json!({
        "path": found.path.display().to_string(),
        "source": found.source.describe(),
        "version": found.version,
    })
}

fn pc_status(st: Style, path: Option<&str>) -> i32 {
    let root = match need_path(st, path) {
        Ok(r) => r,
        Err(c) => return c,
    };
    let target = install::bin_dir(&root);
    let i = install::inspect(&root);

    st.heading("INSTALL");
    st.field("root", &root.display().to_string());
    st.field("target", &target.display().to_string());
    st.field("free", &i.free_bytes.map(human_bytes).unwrap_or_else(|| "unknown".into()));
    if !i.root_exists {
        st.warn("that folder does not exist yet");
    } else if !i.has_echo {
        st.warn("no Echo VR install found there");
    } else {
        st.ok("Echo VR is installed there");
    }
    if i.root_exists && !i.writable {
        st.warn("not writable by this process; an update would need administrator rights");
    }

    let manifest = match fetch_manifest(st, endpoints::PC_MANIFEST) {
        Ok(m) => m,
        Err(c) => return c,
    };
    let plan = match update::plan(&manifest, &target, interrupted()) {
        Ok(p) => p,
        Err(e) => {
            st.err(&format!("could not work out what to do: {e}"));
            return code::FAILED;
        }
    };

    st.heading("UPDATE");
    st.field("manifest", &format!("{} entries", manifest.entries().len()));
    st.field("current", &format!("{} files", plan.up_to_date.len()));
    st.field("to fetch", &format!("{} files", plan.fetches.len()));
    st.field("to remove", &format!("{} files", plan.deletes.len()));
    if plan.is_empty() {
        st.ok("already up to date");
    } else {
        // Deliberately not a failure: "there is work to do" is a normal answer, and making
        // it non-zero would break the obvious `status && update` chain.
        st.info("run `update` to apply it");
        // A listing that silently stops is worse than one that says it stopped, so the
        // count below is of what was left out, not of the whole plan.
        const SHOW: usize = 10;
        let mut shown = 0;
        for f in plan.fetches.iter().take(SHOW) {
            st.plain(&st.dim(&format!("    + {}", f.rel)));
            shown += 1;
        }
        for d in plan.deletes.iter().take(SHOW) {
            st.plain(&st.dim(&format!("    - {}", d.rel)));
            shown += 1;
        }
        let hidden = plan.work_items() - shown;
        if hidden > 0 {
            st.plain(&st.dim(&format!("    and {hidden} more")));
        }
    }
    st.emit(
        code::OK,
        json!({
            "ok": true,
            "root": root.display().to_string(),
            "target": target.display().to_string(),
            "exists": i.root_exists,
            "has_echo": i.has_echo,
            "writable": i.writable,
            "free_bytes": i.free_bytes,
            "manifest_entries": manifest.entries().len(),
            "up_to_date": plan.up_to_date.len(),
            "fetch": plan.fetches.iter().map(|s| &s.rel).collect::<Vec<_>>(),
            "delete": plan.deletes.iter().map(|s| &s.rel).collect::<Vec<_>>(),
        }),
    )
}

fn pc_update(st: Style, path: Option<&str>) -> i32 {
    let root = match need_path(st, path) {
        Ok(r) => r,
        Err(c) => return c,
    };
    let target = install::bin_dir(&root);
    let cancel = interrupted();

    // Recorded when the run starts, not when it succeeds, and for the same reason the
    // window does it: a failed install is exactly the case that leaves a 4.68 GB archive
    // behind, and the cache cleaner has to know which folder to look in.
    config::remember_install_path(&root.display().to_string());

    st.heading("UPDATE ECHO VR (PC)");
    st.field("target", &target.display().to_string());

    let manifest = match fetch_manifest(st, endpoints::PC_MANIFEST) {
        Ok(m) => m,
        Err(c) => return c,
    };
    let plan = match update::plan(&manifest, &target, &cancel) {
        Ok(p) => p,
        Err(e) => {
            st.err(&format!("could not work out what to do: {e}"));
            return code::FAILED;
        }
    };
    if plan.is_empty() {
        st.ok(&format!("already up to date ({} files checked)", plan.up_to_date.len()));
        return st.emit(
            code::OK,
            json!({"ok": true, "changed": false, "skipped": plan.up_to_date.len()}),
        );
    }
    st.info(&format!(
        "{} to fetch, {} to remove, {} already current",
        plan.fetches.len(),
        plan.deletes.len(),
        plan.up_to_date.len()
    ));

    let began = std::time::Instant::now();
    match update::apply(&plan, cancel, &mut |e| on_update_event(st, &e)) {
        Ok(s) => {
            st.progress_done();
            st.ok(&format!(
                "{} fetched, {} removed, {} unchanged in {}",
                s.fetched,
                s.deleted,
                s.skipped,
                human_duration(began.elapsed())
            ));
            events::emit(&Event::Done {
                ok: true,
                summary: format!("{} fetched, {} removed, {} unchanged", s.fetched, s.deleted, s.skipped),
            });
            st.emit(
                code::OK,
                json!({
                    "ok": true,
                    "changed": true,
                    "fetched": s.fetched,
                    "deleted": s.deleted,
                    "skipped": s.skipped,
                    "seconds": began.elapsed().as_secs(),
                }),
            )
        }
        Err(e) => {
            st.progress_done();
            st.err(&format!("{e}"));
            if e.needs_elevation() {
                st.warn("that folder needs administrator rights. Re-run elevated.");
                return fail(st, code::ELEVATION, &e.to_string());
            }
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

fn pc_install_cmd(st: Style, path: Option<&str>, keep_archive: bool, yes: bool) -> i32 {
    let root = match need_path(st, path) {
        Ok(r) => r,
        Err(c) => return c,
    };

    // Recorded when the run starts, not when it succeeds, and for the same reason the
    // window does it: a failed install is exactly the case that leaves a 4.68 GB archive
    // behind, and the cache cleaner has to know which folder to look in.
    config::remember_install_path(&root.display().to_string());

    st.heading("INSTALL ECHO VR (PC)");
    st.field("root", &root.display().to_string());

    // The same two things the window says on its licence step. They were only in the
    // window, which meant anyone scripting an install was never told either of them - and
    // both are work to do somewhere else, before this runs.
    st.heading("BEFORE YOU RUN THIS");
    st.plain("If you own Echo VR on Meta:");
    st.plain("  Install it from the Meta app and let it finish, then delete the folder it");
    st.plain("  made. Installing it there first is what registers the licence on your");
    st.plain("  account; leaving Meta's copy means Meta can repair those files later and");
    st.plain("  undo this install. The folder is:");
    let (meta_folder, folder_source) = crate::engine::meta::expected_echo_dir();
    st.plain(&format!("    {}", crate::fmt::windows_path(&meta_folder)));
    st.plain(&format!("  ({})", match folder_source {
        crate::engine::meta::Source::Registry => "read from your Meta installation",
        crate::engine::meta::Source::KnownPath => "the usual location; yours may differ",
    }));
    st.plain("");
    st.plain("If you do not own it:");
    st.plain("  The install below works either way, but the licence patch afterwards needs");
    st.plain("  a Discord account in the patcher server. The bot checks membership by name");
    st.plain("  and refuses anyone who is not in it:");
    st.plain(&format!("    {}", endpoints::DISCORD_PATCHER));
    st.plain("  The community server is a different one and is not enough on its own:");
    st.plain(&format!("    {}", endpoints::DISCORD_LOUNGE));

    st.heading("THIS FOLDER");
    let i = install::inspect(&root);
    st.field("free", &i.free_bytes.map(human_bytes).unwrap_or_else(|| "unknown".into()));
    // The folder existing is the test, not a valid install being in it: an interrupted or
    // hand-made folder has no executable and would still be deleted.
    if i.arena_exists {
        st.warn(&format!(
            "this folder already exists and will be DELETED, then replaced:\n      {}{}",
            crate::fmt::windows_path(&root.join(install::ARENA_DIR)),
            if i.has_echo { "" } else { "\n      (no echovr.exe in it, so its contents are unknown)" }
        ));
    }
    // Advisory only. The engine refuses on the real announced size before it downloads
    // anything; a second threshold here would be a worse copy of that rule, and a hardcoded
    // one would drift from it. So this reports, and lets the engine decide.
    if i.free_bytes.is_some_and(|free| free < endpoints::PC_ARCHIVE_BYTES * 2) {
        st.warn("that drive may not have room for the archive and its contents");
    }
    let question = if i.arena_exists {
        format!(
            "This deletes the existing install and downloads about {}.",
            human_bytes(endpoints::PC_ARCHIVE_BYTES)
        )
    } else {
        format!("This downloads about {}.", human_bytes(endpoints::PC_ARCHIVE_BYTES))
    };
    if !yes && !confirm(st, &question) {
        st.info("nothing was done");
        return st.emit(code::OK, json!({"ok": true, "changed": false, "declined": true}));
    }

    let cfg = pc_install::Config {
        root: root.clone(),
        archive: endpoints::PC_ARCHIVE.into(),
        mirrors: endpoints::MIRRORS.iter().map(|s| s.to_string()).collect(),
        probe: endpoints::MIRROR_PROBE.into(),
        manifest_url: endpoints::PC_MANIFEST.into(),
        keep_archive,
        // The confirmation above is what grants this, or --yes is.
        replace_existing: true,
    };
    let cancel = interrupted();
    let began = std::time::Instant::now();

    let result = pc_install::run(&cfg, cancel, &mut |e| match e {
        pc_install::Event::Stage(s) => {
            st.progress_done();
            st.info(s);
            events::emit(&Event::Stage(s.to_string()));
        }
        pc_install::Event::Probing { base, index, of } => {
            st.info(&format!("trying {base} ({index} of {of})"));
            events::emit(&Event::Item { name: base, index, of });
        }
        pc_install::Event::Mirror(m) => st.field("server", &m),
        pc_install::Event::MirrorProblem(m) => st.warn(&m),
        pc_install::Event::Downloading(snap) => {
            st.download(&snap);
            events::emit(&Event::Progress {
                what: endpoints::PC_ARCHIVE.to_string(),
                done: snap.done,
                total: snap.total,
            });
        }
        pc_install::Event::Extracting { done, total } => {
            st.progress(done, total, None, None);
            events::emit(&Event::Progress {
                what: "extracting".into(),
                done,
                total: Some(total),
            });
        }
        pc_install::Event::Updating(u) => on_update_event(st, &u),
    });
    st.progress_done();

    match result {
        Ok(r) => {
            st.ok(&format!(
                "installed: {} extracted, {} updated, in {}",
                r.extracted_files,
                r.update.fetched,
                human_duration(began.elapsed())
            ));
            events::emit(&Event::Done {
                ok: true,
                summary: format!("{} extracted, {} updated", r.extracted_files, r.update.fetched),
            });
            st.emit(
                code::OK,
                json!({
                    "ok": true,
                    "changed": true,
                    "archive_bytes": r.archive_bytes,
                    "extracted_files": r.extracted_files,
                    "updated": r.update.fetched,
                    "seconds": began.elapsed().as_secs(),
                }),
            )
        }
        Err(e) => {
            st.err(&format!("{e}"));
            if e.needs_elevation() {
                return fail(st, code::ELEVATION, &e.to_string());
            }
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

fn quest_status(st: Style, serial: Option<&str>) -> i32 {
    let (adb, device) = match open_device(st, serial) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let q = Quest::new(&adb, Some(&device));

    st.heading("HEADSET");
    st.field("serial", &device.serial);
    for (label, prop) in [("model", "ro.product.model"), ("android", "ro.build.version.release")]
    {
        if let Ok(v) = q.exec(&["shell", "getprop", prop]) {
            st.field(label, v.trim());
        }
    }

    let installed = q.installed_apk_path();
    st.field("package", if installed.is_some() { "installed" } else { "not installed" });
    if let Some(code) = q.version_code() {
        st.field("build", &code.to_string());
    }

    let manifest = match fetch_manifest(st, endpoints::QUEST_MANIFEST) {
        Ok(m) => m,
        Err(c) => return c,
    };
    let base = manifest.base_apk().map(|b| b.sha256.clone());
    let marker = q.read_marker();
    let installed_sha = q.installed_sha();
    let decision = quest::decide(base.as_deref(), marker.as_ref(), installed.is_some(), installed_sha.as_deref());

    st.heading("UPDATE");
    let (verdict, can_update, note) = match &decision.verdict {
        Verdict::Ok => {
            st.ok("this install can be updated");
            ("ok", true, None)
        }
        Verdict::NotInstalled => {
            st.warn("Echo VR is not installed. Run `quest install` first.");
            ("not_installed", false, None)
        }
        Verdict::Mismatch(why) => {
            st.warn(why);
            ("mismatch", false, Some(why.clone()))
        }
    };
    if can_update && decision.self_heal {
        st.info("no install record on the headset; updating will write one");
    }
    st.emit(
        code::OK,
        json!({
            "ok": true,
            "serial": device.serial,
            "model": device.model,
            "installed": installed.is_some(),
            "version_code": q.version_code(),
            "verdict": verdict,
            "can_update": can_update,
            "note": note,
            "self_heal": decision.self_heal,
        }),
    )
}

fn quest_update_cmd(st: Style, serial: Option<&str>) -> i32 {
    let (adb, device) = match open_device(st, serial) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let q = Quest::new(&adb, Some(&device));
    let cancel = interrupted();

    st.heading("UPDATE ECHO VR (QUEST)");
    let manifest = match fetch_manifest(st, endpoints::QUEST_MANIFEST) {
        Ok(m) => m,
        Err(c) => return c,
    };
    let Some(root) = manifest.target_root().map(|s| s.to_string()) else {
        st.err("the manifest does not say where on the device it applies");
        return code::FAILED;
    };
    st.field("target", &root);

    let plan = match quest_update::plan(&manifest, &q, cancel, &mut |e| {
        if let quest_update::Event::Hashing = e {
            st.info("asking the headset what it already has");
        }
    }) {
        Ok(p) => p,
        Err(e) => {
            st.progress_done();
            st.err(&format!("{e}"));
            return code::FAILED;
        }
    };
    st.progress_done();

    if plan.is_empty() {
        st.ok("already up to date");
        return st.emit(code::OK, json!({"ok": true, "changed": false}));
    }
    if plan.hashing_unavailable {
        // Otherwise this looks like the update did nothing the first time: it re-pushes
        // files that are already correct, every run, and never says why.
        st.warn("this headset has no sha256sum, so nothing can be skipped: every file is pushed");
    }
    st.info(&format!(
        "{} to push, {} to remove, {} already current",
        plan.pushes.len(),
        plan.deletes.len(),
        plan.up_to_date.len()
    ));

    let began = std::time::Instant::now();
    let staging = config::dir().join("staging");
    match quest_update::apply(&plan, &q, &root, &staging, cancel, &mut |e| match e {
        quest_update::Event::Downloading { rel, index, of, done, total } => {
            if done == 0 {
                st.progress_done();
                st.info(&format!("[{index}/{of}] {rel}"));
            }
            st.progress(done, total.unwrap_or(0), None, None);
        }
        // Deliberately silent: the numbered line above already named this file, and the
        // download bar is still the thing worth looking at.
        quest_update::Event::Pushing { .. } => {}
        quest_update::Event::Deleting { rel, index, of } => {
            st.progress_done();
            st.info(&format!("[{index}/{of}] removing {rel}"));
        }
        _ => {}
    }) {
        Ok(s) => {
            st.progress_done();
            st.ok(&format!(
                "{} pushed, {} removed in {}",
                s.pushed,
                s.deleted,
                human_duration(began.elapsed())
            ));
            st.emit(
                code::OK,
                json!({
                    "ok": true,
                    "changed": true,
                    "pushed": s.pushed,
                    "deleted": s.deleted,
                    "skipped": s.skipped,
                    "hashing_unavailable": plan.hashing_unavailable,
                    "seconds": began.elapsed().as_secs(),
                }),
            )
        }
        Err(e) => {
            st.progress_done();
            st.err(&format!("{e}"));
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

fn quest_install_cmd(st: Style, serial: Option<&str>, yes: bool) -> i32 {
    let (adb, device) = match open_device(st, serial) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let q = Quest::new(&adb, Some(&device));

    st.heading("INSTALL ECHO VR (QUEST)");
    let manifest = match fetch_manifest(st, endpoints::QUEST_MANIFEST) {
        Ok(m) => m,
        Err(c) => return c,
    };
    let Some(base) = manifest.base_apk().cloned() else {
        st.err("the manifest does not name a base APK");
        return fail(st, code::FAILED, "the manifest does not name a base APK");
    };
    st.field("build", &base.name);

    if !yes && !confirm(st, "This downloads about 4 GB and replaces what is on the headset.") {
        st.info("nothing was done");
        return st.emit(code::OK, json!({"ok": true, "changed": false, "declined": true}));
    }

    let cfg = quest_install::Config {
        apk_name: base.name.clone(),
        base_sha256: base.sha256.clone(),
        patched_url: None,
        mirrors: endpoints::MIRRORS.iter().map(|s| s.to_string()).collect(),
        probe: endpoints::MIRROR_PROBE.into(),
        staging: config::dir().join("staging"),
        installer_version: crate::app::VERSION.to_string(),
    };
    let cancel = interrupted();
    let began = std::time::Instant::now();

    let mut report = |e: quest_install::Event| match e {
        quest_install::Event::Stage(s) => {
            st.progress_done();
            st.info(s);
        }
        quest_install::Event::Probing { base, index, of } => {
            st.info(&format!("trying {base} ({index} of {of})"));
            events::emit(&Event::Item { name: base, index, of });
        }
        quest_install::Event::Mirror(m) => st.field("server", &m),
        quest_install::Event::MirrorProblem(m) => st.warn(&m),
        quest_install::Event::Downloading { done, total, .. } => {
            st.progress(done, total.unwrap_or(0), None, None)
        }
    };

    let (apk, data) = match quest_install::download(&cfg, cancel, &mut report) {
        Ok(p) => p,
        Err(e) => {
            st.progress_done();
            st.err(&format!("{e}"));
            return fail(st, code::FAILED, &e.to_string());
        }
    };
    match quest_install::install(&cfg, &apk, &data, Some(&manifest), &q, cancel, &mut report) {
        Ok(r) => {
            st.progress_done();
            st.ok(&format!("installed in {}", human_duration(began.elapsed())));
            st.field("sha256", &r.apk_sha256);
            st.emit(
                code::OK,
                json!({
                    "ok": true,
                    "changed": true,
                    "apk_sha256": r.apk_sha256,
                    "patched": r.patched,
                    "seconds": began.elapsed().as_secs(),
                }),
            )
        }
        Err(e) => {
            st.progress_done();
            st.err(&format!("{e}"));
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

/// One reply for "that is not a subcommand of this", named or missing.
fn sub_help(st: Style, group: &str, sub: Option<&str>, valid: &[&str]) -> i32 {
    match sub {
        Some(other) => st.err(&format!("unknown subcommand: {group} {other}")),
        None => st.err(&format!("{group} needs a subcommand")),
    }
    st.info(&format!("expected: {}", valid.join(", ")));
    fail(st, code::USAGE, &format!("{group} needs one of: {}", valid.join(", ")))
}

fn self_update_check(st: Style) -> i32 {
    use crate::engine::selfupdate;
    let current = selfupdate::current();
    st.heading("SELF UPDATE");
    st.field("running", current);
    match selfupdate::published(interrupted()) {
        Ok(latest) => {
            // Remembered whatever the answer is, so `--version` reports the same thing the
            // window does without either of them having to ask again.
            let mut settings = config::Settings::load();
            settings.update_latest_seen = Some(latest.clone());
            settings.update_checked_at = Some(crate::update_notice::now_secs());
            settings.save();

            let newer = selfupdate::is_newer(&latest, current);
            if newer {
                st.ok(&format!("{latest} is available. `self-update apply` installs it."));
            } else {
                st.ok(&format!("{current} is the newest published version"));
            }
            st.emit(
                code::OK,
                json!({"ok": true, "running": current, "latest": latest, "newer": newer}),
            )
        }
        Err(e) => {
            st.err(&e.to_string());
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

fn self_update_apply(st: Style, yes: bool) -> i32 {
    use crate::engine::selfupdate;
    st.heading("SELF UPDATE");
    st.field("from", crate::endpoints::UPDATE_ZIP);
    match selfupdate::install_dir() {
        Ok(d) => st.field("into", &d.display().to_string()),
        Err(e) => {
            st.err(&e.to_string());
            return fail(st, code::FAILED, &e.to_string());
        }
    }
    if !selfupdate::can_replace_in_place() {
        // Said before anything is downloaded rather than after: there is no point spending
        // six megabytes to find out the folder was never writable.
        let msg = "this folder cannot be written to, so the update cannot be applied here";
        st.err(msg);
        return fail(st, code::FAILED, msg);
    }
    if !yes
        && !confirm(
            st,
            "This replaces both executables in that folder with the newest published \
             build. The current two are kept beside them with .old on the name.",
        )
    {
        st.info("nothing was done");
        return st.emit(code::OK, json!({"ok": true, "changed": false, "declined": true}));
    }

    let mut stage = String::new();
    let result = selfupdate::apply(interrupted(), &mut |e| match e {
        selfupdate::Event::Stage(s) => {
            st.progress_done();
            stage = s.to_string();
            st.info(s);
        }
        selfupdate::Event::Downloading(snap) => st.download(&snap),
        selfupdate::Event::Extracting { done, total } => st.progress(done, total, None, None),
    });
    st.progress_done();
    match result {
        Ok(()) => {
            st.ok("installed. Run it again to be on the new version.");
            st.emit(code::OK, json!({"ok": true, "changed": true}))
        }
        Err(e) => {
            st.err(&format!("{stage}: {e}"));
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

fn deps(st: Style) -> i32 {
    let settings = config::Settings::load();

    st.heading("ADB");
    let adb = adb::locate(settings.adb_path.as_deref());
    match &adb {
        Some(f) => {
            st.ok(f.version.clone().unwrap_or_else(|| "installed".into()).trim());
            st.field("path", &f.path.display().to_string());
            st.field("source", f.source.describe());
        }
        None => st.warn("not found. `adb install` fetches one, or `adb use -p <file>`."),
    }

    st.heading("REVIVE");
    let revive = crate::engine::revive::locate(settings.revive_path.as_deref());
    match &revive {
        Some(f) => {
            st.ok("installed");
            st.field("path", &f.dir.display().to_string());
            st.field("source", f.source.describe());
        }
        None if !cfg!(windows) => st.info("Windows only"),
        None => st.info("not installed. `revive install` fetches it."),
    }

    st.heading("APP DATA");
    st.field("folder", &config::dir().display().to_string());
    st.field("logs", &config::logs_dir().display().to_string());

    st.emit(
        code::OK,
        json!({
            "ok": true,
            "adb": adb.as_ref().map(adb_json),
            "revive": revive.as_ref().map(|f| json!({
                "path": f.dir.display().to_string(),
                "source": f.source.describe(),
            })),
            "app_data": config::dir().display().to_string(),
            "logs": config::logs_dir().display().to_string(),
        }),
    )
}

fn adb_install(st: Style, yes: bool) -> i32 {
    let existing = adb::locate(config::Settings::load().adb_path.as_deref());
    st.heading("ADB");
    if let Some(f) = &existing {
        st.info(&format!("replacing the copy at {}", f.path.display()));
        if !yes
            && !confirm(
                st,
                "This stops the adb server first, so any headset connection drops and \
                 anything in progress on one is interrupted.",
            )
        {
            st.info("nothing was done");
            return st.emit(code::OK, json!({"ok": true, "changed": false, "declined": true}));
        }
    }
    match adb::install(interrupted(), &mut |stage, done, total| {
        if done == 0 {
            st.info(&format!("{stage:?}"));
        }
        st.progress(done, total.unwrap_or(0), None, None);
    }) {
        Ok(p) => {
            st.progress_done();
            st.ok(&format!("installed at {}", p.display()));
            st.emit(code::OK, json!({"ok": true, "changed": true, "path": p.display().to_string()}))
        }
        Err(e) => {
            st.progress_done();
            st.err(&e);
            fail(st, code::FAILED, &e)
        }
    }
}

fn adb_use(st: Style, path: Option<&str>) -> i32 {
    let Some(p) = path.filter(|p| !p.trim().is_empty()) else {
        st.err("`adb use` needs --path <file>, the adb executable to use");
        return fail(st, code::USAGE, "adb use needs --path");
    };
    let chosen = std::path::PathBuf::from(p);
    // Run before it is stored, not merely found. `locate` answers "is there a file there",
    // which is what the settings panel wants so it can show a broken choice. A command has
    // nowhere to show it: it either stores the path or refuses, and storing one that does
    // not run means the failure surfaces at the next thing that reaches for a headset
    // instead of here. Anything at all will pass an existence check - a text file did.
    match adb::locate(Some(&chosen)).filter(|f| f.version.is_some()) {
        Some(f) => {
            let mut s = config::Settings::load();
            s.adb_path = Some(chosen);
            s.save();
            st.ok(&format!("using {}", f.path.display()));
            st.field("version", f.version.as_deref().unwrap_or("").trim());
            st.emit(
                code::OK,
                json!({"ok": true, "path": f.path.display().to_string(), "version": f.version}),
            )
        }
        None => {
            st.err(&format!("{p} did not answer as adb"));
            st.info("it has to be the adb executable itself, and it has to run");
            fail(st, code::FAILED, "that is not an adb that runs")
        }
    }
}

fn adb_forget(st: Style) -> i32 {
    let mut s = config::Settings::load();
    s.adb_path = None;
    s.save();
    st.ok("choice cleared; adb is found automatically again");
    st.emit(code::OK, json!({"ok": true}))
}

fn revive_install(st: Style, yes: bool) -> i32 {
    if !cfg!(windows) {
        st.err("Revive is Windows only");
        return fail(st, code::FAILED, "Revive is Windows only");
    }
    st.heading("REVIVE");
    st.field("installer", &crate::engine::revive::installer_url());
    if !yes
        && !confirm(
            st,
            "This downloads Revive's own installer and runs it. It asks for administrator \
             rights and that prompt has to be answered on screen.",
        )
    {
        st.info("nothing was done");
        return st.emit(code::OK, json!({"ok": true, "changed": false, "declined": true}));
    }
    match crate::engine::revive::install(interrupted(), &mut |done, total| {
        st.progress(done, total.unwrap_or(0), None, None)
    }) {
        Ok(_) => {
            st.progress_done();
            let found = crate::engine::revive::locate(None);
            match &found {
                Some(f) => st.ok(&format!("installed at {}", f.dir.display())),
                // The installer runs detached and elevated, so its files can appear a moment
                // after it returns. Saying "not found yet" beats claiming failure.
                None => st.info("the installer ran; Revive was not visible yet"),
            }
            st.emit(
                code::OK,
                json!({"ok": true, "path": found.map(|f| f.dir.display().to_string())}),
            )
        }
        Err(e) => {
            st.progress_done();
            st.err(&e.to_string());
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

fn revive_use(st: Style, path: Option<&str>) -> i32 {
    let Some(p) = path.filter(|p| !p.trim().is_empty()) else {
        st.err("`revive use` needs --path <dir>, the folder holding ReviveInjector.exe");
        return fail(st, code::USAGE, "revive use needs --path");
    };
    let chosen = std::path::PathBuf::from(p);
    match crate::engine::revive::locate(Some(&chosen)) {
        Some(f) => {
            let mut s = config::Settings::load();
            s.revive_path = Some(chosen);
            s.save();
            st.ok(&format!("using {}", f.dir.display()));
            st.emit(code::OK, json!({"ok": true, "path": f.dir.display().to_string()}))
        }
        None => {
            st.err(&format!("{p} has no ReviveInjector.exe in it"));
            fail(st, code::FAILED, "no ReviveInjector.exe there")
        }
    }
}

fn revive_forget(st: Style) -> i32 {
    let mut s = config::Settings::load();
    s.revive_path = None;
    s.save();
    st.ok("choice cleared; Revive is found automatically again");
    st.emit(code::OK, json!({"ok": true}))
}

fn revive_setup(st: Style, path: Option<&str>, yes: bool) -> i32 {
    let root = match need_path(st, path) {
        Ok(r) => r,
        Err(c) => return c,
    };
    let settings = config::Settings::load();
    let Some(found) = crate::engine::revive::locate(settings.revive_path.as_deref()) else {
        st.err("Revive is not installed. Run `revive install` first.");
        return fail(st, code::FAILED, "Revive is not installed");
    };

    st.heading("REVIVE SETUP");
    st.field("revive", &found.dir.display().to_string());
    st.field("echo", &root.display().to_string());
    // Refused, not warned about. Everything this command produces is a reference to that
    // executable: a shortcut to it and a SteamVR entry naming it. With no game there, both
    // are broken by construction, and exiting zero would call that success. The window
    // blocks on the same condition, so this is also what keeps the two the same.
    if !install::exe_path(&root).is_file() {
        st.err("no echovr.exe in that folder, so there is nothing to point Revive at");
        st.info("point at a folder containing echovr.exe");
        return fail(st, code::FAILED, "no Echo VR install at that path");
    }

    let exe = install::exe_path(&root);

    // Same question the window asks on its Actions step, and the same answer either way.
    // Revive resolves its SteamVR entry against a Meta library, so an Echo the registry
    // does not place in one gets an entry that points at nothing. `patch_manifest` will
    // still write it, borrowing an id from another app, which is exactly what makes the
    // failure look like a success.
    let library = crate::engine::meta::library_id_for(&exe);
    match &library {
        Some(_) => st.ok("inside your Meta library, so the SteamVR entry will work"),
        None => {
            st.warn("the SteamVR entry will not work for this folder");
            st.info(
                "Revive resolves it against a Meta library and this Echo is not inside one. \
                 The desktop shortcut is unaffected.",
            );
        }
    }
    // The window puts a confirmation in the way here, so this asks for the same consent in
    // the way a command line can: --yes. Without it, nothing is written at all, because
    // writing only the half that works and calling it success is the behaviour being fixed.
    if library.is_none() && !yes {
        st.info("re-run with --yes to write it anyway, or use the shortcut on its own");
        return fail(st, code::FAILED, "would write a SteamVR entry that cannot launch");
    }

    let mut done = Vec::new();
    let mut failed = None;

    match crate::engine::revive::create_shortcut(&found.dir, &exe) {
        Ok(link) => {
            st.ok(&format!("shortcut at {}", link.display()));
            done.push(json!({"action": "shortcut", "path": link.display().to_string()}));
        }
        Err(e) => {
            st.err(&e.to_string());
            failed = Some(e.to_string());
        }
    }
    match crate::engine::revive::patch_manifest(&found.dir, &exe) {
        Ok(o) => {
            st.ok(&format!("SteamVR entry {o:?}").to_lowercase());
            done.push(json!({"action": "manifest", "outcome": format!("{o:?}")}));
        }
        Err(e) => {
            st.err(&e.to_string());
            failed = Some(e.to_string());
        }
    }

    match failed {
        None => st.emit(code::OK, json!({"ok": true, "done": done})),
        Some(e) => {
            // Told apart so a script, and the broker, can see the difference between "this
            // needs rights" and "this went wrong".
            let rights = e.to_lowercase().contains("access is denied")
                || e.to_lowercase().contains("permission denied");
            let which = if rights { code::ELEVATION } else { code::FAILED };
            st.emit(which, json!({"ok": false, "done": done, "error": e, "code": which}))
        }
    }
}

/// The licence patch: a Discord round trip, then a personalised DLL.
fn patch(st: Style, path: Option<&str>, from: Option<&str>, yes: bool) -> i32 {
    let root = match need_path(st, path) {
        Ok(r) => r,
        Err(c) => return c,
    };
    st.heading("LICENCE PATCH (PC)");
    st.field("target", &install::bin_dir(&root).display().to_string());
    st.info("this needs a Discord account in the patcher server, checked by the bot by name");
    st.plain(&format!("    {}", endpoints::DISCORD_PATCHER));
    if !install::exe_path(&root).is_file() {
        st.err("no echovr.exe there; the patch has nowhere to go");
        return fail(st, code::FAILED, "no Echo VR install at that path");
    }
    // A patch already on disk skips the whole Discord round trip. That is what an elevated
    // retry uses: the link is personal and expires, so asking for it twice is not a retry,
    // it is a second request the user has to sit through.
    if let Some(file) = from {
        let staged = std::path::PathBuf::from(file);
        if !staged.is_file() {
            st.err(&format!("{file} is not there"));
            return fail(st, code::FAILED, "the file to apply is not there");
        }
        return match crate::engine::pc_patch::apply(&staged, &root) {
            Ok(dest) => {
                st.ok(&format!("placed {}", dest.display()));
                st.emit(code::OK, json!({"ok": true, "path": dest.display().to_string()}))
            }
            Err(e) => {
                st.err(&e.to_string());
                let which =
                    if e.needs_elevation() { code::ELEVATION } else { code::FAILED };
                fail(st, which, &e.to_string())
            }
        };
    }

    if !yes
        && !confirm(
            st,
            "This opens Discord in a browser so you can authorise it, then downloads a copy \
             built for your account.",
        )
    {
        st.info("nothing was done");
        return st.emit(code::OK, json!({"ok": true, "changed": false, "declined": true}));
    }

    let cancel = interrupted();
    let url = match crate::engine::patch::obtain(
        crate::engine::patch::Kind::Dll,
        cancel,
        &mut |p| match p {
            crate::engine::patch::Progress::WaitingForBrowser => {
                st.info("a browser should have opened. Authorise there.")
            }
            crate::engine::patch::Progress::Generating => st.info("Discord is building it"),
        },
    ) {
        Ok(u) => u,
        // The one failure a terminal can recover from: no browser could be launched, so
        // print the address instead of stopping at "could not open a browser".
        Err(crate::engine::patch::Error::NoBrowser(url)) => {
            st.warn("no browser could be opened. Paste this into one:");
            st.plain(&url);
            return fail(st, code::FAILED, "no browser could be opened");
        }
        Err(e) => {
            st.err(&e.to_string());
            return fail(st, code::FAILED, &e.to_string());
        }
    };

    let staging = config::dir().join("staging");
    let staged = match crate::engine::pc_patch::stage(&url, &staging, cancel, &mut |s| {
        st.download(&s)
    }) {
        Ok(p) => p,
        Err(e) => {
            st.progress_done();
            st.err(&e.to_string());
            return fail(st, code::FAILED, &e.to_string());
        }
    };
    st.progress_done();

    match crate::engine::pc_patch::apply(&staged, &root) {
        Ok(dest) => {
            st.ok(&format!("placed {}", dest.display()));
            st.emit(code::OK, json!({"ok": true, "path": dest.display().to_string()}))
        }
        Err(e) => {
            st.err(&e.to_string());
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

fn quest_launch(st: Style, serial: Option<&str>) -> i32 {
    let (adb, device) = match open_device(st, serial) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let q = Quest::new(&adb, Some(&device));

    st.heading("LAUNCH ON HEADSET");
    st.field("headset", &device.model.clone().unwrap_or_else(|| device.serial.clone()));
    if q.installed_apk_path().is_none() {
        st.err("Echo VR is not installed on that headset");
        return fail(st, code::FAILED, "Echo VR is not installed");
    }
    match q.launch() {
        Ok(()) => {
            st.ok("started. Put the headset on.");
            st.emit(code::OK, json!({"ok": true, "launched": true}))
        }
        Err(e) => {
            st.err(&e.to_string());
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

fn logs(st: Style, out: Option<&str>, serial: Option<&str>) -> i32 {
    let (adb, device) = match open_device(st, serial) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let q = Quest::new(&adb, Some(&device));
    let dest = out.map(PathBuf::from).unwrap_or_else(config::logs_dir);

    st.heading("SUPPORT BUNDLE");
    match tools::collect_logs(&q, &dest, &mut |s| st.info(s)) {
        Ok(b) => {
            st.ok(&format!("{} files, {}", b.files, human_bytes(b.bytes)));
            // The path goes to stdout on its own line even under --quiet: a script that
            // runs this wants exactly one thing back. Under --json it is a field instead,
            // because stdout there carries the object and nothing else.
            if !st.json {
                println!("{}", b.path.display());
            }
            st.emit(
                code::OK,
                json!({
                    "ok": true,
                    "path": b.path.display().to_string(),
                    "files": b.files,
                    "bytes": b.bytes,
                }),
            )
        }
        Err(e) => {
            st.err(&format!("{e}"));
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

fn cache(st: Style, clear: bool) -> i32 {
    let staging = config::dir().join("staging");
    // Same two places the Tools screen looks. The install root is here because the PC
    // archive is downloaded into it, so a failed install leaves several gigabytes somewhere
    // this would otherwise never report.
    let settings = config::Settings::load();
    let root = settings.install_path.as_ref().map(std::path::PathBuf::from);
    let caches = tools::caches(&staging, root.as_deref());
    let report = tools::cache_report(&caches);

    st.heading("CACHED DOWNLOADS");
    st.field("staging", &staging.display().to_string());
    if let Some(r) = &root {
        // Named separately because only part of it is cache: the PC archive is downloaded
        // into the install folder, and nothing else in there is ever touched.
        st.field("install", &format!("{} (archive only)", r.display()));
    }
    let entries: Vec<_> = report
        .entries
        .iter()
        .map(|(p, n)| json!({
            "name": p.file_name().map(|n| n.to_string_lossy().into_owned()),
            "bytes": n,
        }))
        .collect();
    let folder = staging.display().to_string();
    if report.entries.is_empty() {
        st.info("nothing cached");
        return st.emit(
            code::OK,
            json!({"ok": true, "staging": folder,
            "install_root": root.as_ref().map(|r| r.display().to_string()), "entries": [], "total_bytes": 0, "cleared": false}),
        );
    }
    for (path, size) in &report.entries {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        st.plain(&format!("{:>10}  {name}", human_bytes(*size)));
    }
    st.field("total", &human_bytes(report.total));

    if !clear {
        st.info("run again with --clear to remove them");
        return st.emit(
            code::OK,
            json!({
                "ok": true,
                "staging": folder,
            "install_root": root.as_ref().map(|r| r.display().to_string()),
                "entries": entries,
                "total_bytes": report.total,
                "cleared": false,
            }),
        );
    }
    match tools::clear_cache(&caches) {
        Ok(freed) => {
            st.ok(&format!("{} freed", human_bytes(freed)));
            st.emit(
                code::OK,
                json!({
                    "ok": true,
                    "staging": folder,
            "install_root": root.as_ref().map(|r| r.display().to_string()),
                    "entries": entries,
                    "total_bytes": report.total,
                    "cleared": true,
                    "freed_bytes": freed,
                }),
            )
        }
        Err(e) => {
            st.err(&format!("{e}"));
            fail(st, code::FAILED, &e.to_string())
        }
    }
}

/// Asks before something long or destructive.
///
/// Answers no when there is no terminal to ask, rather than assuming consent: a script that
/// wants to go ahead unattended says so with `--yes`.
fn confirm(st: Style, what: &str) -> bool {
    if !st.tty {
        st.warn(&format!("{what} Re-run with --yes to proceed."));
        return false;
    }
    use std::io::Write;
    print!("  {what} Continue? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn long_options_take_a_value_either_way() {
        for line in ["update --path /opt/echo", "update --path=/opt/echo"] {
            let a = parse(&argv(line));
            assert_eq!(a.command, vec!["update"], "{line}");
            assert_eq!(a.path.as_deref(), Some("/opt/echo"), "{line}");
            assert!(a.error.is_none(), "{line}");
        }
    }

    #[test]
    fn short_options_cluster_and_take_attached_values() {
        let a = parse(&argv("-qy update -p/opt/echo"));
        assert_eq!(a.command, vec!["update"]);
        assert_eq!(a.path.as_deref(), Some("/opt/echo"));
        assert!(a.quiet && a.yes);

        let b = parse(&argv("-q -y -s ABC123 quest update"));
        assert_eq!(b.command, vec!["quest", "update"]);
        assert_eq!(b.serial.as_deref(), Some("ABC123"));
        assert!(b.quiet && b.yes);
    }

    #[test]
    fn modifiers_are_flags_not_bare_words() {
        // `cache clear` was the old shape. A modifier is an option now, and the leftover
        // word must not be silently swallowed as if it had worked.
        let a = parse(&argv("cache --clear"));
        assert!(a.clear);
        assert_eq!(a.command, vec!["cache"]);

        let b = parse(&argv("cache clear"));
        assert!(!b.clear);
        assert_eq!(b.command, vec!["cache", "clear"]);
    }

    #[test]
    fn no_command_at_all_prints_help_rather_than_panicking() {
        let a = parse(&argv(""));
        assert!(a.command.is_empty());
        // The dispatch used to slice this empty vector from index 1.
        assert_eq!(help_for(Style { json: false, colour: false, tty: false, quiet: true }, a.command.get(1..).unwrap_or(&[])), code::OK);
    }

    #[test]
    fn json_is_a_flag_like_any_other() {
        let a = parse(&argv("cache --json"));
        assert!(a.json);
        assert_eq!(a.command, vec!["cache"]);
        let b = parse(&argv("update --path=/x --json -q"));
        assert!(b.json && b.quiet);
        assert_eq!(b.path.as_deref(), Some("/x"));
    }

    #[test]
    fn a_failure_object_carries_the_same_three_keys_everywhere() {
        // A script should never have to learn a second failure shape.
        let st = Style { json: false, colour: false, tty: false, quiet: true };
        assert_eq!(fail(st, code::NO_DEVICE, "no headset detected"), code::NO_DEVICE);
        assert_eq!(fail(st, code::USAGE, "bad"), code::USAGE);
    }

    #[test]
    fn double_dash_ends_option_parsing() {
        let a = parse(&argv("status -- --path"));
        assert_eq!(a.command, vec!["status", "--path"]);
        assert!(a.path.is_none());
    }

    #[test]
    fn both_spellings_of_a_question_give_the_same_answer() {
        // `--version` and `version` are the same question. They were answering it
        // differently under --json: one an object, the other a line of prose.
        let flag = parse(&argv("--version --json"));
        let verb = parse(&argv("version --json"));
        assert!(flag.version && flag.json);
        assert!(verb.json);
        assert_eq!(verb.command, vec!["version"]);
    }

    #[test]
    fn asking_for_help_always_answers() {
        // --json and --quiet mean "spare me the commentary while you work". Asking a
        // question and getting silence is not that, and both flags used to do exactly it.
        for line in ["--help", "--json --help", "--quiet --help", "-q -h", "update --help"] {
            let a = parse(&argv(line));
            assert!(a.help, "{line} should be asking for help");
        }
    }

    #[test]
    fn the_old_mode_flag_is_tolerated_rather_than_rejected() {
        // It used to select this mode on the other binary. Old scripts and old habits still
        // type it, and an error for a flag that means "yes, this program" helps nobody.
        let a = parse(&argv("--cli status --path /x"));
        assert!(a.error.is_none(), "got {:?}", a.error);
        assert_eq!(a.command, vec!["status"]);
        assert_eq!(a.path.as_deref(), Some("/x"));
    }

    #[test]
    fn a_quoted_path_option_is_unquoted() {
        // What Explorer's "Copy as path" puts on the clipboard, pasted into a batch file
        // or a shell that does not strip quotes of its own.
        let a = parse(&["status", "--path", "\"C:\\Echo VR\""].map(String::from));
        assert_eq!(a.path.as_deref(), Some(r"C:\Echo VR"));
        let a = parse(&["status", "--out", "  C:\\logs  "].map(String::from));
        assert_eq!(a.out.as_deref(), Some(r"C:\logs"));
        assert!(parse(&argv("-c devices")).error.is_none());
    }

    #[test]
    fn help_and_version_are_flags_as_well_as_commands() {
        assert!(parse(&argv("-h")).help);
        assert!(parse(&argv("update --help")).help);
        assert!(parse(&argv("-V")).version);
        assert!(parse(&argv("--version")).version);
    }

    #[test]
    fn subcommands_survive_flags_in_between() {
        let a = parse(&argv("quest --serial ABC123 update"));
        assert_eq!(a.command, vec!["quest", "update"]);
        assert_eq!(a.serial.as_deref(), Some("ABC123"));
    }

    #[test]
    fn a_typo_is_reported_rather_than_read_as_a_command() {
        let a = parse(&argv("update --paht /opt/echo"));
        assert_eq!(a.error.as_deref(), Some("unknown option --paht"));
        let b = parse(&argv("update -Z"));
        assert_eq!(b.error.as_deref(), Some("unknown option -Z"));
    }

    #[test]
    fn an_option_missing_its_value_is_an_error_not_a_silent_none() {
        let a = parse(&argv("update --path"));
        assert_eq!(a.error.as_deref(), Some("--path needs a value"));
    }

    #[test]
    fn every_documented_option_exists_in_the_options_table() {
        // A command listing a flag that no longer exists would silently document nothing.
        let st = Style { json: false, colour: false, tty: false, quiet: false };
        for c in COMMANDS {
            for flag in c.opts {
                assert!(opt_line(st, flag).is_some(), "{}: no such option {flag}", c.name);
            }
        }
    }

    #[test]
    fn every_command_in_the_help_is_one_that_can_be_run() {
        // One direction only, and strictly: anything with a help page must be reachable,
        // or the help promises something that does not exist.
        //
        // Not the other way round. `adb forget` and `revive forget` are dispatched without
        // pages of their own because they are one line each, explained in the entry for the
        // command they undo. A page each would be filler.
        let dispatchable = [
            "status", "update", "install", "patch", "deps",
            "self-update check", "self-update apply",
            "quest status", "quest update", "quest install", "quest launch",
            "adb install", "adb use", "adb forget",
            "revive install", "revive setup", "revive use", "revive forget",
            "devices", "logs", "cache",
        ];
        for c in COMMANDS {
            assert!(dispatchable.contains(&c.name), "{} is documented but not dispatched", c.name);
        }
    }

    #[test]
    fn every_flow_that_writes_where_it_may_not_offers_the_broker() {
        // The broker existed and two flows did not use it, so they told people to relaunch
        // the app themselves - the exact dead end it was built to remove. Checked against
        // the source, because "has an elevation path" is structural, not textual.
        for (flow, what) in [
            ("pc_install.rs", "installing into Program Files"),
            ("pc_update.rs", "updating an install there"),
            ("pc_patch.rs", "placing the patch beside the game"),
            ("revive.rs", "writing Revive's manifest"),
        ] {
            let src = match flow {
                "pc_install.rs" => include_str!("../flows/pc_install.rs"),
                "pc_update.rs" => include_str!("../flows/pc_update.rs"),
                "pc_patch.rs" => include_str!("../flows/pc_patch.rs"),
                _ => include_str!("../flows/revive.rs"),
            };
            assert!(
                src.contains("elevated::Elevated") && src.contains("Run as administrator"),
                "{flow} can fail on rights when {what}, and offers no way through it"
            );
        }
    }

    #[test]
    fn both_clients_remember_where_they_installed() {
        // Not cosmetic: the folder is what the cache cleaner searches for a left-behind
        // 4.68 GB archive. The window recorded it and the command line did not, so an
        // install started from a script left gigabytes nothing could find.
        let cli = include_str!("mod.rs");
        for window in [
            include_str!("../flows/pc_install.rs"),
            include_str!("../flows/pc_update.rs"),
        ] {
            assert!(window.contains("remember_install_path"), "the window stopped recording it");
        }
        // Counted in the code only, not in this file's tests: any spelling searched for
        // also appears in the search itself, and a test that counts itself passes for the
        // wrong reason.
        let code = cli.split("#[cfg(test)]").next().unwrap_or("");
        assert_eq!(
            code.matches("remember_install_path").count(),
            2,
            "install and update both have to record it"
        );
    }

    #[test]
    fn warnings_the_window_gives_reach_the_command_line_too() {
        // The parity test below covers commands. It did not cover *messages*, and two
        // prerequisites - deleting Meta's copy, and needing the patcher Discord - shipped in
        // the window only. Someone scripting an install was told neither.
        //
        // Checked against the source rather than by rendering, because what matters is that
        // both clients reach for the same fact, not that they word it identically.
        let cli = include_str!("mod.rs");
        let window = include_str!("../flows/pc_install.rs");
        for (fact, what) in [
            ("expected_echo_dir", "which Meta folder to delete"),
            ("DISCORD_PATCHER", "the Discord server the patch bot checks"),
        ] {
            assert!(window.contains(fact), "the window stopped mentioning {what}");
            assert!(cli.contains(fact), "the command line does not mention {what}");
        }
    }

    #[test]
    fn the_window_and_the_command_line_can_do_the_same_things() {
        // The two clients are meant to be equals. Each entry here is something the window
        // offers; if one has no command, the command line is not a substitute for it.
        for (in_the_window, command) in [
            ("Install Echo VR (PC)", "install"),
            ("Update Echo VR (PC)", "update"),
            ("Install Echo VR (Quest)", "quest install"),
            ("Launch on headset", "quest launch"),
            ("Update Echo VR (Quest)", "quest update"),
            ("Licence patch (PC)", "patch"),
            ("Revive setup", "revive setup"),
            ("Dependencies: install adb", "adb install"),
            ("Dependencies: choose adb", "adb use"),
            ("Dependencies: install Revive", "revive install"),
            ("Dependencies: choose Revive", "revive use"),
            ("Dependencies: what is set up", "deps"),
            ("Update this installer", "self-update check"),
            ("Tools: collect logs", "logs"),
            ("Tools: cached downloads", "cache"),
        ] {
            assert!(
                find_command(command).is_some(),
                "the window can do {in_the_window} and `{command}` is not documented"
            );
        }
    }

    #[test]
    fn help_resolves_the_longest_name_not_the_first_word() {
        let st = Style { json: false, colour: false, tty: false, quiet: true };
        assert_eq!(help_for(st, &argv("quest update")), code::OK);
        assert_eq!(help_for(st, &argv("quest")), code::OK);
        assert_eq!(help_for(st, &[]), code::OK);
        assert_eq!(help_for(st, &argv("frobnicate")), code::USAGE);
        // Verbs without a page of their own are still verbs: asking about one is not a
        // usage error, and `version --help` used to exit 2.
        assert_eq!(help_for(st, &argv("version")), code::OK);
        assert_eq!(help_for(st, &argv("help")), code::OK);
    }

    #[test]
    fn every_command_documents_a_usage_line_and_a_description() {
        for c in COMMANDS {
            assert!(c.usage.starts_with(c.name), "{}: usage should lead with the name", c.name);
            assert!(!c.detail.is_empty(), "{} has no description", c.name);
            assert!(!c.examples.is_empty(), "{} has no example", c.name);
        }
    }

    #[test]
    fn a_path_that_is_missing_is_a_usage_error_not_a_panic() {
        let st = Style { json: false, colour: false, tty: false, quiet: true };
        assert_eq!(need_path(st, None).unwrap_err(), code::USAGE);
        assert_eq!(need_path(st, Some("")).unwrap_err(), code::USAGE);
        assert!(need_path(st, Some("/tmp")).is_ok());
    }
}
