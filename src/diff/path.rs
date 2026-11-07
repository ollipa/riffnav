//! File names as git writes them, as riffnav holds them, and as they're shown.
//!
//! git C-quotes a name holding anything but plain ASCII, so a diff's headers need
//! [`unquote`]. A path is then held as text, the real name when that's UTF-8. A
//! byte that isn't, as in a Latin-1 name, becomes a private-use character of its
//! own, so the name stays distinct and [`to_os`] can give git and the editor the
//! bytes back. A name can hold anything, so what reaches the screen goes through
//! [`shown`].

use std::borrow::Cow;
use std::ffi::OsString;

/// Where the characters standing for bytes that aren't UTF-8 start, in the last
/// private-use plane. Only bytes from 0x80 up are ever invalid.
const RAW_BYTES: u32 = 0x10_FE00;

/// The name at the start of `s`, if it opens with a quoted one, and what follows
/// the closing quote. `None` for anything git wouldn't have written, which is then
/// a name that happens to start with a quote.
pub fn unquote(s: &str) -> Option<(String, &str)> {
    let mut bytes = Vec::new();
    let rest = s.strip_prefix('"')?;
    let mut at = rest.bytes().enumerate();
    while let Some((i, byte)) = at.next() {
        match byte {
            b'"' => return Some((from_bytes(&bytes), &rest[i + 1..])),
            b'\\' => {
                let (_, escape) = at.next()?;
                bytes.push(match escape {
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 0x0b,
                    b'f' => 0x0c,
                    b'r' => b'\r',
                    b'"' | b'\\' => escape,
                    b'0'..=b'3' => {
                        let (_, second) = at.next()?;
                        let (_, third) = at.next()?;
                        let digits = [escape, second, third];
                        if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                            return None;
                        }
                        digits.iter().fold(0, |value, d| value * 8 + (d - b'0'))
                    }
                    _ => return None,
                });
            }
            _ => bytes.push(byte),
        }
    }
    None
}

/// A name's bytes as a path: its text, with any byte that isn't UTF-8 kept.
pub fn from_bytes(bytes: &[u8]) -> String {
    let mut path = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        path.push_str(chunk.valid());
        let raw = chunk.invalid().iter();
        path.extend(raw.filter_map(|&byte| char::from_u32(RAW_BYTES + u32::from(byte))));
    }
    path
}

/// The byte a character from [`from_bytes`] stands for.
fn raw_byte(c: char) -> Option<u8> {
    let byte = u32::from(c).checked_sub(RAW_BYTES)?;
    u8::try_from(byte).ok().filter(|byte| *byte >= 0x80)
}

/// `path` as the operating system knows it, for handing to git or an editor.
pub fn to_os(path: &str) -> OsString {
    if !path.chars().any(|c| raw_byte(c).is_some()) {
        return path.into();
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut buffer = [0; 4];
    for c in path.chars() {
        match raw_byte(c) {
            Some(byte) => bytes.push(byte),
            None => bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes()),
        }
    }
    os_string(bytes)
}

#[cfg(unix)]
fn os_string(bytes: Vec<u8>) -> OsString {
    std::os::unix::ffi::OsStringExt::from_vec(bytes)
}

/// Elsewhere a name isn't bytes, so one that isn't UTF-8 can't be asked for.
#[cfg(not(unix))]
fn os_string(bytes: Vec<u8>) -> OsString {
    String::from_utf8_lossy(&bytes).into_owned().into()
}

/// Whether a terminal would act on `c` rather than print it: control characters,
/// the line separators, and what reorders the text around it.
fn unsafe_on_screen(c: char) -> bool {
    let reorders = matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}');
    let reorders = reorders || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    c.is_control() || reorders || matches!(c, '\u{2028}' | '\u{2029}')
}

/// `text`, which holds a path, safe to print: what a terminal would act on and
/// bytes that aren't UTF-8 are written out as git would quote them. The result is
/// for reading. It's no longer the name, so nothing should look a file up by it.
pub fn shown(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|c| unsafe_on_screen(c) || raw_byte(c).is_some())
    {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut buffer = [0; 4];
    for c in text.chars() {
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if unsafe_on_screen(c) => {
                for byte in c.encode_utf8(&mut buffer).bytes() {
                    out.push_str(&format!("\\{byte:03o}"));
                }
            }
            _ => match raw_byte(c) {
                Some(byte) => out.push_str(&format!("\\{byte:03o}")),
                None => out.push(c),
            },
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_every_escape_git_writes() {
        let (name, rest) = unquote(r#""a\a\b\t\n\v\f\r\"\\\033z" tail"#).unwrap();
        assert_eq!(name, "a\x07\x08\t\n\x0b\x0c\r\"\\\x1bz");
        assert_eq!(rest, " tail");
    }

    #[test]
    fn octal_bytes_join_up_into_utf8() {
        let (name, _) = unquote(r#""a/caf\303\251.txt""#).unwrap();
        assert_eq!(name, "a/café.txt");
        let (name, _) = unquote(r#""\360\237\246\200""#).unwrap();
        assert_eq!(name, "🦀");
    }

    #[test]
    fn what_git_would_not_write_is_not_quoted() {
        for s in [
            "plain",
            r#""no end"#,
            r#""bad \q""#,
            r#""\400""#,
            r#""\18""#,
            r#""\1"#,
            r#"""#,
            r#""\"#,
        ] {
            assert_eq!(unquote(s), None, "{s}");
        }
    }

    #[test]
    fn a_name_that_is_not_utf8_survives_the_round_trip() {
        let bytes = b"caf\xe9/\xff\xfe.txt";
        let path = from_bytes(bytes);
        assert_eq!(to_os(&path).as_encoded_bytes(), bytes);
        assert_eq!(shown(&path), r"caf\351/\377\376.txt");
        // Two names told apart only by the bad byte stay two names.
        assert_ne!(from_bytes(b"\xe9"), from_bytes(b"\xe8"));
    }

    #[test]
    fn a_cut_off_sequence_keeps_its_bytes_and_what_follows() {
        let bytes = "é".as_bytes();
        let cut = [&bytes[..1], "é".as_bytes()].concat();
        assert_eq!(to_os(&from_bytes(&cut)).as_encoded_bytes(), cut);
    }

    #[test]
    fn plain_names_pass_through_borrowed() {
        for name in [
            "src/a.rs",
            "sp ace.txt",
            r"back\slash",
            "日本語/ファイル.rs",
            "👩‍💻.txt",
        ] {
            assert!(matches!(shown(name), Cow::Borrowed(_)), "{name}");
            assert_eq!(to_os(name), name);
        }
    }

    #[test]
    fn nothing_a_terminal_acts_on_gets_through() {
        assert_eq!(shown("a\tb\nc\rd"), r"a\tb\nc\rd");
        assert_eq!(shown("\x1b[31mred\x07"), r"\033[31mred\007");
        // C1 controls, which some terminals take as escapes, and a bidi override.
        assert_eq!(shown("a\u{9b}b"), r"a\302\233b");
        assert_eq!(shown("a\u{202e}b"), r"a\342\200\256b");
        assert_eq!(shown("a\u{2028}b"), r"a\342\200\250b");
    }
}
