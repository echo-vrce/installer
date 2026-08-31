// SPDX-License-Identifier: GPL-3.0-or-later
//! Generates the Windows resources - executable icon and VERSIONINFO - at compile time.
//!
//! Normally this is `llvm-rc` or `windres` compiling an `.rc` script. Neither is available
//! in this toolchain, and pulling one in would mean a second compiler outside the work
//! directory, so the `.res` file is emitted here directly. The format is well specified and
//! small: a sequence of resource headers, each followed by its payload padded to a DWORD.
//! `lld-link` accepts a `.res` as an input and converts it, so no separate cvtres step.
//!
//! The icon comes from `src/mark.rs`, included rather than copied, so the executable icon,
//! the window icon and the mark drawn inside the app are all the same geometry.

use std::io::Write;
use std::path::PathBuf;

include!("src/mark.rs");

/// Must match `theme::ACCENT_TEXT`. Not shared, because `theme.rs` needs egui and this runs
/// before any dependency is built - `logo::icon_colour_matches_the_build_script` in the
/// crate asserts the two stay equal.
const ICON_RGB: (u8, u8, u8) = (0x5B, 0x9B, 0xFF);

/// The sizes Windows actually asks for: small icons in lists, 32 on the desktop, 48 in the
/// taskbar, 256 for the extra-large view. Each is rasterised, not scaled.
const SIZES: [u32; 6] = [16, 24, 32, 48, 128, 256];

// Resource types, from winuser.h.
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
/// en-US. The strings below are English, so claiming anything else would be a lie a
/// property sheet would repeat.
const LANG_EN_US: u16 = 0x0409;
/// UTF-16, which is the only sane choice for the string table.
const CODEPAGE_UNICODE: u16 = 0x04b0;

fn main() {
    println!("cargo:rerun-if-changed=src/mark.rs");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app.res");
    let mut res = Vec::new();

    // A .res starts with a null header, which is just an entry with ordinal type and name
    // both zero and no data.
    entry(&mut res, 0, 0, 0, &[]);

    let icons: Vec<(u32, Vec<u8>)> = SIZES.iter().map(|&s| (s, icon_dib(s))).collect();
    for (i, (_, data)) in icons.iter().enumerate() {
        entry(&mut res, RT_ICON, i as u16 + 1, 0x1010, data);
    }
    entry(&mut res, RT_GROUP_ICON, 1, 0x1030, &group_icon(&icons));
    entry(&mut res, RT_VERSION, 1, 0x0030, &version_info());

    std::fs::File::create(&out).unwrap().write_all(&res).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// One RESOURCEHEADER plus its payload. Only ordinal types and names are needed here, which
/// keeps every header exactly 32 bytes.
fn entry(out: &mut Vec<u8>, type_id: u16, name_id: u16, memory_flags: u16, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    out.extend_from_slice(&0xFFFFu16.to_le_bytes()); // type is an ordinal, not a string
    out.extend_from_slice(&type_id.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes()); // name likewise
    out.extend_from_slice(&name_id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&memory_flags.to_le_bytes());
    out.extend_from_slice(&LANG_EN_US.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // Version
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    out.extend_from_slice(data);
    pad4(out);
}

fn pad4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// One icon image as a DIB, which is what an RT_ICON resource holds - a bare
/// BITMAPINFOHEADER and pixels, with no BITMAPFILEHEADER.
///
/// Two things about this format catch people out: the height in the header is doubled,
/// because it covers the colour bitmap and an AND mask stacked, and the rows run bottom-up.
fn icon_dib(side: u32) -> Vec<u8> {
    let alpha = coverage(side, if side <= 32 { &SMALL } else { &DISPLAY });
    let (r, g, b) = ICON_RGB;

    let mask_stride = (side.div_ceil(32) * 4) as usize;
    let mut out = Vec::with_capacity(40 + (side * side * 4) as usize + mask_stride * side as usize);

    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(side as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((side * 2) as i32).to_le_bytes()); // biHeight, colour + mask
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression, BI_RGB
    out.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage, may be 0 for BI_RGB
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // BGRA, bottom-up, straight alpha. Not premultiplied: the shell composites icon
    // bitmaps itself and expects unmultiplied channels, so premultiplying here would drag
    // the antialiased edge toward black instead of fading it out.
    for y in (0..side).rev() {
        for x in 0..side {
            let a = alpha[(y * side + x) as usize];
            out.extend_from_slice(&[b, g, r, a]);
        }
    }

    // The AND mask is legacy, and ignored where the alpha channel is honoured. All zero
    // means "opaque everywhere", which is the correct fallback for a 32-bit icon.
    out.extend(std::iter::repeat_n(0u8, mask_stride * side as usize));
    out
}

/// The GRPICONDIR that ties the RT_ICON entries together. This, not the images, is what
/// the icon resource id actually points at.
fn group_icon(icons: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(icons.len() as u16).to_le_bytes());
    for (i, (side, data)) in icons.iter().enumerate() {
        // 256 does not fit in a byte and is encoded as zero, which is the documented
        // convention rather than a trick.
        let dim = if *side >= 256 { 0u8 } else { *side as u8 };
        out.push(dim); // width
        out.push(dim); // height
        out.push(0); // colours in palette: 0 for true colour
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(i as u16 + 1).to_le_bytes()); // matching RT_ICON id
    }
    out
}

