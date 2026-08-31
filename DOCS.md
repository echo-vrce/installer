# Working on the Echo VRCE Installer

Everything someone who has never seen this project needs in order to change it.

This is not the user guide. Nothing here explains how to install Echo VR; it explains how
the program that does it is built, what it talks to, and which decisions are load-bearing.

## Contents

- [What this is](#what-this-is)
- [Getting set up](#getting-set-up)
- [How the code is laid out](#how-the-code-is-laid-out)
- [The shapes you have to know](#the-shapes-you-have-to-know)
- [Rules that are not up for grabs](#rules-that-are-not-up-for-grabs)
- [Decisions worth knowing](#decisions-worth-knowing)
- [What the live service actually does](#what-the-live-service-actually-does)
- [The other installers](#the-other-installers)
- [Testing](#testing)
- [Known gaps](#known-gaps)

---

## What this is

Echo VR was a VR game by Ready At Dawn, published by Meta. Meta shut the servers down.
**EchoVRCE** is the community that kept it playable: community servers, a patched client,
and the infrastructure to distribute both.

This program installs that client. On a PC it downloads a 4.68 GB archive from community
mirrors, unpacks it, and applies an update described by a manifest. On a Quest it sideloads
an APK and its game data over `adb`. It can generate a per-account licence patch for people
who never owned the game, set up Revive so a SteamVR headset can launch it, and collect a
support bundle when something goes wrong.

It is a **rewrite of [marshmallow-mia's original installer](https://github.com/marshmallow-mia/Echo-VR-Installer)**,
which is Java and Swing. The rewrite is Rust with a native UI. It talks to the same
infrastructure: the same manifests, the same mirrors, the same Discord bot. **That
infrastructure is not ours.** If it ever went away, the installer would work again the
moment someone stood up a replacement, and standing one up is a far larger job than the
installer itself: mirrors for a 4.68 GB payload, a manifest kept in step with the client,
and a bot that mints a patch per account. Worth knowing which part of the system you are
actually holding.

Some behaviour differs from the original on purpose. Where it does, the reasoning is in
[Decisions worth knowing](#decisions-worth-knowing) or
[What the live service actually does](#what-the-live-service-actually-does), because a
difference nobody wrote down looks like a bug to the next person.

---

## Getting set up

Rust stable. `rust-toolchain.toml` pins the channel and both targets, so `rustup` sets
itself up on the first build.

```sh
cargo run                    # native build, for iterating on the UI
cargo build --release
cargo test                   # 278 tests, none needs a display, a VM or a headset
cargo clippy --all-targets
```

The program builds and runs on Linux. That is not a supported target for users: it exists
so the UI can be worked on without a virtual machine in the loop. Anything Windows-only
(the registry, elevation, Revive) is behind `cfg(windows)` with a stub that returns
"nothing found" elsewhere.

### Cross-compiling to Windows

```sh
cargo install cargo-xwin
rustup component add llvm-tools
cargo xwin build --release --target x86_64-pc-windows-msvc
```

`cargo-xwin` downloads the MSVC CRT and the Windows SDK itself. It needs two tools on
`PATH` that `llvm-tools` ships under different names. Both pick their behaviour from
`argv[0]`, so symlinks are enough:

| Name it looks for | Symlink to | Needed by |
| --- | --- | --- |
| `lld-link` | `rust-lld` | the linker, standing in for MSVC's `link.exe` |
| `llvm-lib` | `llvm-ar` | `cc-rs`, to archive the C in `ring` |

No system packages are required for any of this.

### The two binaries

The build produces `echo-vrce-installer.exe` (the window) and `echo-vrce-cli.exe` (the same
code without one). They are not two programs. See
[Two binaries](#two-binaries-because-a-windows-subsystem-is-a-link-time-decision) for why
there have to be two files.

### Development conveniences

`--at <screen>` opens straight onto a screen instead of clicking through: `home`,
`install`, `update`, `qinstall`, `qupdate`, `patch`, `revive`, `deps`, `tools`, `about`.

`ECHO_VRCE_HOME` moves settings, logs and the managed `adb` somewhere else. Point it at a
scratch folder and a development run never touches a real profile. Pointed at a folder
beside the executable it also makes the whole thing portable, which is a shipped feature
rather than only a testing one.

### `.cargo/config.toml` is load-bearing

```toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "target-feature=+crt-static"]
```

Without it the executable imports `VCRUNTIME140.dll`, which is not part of Windows: it
comes with the Visual C++ Redistributable. On a clean install it is simply absent, and
because the window is a GUI subsystem binary **the failure is completely silent**. No
window, no error, no log. Verified on a fresh Windows 10 LTSC, where the app did not start
at all and left no trace. Do not remove this to save a few megabytes.

---

## How the code is laid out

Three layers, and the boundary between them is the thing to preserve:

```
src/engine/     no UI, no globals, no printing. Pure operations over paths and HTTP.
src/flows/      one file per wizard. Owns its steps' content, never the window chrome.
src/cli/        the same engine driven from a terminal.
src/app.rs      the window shell: navigation, the step column, Back and Continue.
```

**`engine/` never knows which front end called it.** Progress is reported through a callback
or a channel, never printed. This is why the same code backs the window, the command line
and the elevated child process, and why the tests need no display.

| File | What lives there |
| --- | --- |
| `src/theme.rs` | Colour tokens, spacing unit, type scale, and the egui style they drive |
| `src/icons.rs` | The eight icons, drawn as vector strokes |
| `src/logo.rs` | The disc mark, rasterised parametrically, and the wordmark lockups |
| `src/mark.rs` | The disc geometry, shared with `build.rs` for the executable icon |
| `src/widgets.rs` | Shared widget vocabulary: fields, status lines, progress, checklists, log pane |
| `src/main.rs` | Entry point: the panic hook and the log go up before anything that can fail |
| `src/app.rs` | Window shell and navigation |
| `src/fmt.rs` | Turning byte counts and durations into things people read |
| `src/channel.rs` | The drain loop every flow uses to read its worker's progress |
| `src/flows/` | One wizard per file |
| `src/flows/elevated.rs` | Following an elevated run by tailing the log file it writes |
| `src/dependencies.rs` | The dependency panel: a settings screen, not a wizard |
| `src/tools_screen.rs` | Support bundles and the download cache: likewise a settings screen |
| `src/endpoints.rs` | Every URL the app talks to, in one place |
| `src/config.rs` | Settings, and where app data lives |
| `src/log.rs` | The log that outlives the window, and the ring the flows show |
| `src/os.rs` | Process-wide OS behaviour that must be set before anything else runs |
| `src/cli/mod.rs` | Argument parsing, commands, help, exit codes |
| `src/cli/style.rs` | How the CLI looks: colour, glyphs, the progress bar |
| `src/cli/events.rs` | The NDJSON progress stream the elevated child writes |
| `src/bin/echo-vrce-cli.rs` | Twenty lines, so the exit codes survive |
| `src/engine/manifest.rs` | Manifest grammar and the path validation that guards `rm -rf` |
| `src/engine/download.rs` | Ranged, resumable, verified, cancellable downloads |
| `src/engine/hash.rs` | Streaming SHA-256, including the resume case |
| `src/engine/unzip.rs` | Extraction, with the path guard the original lacks |
| `src/engine/install.rs` | Reading an install: where it is, what is in it, whether it is real |
| `src/engine/update.rs` | The update planner: what to fetch, what to remove |
| `src/engine/pc_install.rs` | The full PC install, including the destructive delete |
| `src/engine/pc_patch.rs` | Placing the licence patch beside `echovr.exe` |
| `src/engine/patch.rs` | The Discord authorisation, with the CSRF check the original lacks |
| `src/engine/adb.rs` | Locating and driving adb, always through an argv |
| `src/engine/watch.rs` | Background device polling, with tolerance for a flaky cable |
| `src/engine/quest*.rs` | Sideloading, the install marker, and the update path |
| `src/engine/revive.rs` | Editing Revive's vrmanifest without disturbing the rest of it |
| `src/engine/meta.rs` | Finding an Echo VR the Meta client installed, and saying how |
| `src/engine/elevate.rs` | Starting an elevated copy, waiting on it, quoting its arguments |
| `src/engine/tools.rs` | Pulling logs off the headset, zipping them, sizing the cache |
| `src/engine/path_input.rs` | Cleaning up a path a person typed or pasted |
| `src/engine/testserver.rs` | A local HTTP server the download tests drive |
| `build.rs` | Hand-written Windows resources: icon and VERSIONINFO |

---

## The shapes you have to know

### The update manifest

Plain text, one entry per line:

```text
add <path> <sha256>
del <path>
```

Paths are relative to the install root and are **validated before anything touches the
filesystem** (`engine/manifest.rs`). That validation is what stands between a hostile or
corrupt manifest and a recursive delete outside the install. Treat it as a security
boundary, not a tidiness check.

Live manifests:

```text
https://files.echovr.de/updates/update.manifest
https://files.echovr.de/updates/quest/update.manifest
```

The Quest one carries `# BASE_APK:` and `# Target:` headers that the version gate depends
on.

**The manifest is edited by hand, under pressure.** When Meta shipped a PC update that
broke Echo, the fix was appended to the manifest within hours: one entry separated by a
single space where every other line uses two, and the file's own date comment still naming
the previous day. So split on arbitrary whitespace, and never trust a date in a header. A
parser that is fussy about spacing, or that believes the header, breaks precisely on the day
it is needed most.

That episode is also the argument for the update flow's priority: it is how a fix reaches
players at all.

### The install layout

```text
<root>/ready-at-dawn-echo-arena/bin/win10/echovr.exe
```

**`<root>` is the folder that contains `ready-at-dawn-echo-arena`, not the game folder.**
Everything is built from the root. This trips up users constantly, which is why the folder
box resolves any path naming part of an install back to its root
([see below](#the-folder-box-reads-the-answer-not-the-letters)).

### The install marker, which is an interop standard

Written at the manifest target root as `.echo_installer_version`:

```text
version=1
base_apk=        base_sha256=      installed_sha256=
patched=         installed_at=     installer_version=
```

**All three Echo installers write this same format.** If this one deviates, its update flow
refuses installs made by the other two and theirs refuse ours. The format is fixed. Do not
add keys casually and never change the meaning of an existing one.

### Registry keys, on Windows

```text
HKLM\SOFTWARE\WOW6432Node\Oculus VR, LLC\Oculus        value: Base
HKCU\Software\Oculus VR, LLC\Oculus\Libraries          DefaultLibrary, and per-library OriginalPath
```

The client's folder has been renamed twice (`Oculus`, then `Meta`, now `Meta Horizon`) and
Meta's own help pages still document the middle one. **The registry keys have not changed
once.** That is why detection reads the registry rather than trying a list of paths, and it
also covers a base moved elsewhere during setup, which no list of guesses can.

### Mirrors

Three, all serving byte-identical payloads and all supporting range requests:

```text
https://files.echovr.de/
https://evr.echo.taxi/
https://mia.cdn.echo.taxi/
```

---

## Rules that are not up for grabs

These are the ones that shaped the whole program. Breaking one is a design change, not a
refactor.

**Nothing is auto-detected into a decision.** The app never fills a field on your behalf,
never picks a path, and never advances a step on its own. It does say what it sees next to
whatever you typed, and it never blocks on that: a red cross beside a path is information,
not a veto. This is the single rule the project was started for.

**Warnings inform, confirmations gate.** Detection never blocks. What sits alongside it is
a confirmation at the moment of committing, and only there. It appears when all three hold:
pressing the button actually does something, the result is expensive or hard to undo, and a
warning is live right now. Putting the consequence in both the page and the dialog would
train people to dismiss the dialog, which is the failure mode confirmations exist to
prevent.

**Every download can be stopped, and stopping is not failing.** Cancel in every flow and in
the dependency panel, Ctrl+C in the command line. A stopped download keeps what it
downloaded and says so. The command line exits `130`, the shell's own convention, so a
script can tell "someone stopped it" from "it broke".

**The command line and the window do the same things.** Anything one can do, the other can.
Where the window shows a confirmation dialog, the command line requires `--yes` and refuses
without it. There are tests whose only job is to hold this.

**Everything the command line prints is plain ASCII.** Box drawing and block characters look
right in a modern terminal and turn to rubbish in an old console, a wrong code page, or a
raster font. This runs on whatever Windows someone already has.

**A disabled primary button always states its reason.** A dead button with no explanation is
the most common sin in installers.

**Icons are drawn, not typeset.** Inter has no U+26A0 (warning) or U+24D8 (circled i), and
Windows Arial has no U+2713 (check). The original installer hit exactly that and its own
comments record the icons rendering as empty boxes. Vector shapes have no font dependency,
recolour per state, and stay crisp at every UI scale.

**One accent colour, two tokens.** `#2563EB` fills the primary button (white on it clears
5:1); `#5B9BFF` is the accent as *text* on near-black. A single blue cannot do both without
failing contrast somewhere. Green is reserved for validation results, never for navigation.

---

## Decisions worth knowing

### Installing over an existing copy deletes it first

Unpacking on top is a merge: whatever the old install had and the archive does not is left
behind. That means reinstalling cannot repair a broken copy, which is the main reason
anyone reinstalls, and it leaves Meta's own files sitting there.

So the game folder is removed and rebuilt. Two boundaries make that safe rather than
alarming: it is always confirmed first, in a dialog naming the exact folder and saying
plainly that nothing outside it is touched; and the removal is **scoped to
`<root>/ready-at-dawn-echo-arena`, never the root someone chose**. That distinction is the
difference between replacing an install and emptying a games drive, and there is a test
whose only job is to hold it.

The delete happens **after** the archive is downloaded and verified, not before. A failed
download must not leave someone with nothing.

### Installing is not finished until the manifest is applied

Caught on a real headset: a Quest install left the base build in place with none of the
manifest's `asset_patches`, because the install flow stopped once it had written the version
marker. The PC side had always chained into the update; the Quest side had not.

A fresh install that is already behind the current manifest is worse than an obvious
failure. The game is there, it is subtly wrong, and nothing says so.

The marker is still written **before** the update, deliberately. If the update then fails,
the marker left behind is accurate, so the standalone update flow can identify the install
and pick up cleanly rather than refusing it.

### Two binaries, because a Windows subsystem is a link-time decision

`echo-vrce-installer.exe` is a GUI subsystem binary: double-clicking it must not flash a
console. `echo-vrce-cli.exe` is a console one. They share every line of code; the second is
twenty lines handing their arguments to the first's entry point.

The reason is measured, not theoretical. **PowerShell does not record an exit code for a GUI
subsystem process** (`$LASTEXITCODE` comes back empty), so the documented exit codes were
invisible to precisely the scripts they exist for. A subsystem is a field in the PE header,
chosen at link time, so no runtime cleverness fixes it inside one file. Node and VS Code
ship two executables on Windows for the same reason.

The window's binary has no command line mode. It had one behind `--cli` and it was removed:
two ways in that behave differently under a shell is worse than one that works.

### An elevated run reports like an ordinary one

Some steps write where administrator rights are needed. Rather than a purpose-built helper
or a service, the window re-runs **`echo-vrce-cli` itself**, elevated, through
`ShellExecuteExW`, and follows it by tailing a log file it names.

The child emits one JSON object per line into that file, and the window decodes those into
the state its ordinary widgets already read: the stage list ticks, the bar moves, the counts
are real. Prose lines stay alongside for whoever reads the file afterwards. The two are told
apart by whether the line starts with a brace, and there is a test that a sentence can never
decode as progress. Stage names are matched by name, pinned by a test against the engine
that sends them; an unrecognised stage still reaches the log rather than vanishing.

The two executables therefore belong in the same folder, and the app says so if they are
not.

### Text stays inside the window, and breaks where a reader would

egui offers two wrapping modes and neither is right alone. Word wrapping leaves a path
hanging off the edge, because a path is one unbreakable token. Breaking anywhere fixes that
and ruins prose: it will split `SteamVR` across two lines.

So the choice is made per token, in the order a reader minds least: spaces first; then,
inside a token too long to fit, after a separator (`\ / - _ .`) so a path breaks between its
parts; and only then mid-token, for a hash that offers nothing else. The rule is a pure
function with the font taken out of it, because the **order** is the contract.

Two things had to be fixed underneath it, and both will bite again: a `Frame` grows to fit
its contents, so cards need a maximum width as well as a minimum; and **text inside a
horizontal layout does not wrap at all**, because egui hands those children unbounded width.

### The folder box reads the answer, not the letters

The field asks for a folder and people give the folder the game is in, which is the sensible
reading of the question and the wrong answer to it. Pasting the `win10` folder used to be
told "no echovr.exe here" while the user was looking straight at it.

Any path naming part of an install now resolves to its root: the arena folder, `bin`,
`win10`. It climbs **at most three levels**, the exact depth of an install, so a wrong path
cannot resolve to some unrelated install several folders away. A path with no install under
or above it is left exactly as typed, because guessing there would move someone's chosen
folder somewhere they never asked for.

Pasted paths are also cleaned: Explorer's "Copy as path" wraps them in double quotes, and a
quoted path is not a path. Quotes and stray spaces are stripped in one place
(`engine/path_input.rs`) so all six screens that take a path behave the same.

### The folder box suggests, and says why

Precedence: the folder used last time, then whatever the Meta client says it installed to,
then a neutral guess. A decision the user already made beats a deduction; a deduction beats
a guess.

Whatever is offered carries a line saying where it came from. **That line is the difference
between informing and deciding**: a prefilled box whose reasoning is invisible is the app
choosing for someone. It disappears the moment they type, because by then it describes a
path that is no longer there.

### The Meta library id comes from Meta, not from Revive

Revive launches a title by library id plus a path relative to it. The original installer
copies that id out of some other app's entry in Revive's manifest, which only works once
Revive has already seen a library, hence its advice to install any free title and start
SteamVR first. On a fresh machine it simply cannot proceed.

The id is a GUID the Meta client assigns to each install location and records under
`HKCU\Software\Oculus VR, LLC\Oculus\Libraries`, per user because libraries are per account.
Reading it there means a first-time setup works with an empty Revive manifest, which is the
normal state. Copying from an existing entry stays as a fallback.

Two drives means two libraries, so the one whose folder contains the install is chosen,
longest match first, falling back to `DefaultLibrary`. Match on **path components, not
string prefixes**: `C:\Meta` is not the parent of `C:\MetaOther`, and comparing as text says
it is. There is a test for exactly that.

### A limitation worth knowing about Revive

The vrmanifest entry launches Echo through the Oculus runtime, so its path is resolved
against the library it names. The consequence is that **the SteamVR entry only works for an
Echo inside a Meta library**. An install anywhere else, including one this app made at
`C:\EchoVR`, gets an entry pointing at a path that does not exist.

`patch_manifest` will still write it, borrowing an id from another app, which is what makes
the failure look like a success. So the Actions step states which case you are in either
way, and confirms before writing an entry that cannot launch. The desktop shortcut is
unaffected: it launches the injector directly with the real path.

### Three things about Revive that cost time to find out

**The artwork pack is gone.** `files.echovr.de/stuff/patches/ready-at-dawn-echo-arena_assets.zip`
is 404 on all three mirrors, and so is the whole `stuff/` tree. The original installer still
offers "fix game artwork" with the box **ticked by default**, and its download throws on the
404, which takes the entire Revive chain down with it. This port does not offer the action
and says why, rather than quietly dropping it. Worth re-checking before building on it.

**Revive is on 3.2.0**, and the original pins 3.1.1 in a string. Here the installer URL comes
from the releases API with the pinned one only as a fallback, so it cannot go stale in
silence.

**Running Revive's own installer needs no broker.** It requests elevation in its own
manifest, so a plain spawn fails with **error 740**. `ShellExecuteW` with the `runas` verb is
the whole answer: Windows shows the prompt and this process stays unprivileged. The broker
exists only for writes *this* app performs into Program Files.

### Windows' own dialogs are suppressed at startup

`src/os.rs` calls `SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX)` as the
first thing the process does.

The app is built around the user choosing paths by hand, so it has to expect to be pointed
at the wrong file. Probing one is a `CreateProcess`, and when that fails on something that
is not a valid executable, Windows puts up a hard-error box owned by this process. The
window behind it stops responding, and the only screen that could correct the setting is the
one now unreachable. A wrong choice in a file picker locked the app.

The behaviour is inherited rather than chosen: launched from PowerShell it never appeared,
because PowerShell had already turned it off, and launched by double-click it always did.
`SEM_NOGPFAULTERRORBOX` is deliberately **not** set: a crash of our own should stay visible.

### Probing an external program is always bounded

`Adb::version()` is the identity probe: it is run against whatever file the user picked, at
startup and on every re-check. It has a timeout, and that is not the whole story.

Point it at a `.bat` ending in `start "" something.exe` and the batch file exits
immediately, so **there is nothing left to time out**, but the detached grandchild inherited
the output pipe and holds it open. Reading to end-of-stream then blocks forever. The reader
threads therefore report through a channel with a short grace period after the child exits,
rather than being joined. Two regression tests cover both shapes.

---

## What the live service actually does

Measured against the real service, not read off the Java. Anything here that changes on the
server side will change what this program must do.

### The Discord licence patch

`POST https://files.echovr.de/api/exchange` with `{"code": ..., "type": "dll"|"apk"}`.

**The callback listener must not take the first connection.** Browsers speculatively open
sockets; Chrome preconnects as soon as the address bar resolves a host, and that socket
often carries no bytes. The original accepts one connection and parses it as the callback.
This is a live intermittent failure in the shipping Java, observed here: the first probe run
died on exactly it. Loop until a connection carries `code` or `error`, answer everything
else with a 404, and set a read timeout on accepted sockets, because an accepted socket does
not inherit the listener's non-blocking flag.

**Read the error body.** Not-a-member is `403` with `error`, `message` and `invite`. The
server supplies the invite, so a changed invite is followed automatically instead of being
hardcoded. Note that `ureq` treats a non-2xx as `Err` and discards the body by default, so
`http_status_as_error(false)` is mandatory on that call.

**The patch URL is signed and expires in 24 hours.** `is` and `ex` are hex unix timestamps
exactly 24 h apart and `hm` is an HMAC. A tampered or expired signature returns **404**,
indistinguishable by status from a URL that never existed, which is why that case is called
out by name. URL validation must not require the string to end in `.dll`, since a query
string follows it. Reuse the downloaded **file** on retry, never the URL.

**The port and the path are not ours to change.** Port `53124` and path `/callback` are
registered against the Discord client ID in the developer portal. Anything that "tidies up"
either of them breaks the flow for everyone.

**Generation takes about nine seconds**, because the bot builds a personalised DLL per
request. That is why the API has a `409 busy` state at all: generation is serialised. The
exchange call needs its own generous timeout, separate from the callback wait.

**There is no CSRF `state` in the original flow.** While the listener is open, any web page
could redirect to `http://127.0.0.1:<port>/callback?code=<attacker code>` and the installer
would download and apply someone else's patch. This port generates a random `state`, passes
it through, and rejects a callback whose `state` does not match. Six lines.

### Zip extraction is a security boundary

`UnzipFile.java` joins the destination with the entry name and writes there with no
validation, so an archive naming `../../x` writes outside the destination. Reachable rather
than theoretical: the licence patch flow accepts a URL, so the archive is not necessarily
one of Echo's.

Two things learned writing the replacement:

- The `zip` crate's `enclosed_name` **skips** a leading `/` or `C:` rather than refusing it.
  Containment still holds, but an archive naming `/etc/passwd` is quietly relocated and
  nobody is told. This port refuses absolute names itself so the error means what a reader
  expects.
- `enclosed_name` alone does not stop the two-step symlink attack: extract a symlink
  `dir -> /etc`, then extract `dir/passwd` through it. Symlink entries are refused outright;
  a game archive has no use for them.

Writing tests for this: `ZipWriter` normalises a leading slash away but happily preserves
`..`, so an absolute-name archive has to be forged by patching bytes.

### The mirror speed test in the original wastes 60 MB

`randomDownloadTestFile` is exactly 30 MiB and the original fetches **all of it from both
servers before every download**. On a slow line that is minutes of "Preparing Download..."
before a byte of the real payload moves. This port samples a small range from each instead;
all three mirrors support ranges.

Mirror selection never refuses to proceed. If none of them answers the speed test it still
tries the first, because a missing probe file says nothing about the payload. What it does
is say so, and name what each server replied.

### Echo's logs on a Quest

Written against a connected Quest 2 rather than transcribed from the Java, which turned out
to matter. The original pulls from two locations and **only one exists**:

| Path | On a real Quest 2 |
| --- | --- |
| `/sdcard/r14logs` | Exists. One file per session |
| `/sdcard/Android/data/com.readyatdawn.r15/files/_local/r14logs` | Does not exist |

So the second `adb pull` in the original always fails, silently, and always has. Two more
files are worth having and it takes neither: `assetpatch.log`, which is what proves an
update actually took effect rather than the installer believing it did, and the install
marker.

Log filenames contain brackets and parentheses, so the directory is pulled as a directory.
Anything that globs those names will mangle them.

---

## The other installers

Two others are recommended alongside this one. Both are **Android apps that run on the Quest
itself**, which is the whole reason they exist: they sidestep adb and the PC. They are not
alternatives for the PC flows, which have none.

- `heisthecat31/EchoVR-Installer`: native Android, Java. Patches the APK on-device.
- `Crafter-1/Quest-EchoVR-Installer`: Unity, C#. Same on-device approach.

Both fetch the same Quest manifest this one does, and all three write the same
[install marker](#the-install-marker-which-is-an-interop-standard).

**Worth adopting from the Unity one:** it writes a *pending* marker into its own storage
before installing and only promotes it once the install is confirmed, so an interrupted
install cannot leave a marker describing an install that never finished. It also records the
APK's `versionCode` and cross-checks it after install, which nothing else does.

**Worth avoiding from the Android one:** its entry parser requires three tokens per line, so
a `del path` line with no hash is silently dropped; the first deletion the Quest manifest
ever ships will not reach its users. It also reads a base URL out of the manifest **body**
and downloads from whatever it says, so a manifest could redirect downloads to another host;
this port derives the base from the manifest URL instead.

Two rules taken from a mistake found there, where a hardcoded RSA signing key sits in the
source (on a dead code path, confirmed by asking, but committed all the same): **never ship
a signing key**, because dead code does not unpublish it and git history keeps it; and do
not take on APK repacking without a real answer for where the signing identity lives. That
last one is the reason the "Better Graphics" and texture mods are not implemented here.

---

## Testing

**278 unit and integration tests**, none of which needs a display, a virtual machine or a
headset. Downloads are tested against a local HTTP server (`engine/testserver.rs`) that can
be told to drop connections mid-transfer, so resume and retry are exercised rather than
assumed.

The tests worth knowing about, because they encode rules rather than behaviour:

- the destructive delete stays inside `ready-at-dawn-echo-arena`
- a prose log line can never decode as a progress event
- the CLI help table and the command dispatch cannot drift apart
- the CLI and the window offer the same operations
- library matching compares path components, not string prefixes
- the adb probe returns even when a detached grandchild holds its pipe open

### What tests cannot cover

Some behaviour only exists on Windows and has to be exercised there: elevation, the
registry, file locking, and the ways `CreateProcess` fails. A disposable Windows install is
the practical way to do it, since several of those checks are destructive by nature.

**Anything behind UAC cannot be automated.** The secure desktop does not accept injected
input, by design. Installing Revive and the licence patch flow have to be walked by a
person, every time.

### Wine and Proton

The renderer is glow (OpenGL) rather than wgpu specifically so the window would survive
there, and that is not a guess any more: **the Quest install flow has been run end to end
under Proton**, with the headset attached to the Linux machine. The UI drew, `adb` found the
device, and the sideload and update completed.

What that covers is the parts with no Windows in them: the window, the downloads, and
talking to a headset over USB. It says nothing about elevation, the registry reads that
locate a Meta library, or Revive, all of which lean on Windows behaviour that Wine
implements to varying degrees. Treat the renderer choice as confirmed and the rest as
untested.

---

## Known gaps

- **Installing Revive itself** and the **licence patch for a non-owner** have been walked by
  hand but are not covered by any automated test, for the UAC reason above.
- **CI does not gate on style.** Clippy runs on every push but deliberately without
  `-D warnings`: there are style warnings outstanding, and a job that is red from the first
  push is a job people learn to ignore. Clearing them and then tightening it is a job of its
  own.
- **Mods are not implemented.** See [The other installers](#the-other-installers) for why:
  they need APK repacking, and repacking needs a signing identity.
- **Tested narrowly.** A handful of machines, x86-64 throughout: Windows 10 and 11, plus
  the one Proton run above.
