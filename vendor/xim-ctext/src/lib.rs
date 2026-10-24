//! COMPOUND_TEXT decoder used by the XIM client.
//!
//! qcode fork of `xim-ctext` 0.3.0 (MIT, <https://github.com/Riey/xim-rs>).
//! Upstream only understood UTF-8 (behind the `ESC % G` escape) and
//! ISO-2022-JP; every other charset designator returned
//! [`DecodeError::UnsupportedEncoding`], which is fatal for callers such as
//! `zed-xim`, that decode preedit text with
//! `.expect("Encoding Error")`.
//!
//! XIM servers do send the other designators: with `LANG=zh_CN.UTF-8`, Xlib
//! turns a UTF-8 string into COMPOUND_TEXT as `ESC $ ( A` + 7-bit GB 2312
//! (`你好` → `1B 24 28 41 44 63 3A 43`), keeps ASCII runs inside `ESC ( B`,
//! wraps emoji in `ESC % G` … `ESC % @` and emits characters that the locale
//! cannot map as raw ISO-8859-1 bytes (`caf\xe9`).
//!
//! This decoder follows those designators, decodes every run with the matching
//! `encoding_rs` codec and falls back to a lossy single byte decode for
//! anything it does not know. Decoding is **total**: `compound_text_to_utf8`
//! keeps its `Result` signature for API compatibility but always returns `Ok`,
//! so a preedit the decoder cannot fully understand shows up with replacement
//! characters instead of aborting the process.

#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use encoding_rs::{
    Encoding, EUC_JP, EUC_KR, GBK, ISO_8859_2, ISO_8859_3, ISO_8859_4, ISO_8859_5, ISO_8859_6,
    WINDOWS_1252,
};

const UTF8_START: &[u8] = &[0x1B, 0x25, 0x47];
const UTF8_END: &[u8] = &[0x1B, 0x25, 0x40];

/// Wrapper for reduce allocation
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct CText<'s> {
    utf8: &'s str,
}

impl<'s> fmt::Debug for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> fmt::Display for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> CText<'s> {
    pub const fn new(utf8: &'s str) -> Self {
        Self { utf8 }
    }

    // Upstream API kept as is (CLI gate runs clippy with `-D warnings`).
    #[allow(clippy::len_without_is_empty)]
    pub const fn len(self) -> usize {
        self.utf8.len() + UTF8_START.len() + UTF8_END.len()
    }

    #[cfg(feature = "std")]
    pub fn write(self, mut out: impl std::io::Write) -> std::io::Result<usize> {
        let mut writed = 0;
        writed += out.write(UTF8_START)?;
        writed += out.write(self.utf8.as_bytes())?;
        writed += out.write(UTF8_END)?;
        Ok(writed)
    }
}

/// Encoding utf8 to COMPOUND_TEXT with utf8 escape
pub fn utf8_to_compound_text(text: &str) -> Vec<u8> {
    let mut ret = Vec::with_capacity(text.len() + 6);
    ret.extend_from_slice(UTF8_START);
    ret.extend_from_slice(text.as_bytes());
    ret.extend_from_slice(UTF8_END);
    ret
}

/// Kept for API compatibility with upstream.
///
/// The fork never constructs these values: decoding is best effort and
/// reports unreadable bytes with U+FFFD instead of failing, because the one
/// caller (`zed-xim`) treats an error as a `panic!`.
#[derive(Debug, Clone)]
pub enum DecodeError {
    InvalidEncoding,
    UnsupportedEncoding,
    Utf8Error(alloc::string::FromUtf8Error),
}

impl From<alloc::string::FromUtf8Error> for DecodeError {
    fn from(err: alloc::string::FromUtf8Error) -> Self {
        DecodeError::Utf8Error(err)
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEncoding => write!(f, "Invalid compound text"),
            Self::UnsupportedEncoding => write!(f, "This encoding is not supported yet"),
            Self::Utf8Error(e) => write!(f, "Not a valid utf8 {}", e),
        }
    }
}

/// Decode COMPOUND_TEXT into UTF-8.
///
/// Always succeeds; see the module documentation.
pub fn compound_text_to_utf8(bytes: &[u8]) -> Result<String, DecodeError> {
    Ok(decode_lossy(bytes))
}

