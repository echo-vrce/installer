// SPDX-License-Identifier: GPL-3.0-or-later
//! Cleaning up a path a person typed or pasted.

/// Strip the noise Windows itself adds when someone copies a path.
///
/// Explorer's "Copy as path" (and PowerShell's, and the address bar's) puts the path on
/// the clipboard **wrapped in double quotes**, because that is what a command line needs.
/// Pasted into a text field it is not a path any more, and the app would reject a path the
/// user can read and that is entirely correct, with no hint as to why. Stray spaces around
/// a path come from the same place: selecting the text by hand rather than with a menu.
///
/// What this deliberately does not do is rewrite the path: no slash conversion, no
/// canonicalising, no guessing at a parent or a child. It removes what is provably not
/// part of the path and leaves the rest exactly as typed - the app should not be making
/// decisions about where the user meant to point.
pub fn clean(input: &str) -> &str {
    let trimmed = input.trim();
    // Only a matched pair. A lone quote is more likely to be a half-finished edit than a
    // wrapper, and eating it would fight the user while they type.
    let unquoted = match (trimmed.strip_prefix('"'), trimmed.ends_with('"')) {
        (Some(rest), true) if trimmed.len() >= 2 => rest.strip_suffix('"').unwrap_or(rest),
        _ => trimmed,
    };
    unquoted.trim()
}

/// Same rule, applied in place, for the text a widget owns.
pub fn clean_in_place(value: &mut String) {
    let cleaned = clean(value);
    if cleaned.len() != value.len() {
        *value = cleaned.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_an_ordinary_path_alone() {
        let p = r"C:\Program Files\Meta Horizon\Software\Software";
        assert_eq!(clean(p), p);
    }

    #[test]
    fn strips_what_explorer_adds() {
        assert_eq!(
            clean(r#""C:\Program Files\Meta Horizon\Software\Software""#),
            r"C:\Program Files\Meta Horizon\Software\Software"
        );
        assert_eq!(clean("  C:\\Echo  "), r"C:\Echo");
        assert_eq!(clean(r#"  "C:\Echo"  "#), r"C:\Echo");
    }

    #[test]
    fn keeps_a_lone_quote() {
        // Mid-edit, not a wrapper. Removing it would delete a character the user just typed.
        assert_eq!(clean(r#""C:\Echo"#), r#""C:\Echo"#);
        assert_eq!(clean(r#"C:\Echo""#), r#"C:\Echo""#);
    }

    #[test]
    fn does_not_rewrite_the_path_itself() {
        // Forward slashes are valid on Windows and are not ours to normalise; a trailing
        // separator is meaningful to some people and harmless to the rest.
        assert_eq!(clean("C:/Echo/"), "C:/Echo/");
        assert_eq!(clean(r"\\host\share\echo"), r"\\host\share\echo");
    }

    #[test]
    fn empty_stays_empty() {
        assert_eq!(clean(""), "");
        assert_eq!(clean("   "), "");
        assert_eq!(clean("\"\""), "");
    }

    #[test]
    fn in_place_only_touches_what_changed() {
        let mut s = String::from(r"C:\Echo");
        clean_in_place(&mut s);
        assert_eq!(s, r"C:\Echo");
        let mut s = String::from(r#" "C:\Echo" "#);
        clean_in_place(&mut s);
        assert_eq!(s, r"C:\Echo");
    }
}
