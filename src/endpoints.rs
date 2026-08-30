// SPDX-License-Identifier: GPL-3.0-or-later
//! Every URL the app talks to, in one place.
//!
//! Recorded here rather than inline so that a change of hosting is a change to one file,
//! and so the set is auditable at a glance. See `docs/findings.md` for what was measured
//! about each of these.

/// Manifest driving incremental PC updates. Entry paths resolve against its parent.
pub const PC_MANIFEST: &str = "https://files.echovr.de/updates/update.manifest";

/// Manifest driving incremental Quest updates. Also names the base APK to install.
pub const QUEST_MANIFEST: &str = "https://files.echovr.de/updates/quest/update.manifest";

/// Payload mirrors, for the large files that are not manifest entries: the PC client
/// archive and the Quest APK and data. All three serve byte-identical payloads and all
/// three support ranges, so any of them can be resumed against.
///
/// Note that manifest *entries* are not fetched from here: they resolve against the
/// manifest's own location, so mirror selection does not apply to an update run.
pub const MIRRORS: &[&str] = &[
    "https://files.echovr.de/",
    "https://evr.echo.taxi/",
    "https://mia.cdn.echo.taxi/",
];

/// The full PC client, served from the mirrors above. Not a manifest entry: it is the
/// 4.68 GiB starting point that an update is then applied on top of.
pub const PC_ARCHIVE: &str = "ready-at-dawn-echo-arena.zip";

/// Its size, for telling someone what they are about to download. Only ever advisory: what
/// decides whether there is room is `pc_install::require_space`, which uses the real
/// announced length rather than this.
pub const PC_ARCHIVE_BYTES: u64 = 5_024_528_313;

/// Small file used to time the mirrors. A ranged sample of it is enough; the original
/// installer fetches all 30 MiB of it from every mirror before every download.
pub const MIRROR_PROBE: &str = "randomDownloadTestFile";

/// Where a patch link comes from. The port reads `error`, `message` and `invite` out of
/// the response rather than hardcoding them.
pub const PATCH_EXCHANGE: &str = "https://files.echovr.de/api/exchange";

pub const DISCORD_LOUNGE: &str = "https://discord.com/invite/echo-vr-lounge";

/// The original installer this one is a rewrite of. Linked from About so the credit is
/// something a reader can follow, rather than a name they have to search for.
pub const REPO_ORIGINAL: &str = "https://github.com/marshmallow-mia/Echo-VR-Installer";

/// The licence this program is under, linked from About because a notice nobody can read
/// in full is only half a notice.
pub const LICENCE: &str = "https://www.gnu.org/licenses/gpl-3.0.html";
/// Membership of this server, not the lounge, is what the patch bot checks.
pub const DISCORD_PATCHER: &str = "https://discord.gg/bMpsva6fmA";