/// Charset selected by an ISO-2022 style designator, i.e. what the run of bytes
/// after the designator means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Charset {
    /// Default, `ESC % G` and the 94-character set `ESC ( B`: UTF-8, which
    /// also covers plain ASCII.
    Utf8,
    /// `ESC $ ( A` / `ESC $ ) A`: GB 2312-80, decoded as its GBK superset.
    Gb2312,
    /// `ESC $ ( B`: JIS X 0208, decoded through its EUC-JP form.
    JisX0208,
    /// `ESC $ ( C`: KS C 5601, decoded through its EUC-KR form.
    KsC5601,
    /// 96-character set (`ESC - X` / `ESC . X`): single byte charset.
    SingleByte(&'static Encoding),
    /// A designator whose charset is not implemented (ISO 10646, JIS X 0212,
    /// …). The run is kept as lossy UTF-8 so the surrounding text survives.
    Unknown,
}

fn decode_lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut run: Vec<u8> = Vec::new();
    let mut charset = Charset::Utf8;
    let mut rest = bytes;

    while let Some(&byte) = rest.first() {
        if byte != 0x1B {
            run.push(byte);
            rest = &rest[1..];
            continue;
        }

        push_run(charset, &run, &mut out);
        run.clear();

        let (next, consumed) = parse_escape(&rest[1..]);
        if let Some(next) = next {
            charset = next;
        }
        rest = &rest[1 + consumed.min(rest.len() - 1)..];
    }

    push_run(charset, &run, &mut out);
    out
}

/// Parse the escape sequence that follows `ESC`.
///
/// Returns the charset the designator selects (if it changed one) and how many
/// bytes the sequence occupies.
fn parse_escape(rest: &[u8]) -> (Option<Charset>, usize) {
    let first = match rest.first() {
        Some(&byte) => byte,
        None => return (None, 0),
    };

    match first {
        // `ESC % G` starts UTF-8, `ESC % @` ends it. Both mean "UTF-8 from
        // here on"; servers may omit the closing sequence entirely.
        b'%' => match rest.get(1) {
            Some(b'G') | Some(b'@') => (Some(Charset::Utf8), 2),
            // `ESC % / F`: another coding system (ISO 10646, …).
            Some(b'/') => (Some(Charset::Unknown), 3),
            _ => (None, 1),
        },
        // `ESC $`: multi byte charsets, either `ESC $ ( F` or `ESC $ F`.
        b'$' => {
            let (final_index, consumed) = match rest.get(1) {
                Some(b'(') | Some(b')') | Some(b'*') | Some(b'+') => (2, 3),
                _ => (1, 2),
            };
            let charset = match rest.get(final_index) {
                Some(b'A') => Charset::Gb2312,
                Some(b'B') => Charset::JisX0208,
                Some(b'C') => Charset::KsC5601,
                _ => Charset::Unknown,
            };
            (Some(charset), consumed)
        }
        // `ESC - X` (G1) and `ESC . X` (G2): 96-character sets.
        b'-' | b'.' => {
            let charset = match rest.get(1) {
                Some(designator) if (b'A'..=b'F').contains(designator) => {
                    Charset::SingleByte(single_byte_charset(*designator))
                }
                _ => Charset::Unknown,
            };
            (Some(charset), 2)
        }
        // `ESC ( X` … `ESC + X`: 94-character sets for G0…G3. Only ASCII and
        // JIS X 0201 Roman can be shown; the rest is not implemented.
        b'(' | b')' | b'*' | b'+' => {
            let charset = match rest.get(1) {
                Some(b'B') | Some(b'J') => Charset::Utf8,
                _ => Charset::Unknown,
            };
            (Some(charset), 2)
        }
        // Control sequences carry no text.
        b'[' => (None, csi_len(rest)),
        b']' => (None, osc_len(rest)),
        // Remaining escapes (shifts, `ESC # 8`, `ESC \`, …) are ignored; the
        // run that follows keeps the current charset.
        _ => (None, 1),
    }
}

