//! Human-readable filenames only. Never use these strings as filesystem paths,
//! clipboard payloads, cache/model keys, or JSON/MCP data.

use std::borrow::Cow;
use std::path::Path;

/// Escape a filename component (or a filename-derived label) at its display
/// boundary. Ordinary Unicode is deliberately unchanged, including combining
/// marks, ZWJ/ZWNJ and emoji. This is not general confusable detection: existing
/// lossy OS-name representations cannot be made unique by this formatter.
pub fn name(text: &str) -> Cow<'_, str> {
    escape(text, true)
}

/// Escape an already-lossy native path string without normalizing its spelling.
/// On Windows, every backslash in a path is structural, not a filename character.
/// In contrast, `name` always escapes backslashes in component/label text.
/// Preserving separators means an inserted escape can look like a separator
/// followed by ordinary component text (for example, `\u{202e}`). Full displayed
/// Windows paths are therefore not reversible or guaranteed distinct; retain
/// raw paths for identity. `name` still distinguishes literal component escapes.
pub fn path_text(text: &str) -> Cow<'_, str> {
    escape(text, !cfg!(windows))
}

pub fn path(path: &Path) -> Cow<'_, str> {
    match path.to_string_lossy() {
        Cow::Borrowed(text) => path_text(text),
        Cow::Owned(text) => Cow::Owned(match path_text(&text) {
            Cow::Borrowed(_) => text,
            Cow::Owned(escaped) => escaped,
        }),
    }
}

fn needs_escape(c: char, backslashes: bool) -> bool {
    (backslashes && c == '\\')
        || matches!(c, '\0'..='\u{1f}' | '\u{7f}'..='\u{9f}'
            | '\u{2028}' | '\u{2029}'
            // Unicode Bidi_Control, not all format/default-ignorable characters.
            | '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}')
}

fn escape(text: &str, backslashes: bool) -> Cow<'_, str> {
    let Some((first, _)) = text.char_indices().find(|&(_, c)| needs_escape(c, backslashes)) else {
        return Cow::Borrowed(text);
    };
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..first]);
    for c in text[first..].chars() {
        match c {
            '\\' if backslashes => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0'..='\u{1f}' | '\u{7f}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c if needs_escape(c, backslashes) => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_and_paths_are_borrowed_and_unchanged() {
        for text in [
            "",
            "-leading.txt",
            "a b.txt",
            "résumé-日本語",
            "e\u{301}",
            "👩\u{200d}💻",
            "a\u{200c}b",
        ] {
            assert!(matches!(name(text), Cow::Borrowed(s) if s == text));
            assert!(matches!(path(Path::new(text)), Cow::Borrowed(s) if s == text));
        }
        for text in ["/", "./plain//child/", "../-child", "C:/plain/child"] {
            assert!(matches!(path_text(text), Cow::Borrowed(s) if s == text));
        }
    }

    #[test]
    fn controls_have_visible_unambiguous_encodings() {
        for c in (0..=0x9f).filter_map(char::from_u32).filter(|c| c.is_control()) {
            let expected = match c {
                '\n' => "\\n".into(),
                '\r' => "\\r".into(),
                '\t' => "\\t".into(),
                c if c <= '\u{7f}' => format!("\\x{:02x}", c as u32),
                c => format!("\\u{{{:04x}}}", c as u32),
            };
            assert_eq!(name(&format!("a{c}b")), format!("a{expected}b"));
            assert_ne!(name(&format!("a{c}b")), name(&format!("a{expected}b")));
        }
        assert_eq!(name("a\nb.txt"), "a\\nb.txt");
        assert_eq!(name("a\\nb.txt"), "a\\\\nb.txt");
        assert_ne!(name("a\nb.txt"), name("ab.txt"));
    }

    #[test]
    fn unicode_policy_is_explicit_and_not_general_normalization() {
        for c in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{2028}', '\u{2029}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            assert_eq!(name(&format!("a{c}b")), format!("a\\u{{{:04x}}}b", c as u32));
        }
        for c in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}', '\u{fe0f}'] {
            let text = format!("a{c}b");
            assert!(matches!(name(&text), Cow::Borrowed(s) if s == text));
        }
        // Existing lossy aliases are intentionally not made falsely unique.
        assert_eq!(name("bad\\xff.txt"), "bad\\\\xff.txt");
        assert_eq!(name("bad�.txt"), "bad�.txt");
    }

    #[test]
    fn path_escaping_preserves_structure_and_does_not_normalize() {
        assert_eq!(path(Path::new("/root\n/child\t.txt")), "/root\\n/child\\t.txt");
        assert_eq!(path_text("./root//child\r/"), "./root//child\\r/");
        // Exercise the Windows structural policy even on non-Windows hosts.
        assert_eq!(escape("C:\\root\\child\n.txt", false), "C:\\root\\child\\n.txt");
        assert_eq!(
            escape("\\\\server\\share\\child\u{202e}", false),
            "\\\\server\\share\\child\\u{202e}"
        );
        assert_eq!(escape("\\\\?\\C:\\root\\child\t", false), "\\\\?\\C:\\root\\child\\t");
    }

    #[test]
    fn windows_structural_separators_can_alias_inserted_component_escapes() {
        let bidi_path = "C:\\root\\a\u{202e}b";
        let separate_component_path = "C:\\root\\a\\u{202e}b";
        assert_ne!(bidi_path, separate_component_path);
        // Windows path rendering preserves separators. It is not a reversible
        // full-path encoding: the inserted escape has this known visual alias.
        assert_eq!(escape(bidi_path, false), escape(separate_component_path, false));
        assert_eq!(escape(bidi_path, false), "C:\\root\\a\\u{202e}b");
        // Component encoding still distinguishes a control from literal text
        // containing a backslash. It must not be used on whole Windows paths.
        assert_eq!(name("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(name("a\\u{202e}b"), "a\\\\u{202e}b");
        assert_ne!(name("a\u{202e}b"), name("a\\u{202e}b"));
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_paths_keep_separators_and_prefixes() {
        for text in ["C:\\root\\child", "\\\\server\\share\\child", "\\\\?\\C:\\root\\child"] {
            assert!(matches!(path(Path::new(text)), Cow::Borrowed(s) if s == text));
        }
        assert_eq!(
            path(Path::new("C:\\root\n\\child\u{202e}")),
            "C:\\root\\n\\child\\u{202e}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_backslashes_are_filename_characters() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(path(Path::new("/root\\n/child")), "/root\\\\n/child");
        assert_eq!(path(Path::new(OsStr::from_bytes(b"/bad\xff\n"))), "/bad�\\n");
    }
}