/// VS_VERSIONINFO: what the file's Properties > Details tab reads.
fn version_info() -> Vec<u8> {
    let v = env!("CARGO_PKG_VERSION");
    let mut parts = v.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );

    let mut fixed = Vec::new();
    fixed.extend_from_slice(&0xFEEF04BDu32.to_le_bytes()); // signature
    fixed.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // struct version 1.0
    // Version is stored as two DWORDs of packed WORDs, most significant pair first.
    // The fourth component. Windows version resources are four numbers; this project uses
    // three, so the build number is fixed at zero rather than left to mean something.
    const BUILD: u32 = 0;
    let ms = ((major as u32) << 16) | minor as u32;
    let ls = ((patch as u32) << 16) | BUILD;
    fixed.extend_from_slice(&ms.to_le_bytes()); // file version
    fixed.extend_from_slice(&ls.to_le_bytes());
    fixed.extend_from_slice(&ms.to_le_bytes()); // product version, same thing here
    fixed.extend_from_slice(&ls.to_le_bytes());
    fixed.extend_from_slice(&0x3Fu32.to_le_bytes()); // flags mask
    fixed.extend_from_slice(&0u32.to_le_bytes()); // flags: not a debug or prerelease build
    fixed.extend_from_slice(&0x0004_0004u32.to_le_bytes()); // VOS_NT_WINDOWS32
    fixed.extend_from_slice(&1u32.to_le_bytes()); // VFT_APP
    fixed.extend_from_slice(&0u32.to_le_bytes()); // subtype
    fixed.extend_from_slice(&0u32.to_le_bytes()); // date, unused
    fixed.extend_from_slice(&0u32.to_le_bytes());

    let version_text = format!("{major}.{minor}.{patch}.0");
    let strings: Vec<(&str, &str)> = vec![
        ("CompanyName", "EchoVRCE community"),
        ("FileDescription", "Echo VRCE Installer"),
        ("FileVersion", &version_text),
        ("InternalName", "echo-vrce-installer"),
        // No copyright claim over the game or its assets. This covers the installer only.
        ("LegalCopyright", "Community project. No affiliation with Meta or Ready At Dawn."),
        ("OriginalFilename", "echo-vrce-installer.exe"),
        ("ProductName", "Echo VRCE Installer"),
        ("ProductVersion", &version_text),
    ];

    let table_key = format!("{LANG_EN_US:04x}{CODEPAGE_UNICODE:04x}");
    let mut table_children = Vec::new();
    for (k, val) in &strings {
        table_children.extend(node(k, Value::Text(val)));
    }
    let table = node(&table_key, Value::Children(&table_children));
    let string_file_info = node("StringFileInfo", Value::Children(&table));

    let mut translation = Vec::new();
    translation.extend_from_slice(&LANG_EN_US.to_le_bytes());
    translation.extend_from_slice(&CODEPAGE_UNICODE.to_le_bytes());
    let var = node("Translation", Value::Binary(&translation));
    let var_file_info = node("VarFileInfo", Value::Children(&var));

    let mut children = string_file_info;
    children.extend(var_file_info);
    node("VS_VERSION_INFO", Value::FixedThenChildren(&fixed, &children))
}

enum Value<'a> {
    Text(&'a str),
    Binary(&'a [u8]),
    Children(&'a [u8]),
    FixedThenChildren(&'a [u8], &'a [u8]),
}

/// One node of the version tree. Every node is the same shape, which is why this is one
/// function: length, value length, a type flag, a UTF-16 key, then the value and any
/// children, each aligned to a DWORD.
///
/// The catch is `wValueLength`: for text it counts UTF-16 code units including the
/// terminator, and for binary it counts bytes.
fn node(key: &str, value: Value<'_>) -> Vec<u8> {
    let mut body = Vec::new();
    let (value_len, is_text) = match value {
        Value::Text(s) => {
            let utf16: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
            for u in &utf16 {
                body.extend_from_slice(&u.to_le_bytes());
            }
            (utf16.len() as u16, 1u16)
        }
        Value::Binary(b) => {
            body.extend_from_slice(b);
            (b.len() as u16, 0)
        }
        Value::Children(c) => {
            body.extend_from_slice(c);
            (0, 1)
        }
        Value::FixedThenChildren(fixed, c) => {
            body.extend_from_slice(fixed);
            pad4(&mut body);
            body.extend_from_slice(c);
            (fixed.len() as u16, 0)
        }
    };

    let mut head = Vec::new();
    head.extend_from_slice(&0u16.to_le_bytes()); // length, filled in below
    head.extend_from_slice(&value_len.to_le_bytes());
    head.extend_from_slice(&is_text.to_le_bytes());
    for u in key.encode_utf16().chain(std::iter::once(0)) {
        head.extend_from_slice(&u.to_le_bytes());
    }
    pad4(&mut head);

    let total = head.len() + body.len();
    head[0..2].copy_from_slice(&(total as u16).to_le_bytes());
    head.extend_from_slice(&body);
    // Trailing padding belongs to the parent's accounting, not this node's length.
    head
}