fn single_byte_charset(designator: u8) -> &'static Encoding {
    match designator {
        // ISO-8859-1: windows-1252 decodes every byte of the range and is the
        // superset browsers and terminals use for it.
        b'A' => WINDOWS_1252,
        b'B' => ISO_8859_2,
        b'C' => ISO_8859_3,
        b'D' => ISO_8859_4,
        b'E' => ISO_8859_5,
        // `b'F'`, the only remaining case of the `A..=F` match above.
        _ => ISO_8859_6,
    }
}

/// Length of a CSI sequence (`ESC [` … final byte in `0x40..=0x7e`).
fn csi_len(rest: &[u8]) -> usize {
    let mut index = 1;
    while index < rest.len() {
        if (0x40..=0x7e).contains(&rest[index]) {
            return index + 1;
        }
        index += 1;
    }
    rest.len()
}

/// Length of an OSC sequence (`ESC ]` … `BEL` or `ESC \`).
fn osc_len(rest: &[u8]) -> usize {
    let mut index = 1;
    while index < rest.len() {
        match rest[index] {
            0x07 => return index + 1,
            0x1B if rest.get(index + 1) == Some(&b'\\') => return index + 2,
            _ => index += 1,
        }
    }
    rest.len()
}

fn push_run(charset: Charset, bytes: &[u8], out: &mut String) {
    if bytes.is_empty() {
        return;
    }

    match charset {
        Charset::Utf8 | Charset::Unknown => push_utf8_or_latin1(bytes, out),
        Charset::Gb2312 => push_multibyte(GBK, bytes, out),
        Charset::JisX0208 => push_multibyte(EUC_JP, bytes, out),
        Charset::KsC5601 => push_multibyte(EUC_KR, bytes, out),
        Charset::SingleByte(encoding) => {
            let (decoded, _) = encoding.decode_without_bom_handling(bytes);
            out.push_str(&decoded);
        }
    }
}

fn push_utf8_or_latin1(bytes: &[u8], out: &mut String) {
    match core::str::from_utf8(bytes) {
        Ok(text) => out.push_str(text),
        // Xlib emits unescaped ISO-8859-1 bytes for text the X locale cannot
        // map (`café` in a UTF-8 locale arrives as `caf\xe9`).
        Err(_) => {
            let (decoded, _) = WINDOWS_1252.decode_without_bom_handling(bytes);
            out.push_str(&decoded);
        }
    }
}

