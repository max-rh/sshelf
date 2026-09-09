//! One sanitizer for everything the plain CLI prints.
//!
//! Host names, users, hostnames, tags and site names are untrusted text: they come out of
//! `hosts.toml`, `~/.ssh/config` or a tailnet, and any of those can be crafted. The TUI is
//! safe by construction — ratatui filters control characters before it writes a cell — but the
//! plain CLI path has no such filter. `println!` hands the bytes straight to the terminal,
//! where one escape sequence can move the cursor, repaint the screen, or forge the lines that
//! follow a `sshelf list`. Every human-readable host- or site-derived field on stdout/stderr
//! therefore goes through [`sanitize`] first.
//!
//! Machine-readable output is deliberately left alone: `sshelf list --json` and every other
//! serde path already escape control characters, and a script needs the value exactly as it is
//! stored.
//!
//! The two boundaries sshelf owns refuse these characters outright ([`has_control`]): the
//! add/edit forms reject the field, and the importers drop a host whose name carries one and
//! fall back for a site name that does, so no record sshelf writes itself carries one. Loading
//! never refuses: a name already on disk is sanitized on display, not rejected, so a database
//! written by an older build still opens.

/// What every rejected code point collapses to. One replacement per character, so the columns
/// of the plain `sshelf list` table still line up.
const REPLACEMENT: char = '\u{FFFD}';

/// The inline message the host and site forms show for a field that carries one of these.
pub const CONTROL_REJECTED: &str = "control characters are not allowed";

/// A copy of `text` with every character [`has_control`] objects to replaced by U+FFFD.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if is_forbidden(c) { REPLACEMENT } else { c })
        .collect()
}

/// True when `text` carries a character the terminal must never be shown verbatim.
pub fn has_control(text: &str) -> bool {
    text.chars().any(is_forbidden)
}

/// One terminal-safe line for an error on its way to stderr.
///
/// An `anyhow` chain quotes untrusted text: a `toml` parse error prints a caret diagram that
/// repeats the offending line of `hosts.toml` verbatim, escape sequences and all. Folding
/// matters as much as sanitizing — a bare newline is enough to forge the line after the error
/// — so the message is squashed onto one line first and everything left is sanitized.
///
/// `toml`'s diagram lines are the ones containing `|`; dropping them keeps the two lines that
/// matter (*where* and *what*) and leaves an ordinary single-line error untouched.
pub fn error_line(text: &str) -> String {
    let folded = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains('|'))
        .collect::<Vec<_>>()
        .join(" — ");
    sanitize(&folded)
}

/// The rejected set, in one place so display and validation can't drift apart.
fn is_forbidden(c: char) -> bool {
    // `char::is_control` is C0 (0x00..=0x1F), DEL (0x7F) and the C1 block (U+0080..=U+009F) —
    // the last of these matters because a lone U+009B is a CSI introducer on some terminals.
    // Tab and newline are in there too, and stay in: neither has any business in a host field,
    // and either one alone is enough to fake a row of `sshelf list`.
    c.is_control()
        // Bidirectional overrides and isolates don't move the cursor; they reorder what is
        // already on the line, which is enough to make one hostname read as another.
        || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_terminal_control_becomes_a_replacement_character() {
        // An SGR escape, a bare C1 CSI introducer, a C1 NEL, and a right-to-left override.
        let hostile = "web\u{1b}[31m\u{9b}2J\u{85}\u{202e}moc.live";
        let clean = sanitize(hostile);
        assert!(!clean.contains('\u{1b}'), "{clean:?}");
        assert!(!clean.contains('\u{9b}'), "{clean:?}");
        assert!(!clean.contains('\u{85}'), "{clean:?}");
        assert!(!clean.contains('\u{202e}'), "{clean:?}");
        assert_eq!(clean.matches(REPLACEMENT).count(), 4);
        // The printable text around them survives, so the row is still readable.
        assert!(clean.starts_with("web"), "{clean:?}");
        assert!(clean.ends_with("moc.live"), "{clean:?}");
    }

    #[test]
    fn tabs_and_newlines_are_rejected_too() {
        assert_eq!(
            sanitize("a\tb\nc\rd\u{7f}"),
            "a\u{fffd}b\u{fffd}c\u{fffd}d\u{fffd}"
        );
        assert!(has_control("two\nlines"));
        assert!(has_control("\u{200f}"));
        assert!(!has_control("perfectly ordinary"));
    }

    #[test]
    fn an_error_chain_is_folded_onto_one_sanitized_line() {
        // What `store::load_hosts` hands back for a crafted `hosts.toml`: the caret diagram
        // quotes the offending source line, escape sequence and all.
        let chain = "parsing /tmp/hosts.toml: TOML parse error at line 3, column 25\n  |\n3 | \
                     name = \"web\u{1b}[2Jspoofed\" oops\n  |                         ^\nexpected \
                     newline after \u{9b}2J\n";
        let line = error_line(chain);
        assert!(!line.contains('\u{1b}'), "{line:?}");
        assert!(!line.contains('\u{9b}'), "{line:?}");
        assert!(!line.contains('\n'), "{line:?}");
        // The diagram — the half that quotes the crafted source line — is folded away, and the
        // escape in the reason that survives the fold is neutralised.
        assert_eq!(
            line,
            "parsing /tmp/hosts.toml: TOML parse error at line 3, column 25 — expected newline \
             after \u{fffd}2J"
        );
    }

    #[test]
    fn an_ordinary_error_survives_error_line_intact() {
        assert_eq!(
            error_line("Permission denied (os error 13)"),
            "Permission denied (os error 13)"
        );
        assert_eq!(error_line("  padded  \n\n"), "padded");
        assert_eq!(error_line(""), "");
    }

    #[test]
    fn ordinary_text_is_returned_unchanged() {
        for text in [
            "prod-web",
            "deploy@10.0.0.1:22",
            "bücher-server", // non-ASCII letters
            "серверная",     // a non-Latin script
            "büro 🖥️ nas",   // an emoji with a variation selector
            "site · with · middots",
            "",
        ] {
            assert_eq!(sanitize(text), text, "{text:?} must survive untouched");
            assert!(!has_control(text), "{text:?}");
        }
    }
}