fn push_multibyte(encoding: &'static Encoding, bytes: &[u8], out: &mut String) {
    // COMPOUND_TEXT 94x94 sets are 7-bit: ESC $ ( A carries GB 2312 as
    // 0x21..0x7e pairs and 0xA1..0xFE is the 8-bit form. Shift the GL bytes
    // into the 8-bit form encoding_rs decodes and keep bytes that already are
    // 8-bit. Control characters and space stay untouched.
    let widened: Vec<u8> = bytes
        .iter()
        .map(|&byte| {
            if (0x21..0x7f).contains(&byte) {
                byte | 0x80
            } else {
                byte
            }
        })
        .collect();
    let (decoded, _) = encoding.decode_without_bom_handling(&widened);
    out.push_str(&decoded);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> String {
        compound_text_to_utf8(bytes).expect("decoding is total")
    }

    #[test]
    fn korean() {
        const UTF8: &str = "가나다";
        const COMP: &[u8] = &[
            27, 37, 71, 234, 176, 128, 235, 130, 152, 235, 139, 164, 27, 37, 64,
        ];
        assert_eq!(utf8_to_compound_text(UTF8), COMP);
        assert_eq!(decode(COMP), UTF8);
    }

    #[test]
    fn iso_2011_jp() {
        const UTF8: &str = "東京";
        const COMP: &[u8] = &[27, 36, 40, 66, 69, 108, 53, 126];
        assert_eq!(decode(COMP), UTF8);
    }

    /// `Xutf8TextListToTextProperty("你好", XCompoundTextStyle)` with
    /// `LANG=zh_CN.UTF-8` — the string an IBus/fcitx XIM preedit carries.
    #[test]
    fn gb2312_chinese() {
        assert_eq!(decode(b"\x1b$(ADc:C"), "你好");
    }

    /// The same converter switching back to ASCII and into GB 2312 again.
    #[test]
    fn gb2312_mixed_with_ascii() {
        assert_eq!(
            decode(b"\x1b$(ADc:C\x1b(B, \x1b$(AJ@=g\x1b(B!"),
            "你好, 世界!"
        );
        assert_eq!(decode(b"\x1b$(ADc:C\x1b(Babc\x1b$(A:C"), "你好abc好");
        assert_eq!(decode(b"abc\x1b$(ADc:C"), "abc你好");
    }

    /// Emoji are wrapped in `ESC % G` … `ESC % @` next to GB 2312 runs.
    #[test]
    fn gb2312_next_to_utf8_escape() {
        assert_eq!(decode(b"\x1b$(ADc:C\x1b%G\xf0\x9f\x98\x80\x1b%@"), "你好😀");
        assert_eq!(decode(b"\x1b%G\xf0\x9f\x98\x80\x1b%@\x1b$(ADc:C"), "😀你好");
    }

    /// `ESC % G` without its closing `ESC % @`: the old decoder dropped the
    /// last three bytes of such a string.
    #[test]
    fn unterminated_utf8_escape_keeps_all_bytes() {
        assert_eq!(decode(b"\x1b%G\xe4\xbd\xa0"), "你");
        // A truncated sequence falls back to the single byte charset: lossy,
        // but never fatal.
        assert_eq!(decode(b"\x1b%G\xe4\xbd"), "ä½");
    }

    /// Xlib mixes unescaped ISO-8859-1 bytes into COMPOUND_TEXT; that used to
    /// be `DecodeError::Utf8Error`, i.e. another panic.
    #[test]
    fn raw_latin1_bytes() {
        assert_eq!(decode(b"caf\xe9"), "café");
        assert_eq!(decode(b"\x1b$(ADc:C\x1b(Bcaf\xe9"), "你好café");
    }

    /// 96-character set designator (`ESC - A` = ISO-8859-1 on G1).
    #[test]
    fn iso_8859_1_designator() {
        assert_eq!(decode(b"\x1b-Acaf\xe9"), "café");
    }

    /// KS C 5601 arrives as 7-bit pairs, like GB 2312.
    #[test]
    fn ks_c_5601_pairs() {
        // "가" = EUC-KR B0 A1 → 7-bit 30 21.
        assert_eq!(decode(b"\x1b$(C0!"), "가");
    }

    /// Everything the decoder does not understand is replaced, never fatal.
    #[test]
    fn unknown_designators_are_lossy() {
        assert_eq!(decode(b"\x1b%/Idoes not matter"), "does not matter");
        assert_eq!(decode(b"\x1b$(D\xa4\xa2"), "¤¢");
        assert_eq!(decode(&[0xff, 0xfe, 0x00]), "ÿþ\u{0}");
    }

    /// A lone `ESC` and truncated designators used to underflow (`len - 3`).
    #[test]
    fn truncated_escapes() {
        for input in [
            &b"\x1b"[..],
            b"\x1b%",
            b"\x1b%G",
            b"\x1b$",
            b"\x1b$(",
            b"\x1b$(\x1b",
            b"\x1b-",
            b"\x1b]",
            b"\x1b[",
            b"\x1b#",
        ] {
            assert!(compound_text_to_utf8(input).is_ok(), "{:?}", input);
        }
    }

    /// No byte sequence may panic or fail: XIM preedit text is untrusted input
    /// that reaches the event loop.
    #[test]
    fn never_fails() {
        let mut cases: Vec<Vec<u8>> = Vec::new();
        for byte in 0..=u8::MAX {
            cases.push(alloc::vec![byte]);
            cases.push(alloc::vec![0x1b, byte]);
            cases.push(alloc::vec![0x1b, 0x24, byte]);
            cases.push(alloc::vec![0x1b, 0x25, byte]);
            for tail in [0x00, 0x1b, 0x7f, 0x80, 0xff] {
                cases.push(alloc::vec![byte, tail]);
                cases.push(alloc::vec![0x1b, byte, tail]);
                cases.push(alloc::vec![0x1b, 0x24, byte, tail]);
            }
        }

        for input in cases {
            let decoded = compound_text_to_utf8(&input);
            assert!(decoded.is_ok(), "{:?}", input);
        }
    }
}
