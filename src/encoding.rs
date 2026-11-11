//! Character encodings supported by the editor.
//!
//! Decoding/encoding is implemented by hand: the supported single byte
//! charsets differ from Latin-1 in a handful of code points only, so a few
//! tiny tables are all that is needed (no external dependency).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Ascii,
    Iso8859_1,
    Iso8859_15,
    Windows1252,
}

pub const ALL: [Encoding; 8] = [
    Encoding::Utf8,
    Encoding::Utf8Bom,
    Encoding::Utf16Le,
    Encoding::Utf16Be,
    Encoding::Ascii,
    Encoding::Iso8859_1,
    Encoding::Iso8859_15,
    Encoding::Windows1252,
];

impl Encoding {
    pub fn id(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf8Bom => "utf-8-bom",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
            Encoding::Ascii => "ascii",
            Encoding::Iso8859_1 => "iso-8859-1",
            Encoding::Iso8859_15 => "iso-8859-15",
            Encoding::Windows1252 => "windows-1252",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 con BOM",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Ascii => "ASCII",
            Encoding::Iso8859_1 => "ISO-8859-1",
            Encoding::Iso8859_15 => "ISO-8859-15",
            Encoding::Windows1252 => "Windows-1252",
        }
    }

    pub fn from_id(s: &str) -> Option<Encoding> {
        ALL.iter().copied().find(|e| e.id() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    Lf,
    CrLf,
}

impl Eol {
    pub fn id(self) -> &'static str {
        match self {
            Eol::Lf => "lf",
            Eol::CrLf => "crlf",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Eol::Lf => "LF",
            Eol::CrLf => "CRLF",
        }
    }
    pub fn from_id(s: &str) -> Option<Eol> {
        match s {
            "lf" => Some(Eol::Lf),
            "crlf" => Some(Eol::CrLf),
            _ => None,
        }
    }
}

/// Windows-1252 code points for bytes 0x80..=0x9F (undefined bytes map to
/// the corresponding C1 control, as browsers do, so they round-trip).
const CP1252_HIGH: [u16; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0x008D, 0x017D, 0x008F, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
];

/// Bytes where ISO-8859-15 differs from ISO-8859-1.
const LATIN9_DIFF: [(u8, char); 8] = [
    (0xA4, '\u{20AC}'),
    (0xA6, '\u{0160}'),
    (0xA8, '\u{0161}'),
    (0xB4, '\u{017D}'),
    (0xB8, '\u{017E}'),
    (0xBC, '\u{0152}'),
    (0xBD, '\u{0153}'),
    (0xBE, '\u{0178}'),
];

fn decode_byte(enc: Encoding, b: u8) -> char {
    if b < 0x80 {
        return b as char;
    }
    match enc {
        Encoding::Ascii => '\u{FFFD}',
        Encoding::Windows1252 if b < 0xA0 => {
            char::from_u32(CP1252_HIGH[(b - 0x80) as usize] as u32).unwrap_or('\u{FFFD}')
        }
        Encoding::Iso8859_15 => LATIN9_DIFF
            .iter()
            .find(|(x, _)| *x == b)
            .map(|(_, c)| *c)
            .unwrap_or(b as char),
        _ => b as char,
    }
}

fn encode_char(enc: Encoding, c: char) -> Option<u8> {
    let u = c as u32;
    if u < 0x80 {
        return Some(u as u8);
    }
    match enc {
        Encoding::Ascii => None,
        Encoding::Iso8859_1 => (u < 0x100).then_some(u as u8),
        Encoding::Iso8859_15 => {
            if let Some((b, _)) = LATIN9_DIFF.iter().find(|(_, x)| *x == c) {
                return Some(*b);
            }
            if u < 0x100 && !LATIN9_DIFF.iter().any(|(b, _)| *b as u32 == u) {
                Some(u as u8)
            } else {
                None
            }
        }
        Encoding::Windows1252 => {
            if (0xA0..0x100).contains(&u) {
                return Some(u as u8);
            }
            CP1252_HIGH
                .iter()
                .position(|&x| x as u32 == u)
                .map(|i| 0x80 + i as u8)
        }
        _ => None,
    }
}

fn decode_utf16(bytes: &[u8], le: bool) -> String {
    let units = bytes.as_chunks::<2>().0.iter().map(|p| {
        if le {
            u16::from_le_bytes([p[0], p[1]])
        } else {
            u16::from_be_bytes([p[0], p[1]])
        }
    });
    char::decode_utf16(units)
        .map(|r| r.unwrap_or('\u{FFFD}'))
        .collect()
}

/// Heuristic detection of BOM-less UTF-16: ASCII-heavy UTF-16 text has a
/// NUL in every other byte.
fn sniff_utf16(bytes: &[u8]) -> Option<Encoding> {
    let sample = &bytes[..bytes.len().min(4096) & !1];
    if sample.len() < 4 {
        return None;
    }
    let pairs = sample.len() / 2;
    let (mut even0, mut odd0) = (0usize, 0usize);
    for p in sample.as_chunks::<2>().0 {
        even0 += (p[0] == 0) as usize;
        odd0 += (p[1] == 0) as usize;
    }
    if odd0 * 10 > pairs * 4 && even0 * 50 < pairs {
        Some(Encoding::Utf16Le)
    } else if even0 * 10 > pairs * 4 && odd0 * 50 < pairs {
        Some(Encoding::Utf16Be)
    } else {
        None
    }
}

fn detect_latin(bytes: &[u8]) -> Encoding {
    if bytes.iter().any(|&b| (0x80..0xA0).contains(&b)) {
        Encoding::Windows1252
    } else if bytes.contains(&0xA4) {
        // In Western texts '€' is far more likely than '¤'.
        Encoding::Iso8859_15
    } else {
        Encoding::Iso8859_1
    }
}

/// Guess the encoding of raw file contents.
pub fn detect(bytes: &[u8]) -> Encoding {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Encoding::Utf8Bom;
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Encoding::Utf16Le;
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Encoding::Utf16Be;
    }
    if let Some(e) = sniff_utf16(bytes) {
        return e;
    }
    if std::str::from_utf8(bytes).is_ok() {
        // Pure ASCII is reported as UTF-8 (a superset) so that typing
        // non-ASCII characters later does not lose data on save.
        return Encoding::Utf8;
    }
    detect_latin(bytes)
}

/// Normalize `\r\n` and lone `\r` to `\n` into `out`, reusing its capacity.
fn normalize_eol_into(src: &str, out: &mut String) {
    out.clear();
    out.reserve(src.len());
    let bytes = src.as_bytes();
    let mut at = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            out.push_str(&src[at..i]);
            out.push('\n');
            i += 1;
            if bytes.get(i) == Some(&b'\n') {
                i += 1;
            }
            at = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&src[at..]);
}

/// Detect encoding, decode `bytes` and normalize line endings to `\n` in a
/// single UTF-8 pass.
pub fn decode_auto_owned(mut bytes: Vec<u8>) -> String {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.drain(..3);
        let s = String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        return normalize_eol(s).0;
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return normalize_eol(decode_utf16(&bytes[2..], true)).0;
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return normalize_eol(decode_utf16(&bytes[2..], false)).0;
    }
    if let Some(enc) = sniff_utf16(&bytes) {
        return normalize_eol(decode_utf16(&bytes, enc == Encoding::Utf16Le)).0;
    }
    match String::from_utf8(bytes) {
        Ok(s) => normalize_eol(s).0,
        Err(e) => {
            let bytes = e.into_bytes();
            let enc = detect_latin(&bytes);
            normalize_eol(decode(&bytes, enc)).0
        }
    }
}

/// Like `decode_auto_owned`, but borrows directly from `bytes` when it is
/// valid `\n`-only UTF-8 and writes into `scratch` otherwise.
pub fn decode_auto_slice<'a>(bytes: &'a [u8], scratch: &'a mut String) -> &'a str {
    if let Some(b) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        match std::str::from_utf8(b) {
            Ok(s) if !s.contains('\r') => return s,
            Ok(s) => {
                normalize_eol_into(s, scratch);
                return scratch;
            }
            Err(_) => {
                *scratch = normalize_eol(String::from_utf8_lossy(b).into_owned()).0;
                return scratch;
            }
        }
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        *scratch = normalize_eol(decode_utf16(&bytes[2..], true)).0;
        return scratch;
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        *scratch = normalize_eol(decode_utf16(&bytes[2..], false)).0;
        return scratch;
    }
    if let Some(enc) = sniff_utf16(bytes) {
        *scratch = normalize_eol(decode_utf16(bytes, enc == Encoding::Utf16Le)).0;
        return scratch;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) if !s.contains('\r') => s,
        Ok(s) => {
            normalize_eol_into(s, scratch);
            scratch
        }
        Err(_) => {
            let enc = detect_latin(bytes);
            *scratch = normalize_eol(decode(bytes, enc)).0;
            scratch
        }
    }
}

/// `decode` taking the bytes: valid UTF-8 (the common case) becomes the
/// string without a copy.
pub fn decode_owned(mut bytes: Vec<u8>, enc: Encoding) -> String {
    if matches!(enc, Encoding::Utf8 | Encoding::Utf8Bom) {
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            bytes.drain(..3);
        }
        return String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    }
    decode(&bytes, enc)
}

/// Decode raw bytes with the given encoding (BOMs are stripped).
pub fn decode(bytes: &[u8], enc: Encoding) -> String {
    match enc {
        Encoding::Utf8 | Encoding::Utf8Bom => {
            let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
            match std::str::from_utf8(b) {
                Ok(s) => s.to_owned(),
                Err(_) => String::from_utf8_lossy(b).into_owned(),
            }
        }
        Encoding::Utf16Le => decode_utf16(bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes), true),
        Encoding::Utf16Be => decode_utf16(bytes.strip_prefix(&[0xFE, 0xFF]).unwrap_or(bytes), false),
        _ => {
            let mut s = String::with_capacity(bytes.len() + bytes.len() / 8);
            s.extend(bytes.iter().map(|&b| decode_byte(enc, b)));
            s
        }
    }
}

/// Encode text; returns the bytes and the number of characters that could
/// not be represented (replaced by '?').
pub fn encode(text: &str, enc: Encoding) -> (Vec<u8>, usize) {
    let mut bad = 0;
    let out = match enc {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf8Bom => {
            let mut v = Vec::with_capacity(text.len() + 3);
            v.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            v.extend_from_slice(text.as_bytes());
            v
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let le = enc == Encoding::Utf16Le;
            let mut v = Vec::with_capacity(text.len() * 2 + 2);
            v.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            for u in text.encode_utf16() {
                v.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
            }
            v
        }
        _ => {
            let mut v = Vec::with_capacity(text.len());
            for c in text.chars() {
                match encode_char(enc, c) {
                    Some(b) => v.push(b),
                    None => {
                        bad += 1;
                        v.push(b'?');
                    }
                }
            }
            v
        }
    };
    (out, bad)
}

/// Number of characters of `text` that `enc` cannot represent.
pub fn unrepresentable(text: &str, enc: Encoding) -> usize {
    match enc {
        Encoding::Utf8 | Encoding::Utf8Bom | Encoding::Utf16Le | Encoding::Utf16Be => 0,
        _ => text.chars().filter(|&c| encode_char(enc, c).is_none()).count(),
    }
}

/// Normalise line endings to '\n', reporting the original convention.
pub fn normalize_eol(text: String) -> (String, Eol) {
    if !text.contains('\r') {
        return (text, Eol::Lf);
    }
    let eol = if text.contains("\r\n") { Eol::CrLf } else { Eol::Lf };
    // In place, in one pass: "\r\n" and a lone '\r' become '\n'.
    let mut bytes = text.into_bytes();
    let mut w = 0;
    let mut r = 0;
    while r < bytes.len() {
        let b = bytes[r];
        r += 1;
        if b == b'\r' {
            if bytes.get(r) == Some(&b'\n') {
                r += 1;
            }
            bytes[w] = b'\n';
        } else {
            bytes[w] = b;
        }
        w += 1;
    }
    bytes.truncate(w);
    // Only ASCII bytes were replaced by ASCII bytes.
    (String::from_utf8(bytes).expect("still UTF-8"), eol)
}

pub fn apply_eol(text: &str, eol: Eol) -> std::borrow::Cow<'_, str> {
    match eol {
        Eol::Lf => text.into(),
        Eol::CrLf => text.replace('\n', "\r\n").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all() {
        let s = "Perché l'€ è caro: Šž Œ “ok” ±";
        for enc in [Encoding::Utf8, Encoding::Utf8Bom, Encoding::Utf16Le, Encoding::Utf16Be] {
            let (b, bad) = encode(s, enc);
            assert_eq!(bad, 0);
            assert_eq!(detect(&b), enc);
            assert_eq!(decode(&b, enc), s);
        }
        let (b, bad) = encode(s, Encoding::Windows1252);
        assert_eq!(bad, 0);
        assert_eq!(decode(&b, Encoding::Windows1252), s);
        let (b, bad) = encode(s, Encoding::Iso8859_15);
        assert_eq!(bad, 2); // “ ”
        assert_eq!(decode(&b, Encoding::Iso8859_15), s.replace(['“', '”'], "?"));
        let (_, bad) = encode(s, Encoding::Iso8859_1);
        assert_eq!(bad, 6);
        let (b, bad) = encode("plain", Encoding::Ascii);
        assert_eq!((b.as_slice(), bad), (&b"plain"[..], 0));
    }

    #[test]
    fn detection() {
        assert_eq!(detect(b"hello"), Encoding::Utf8);
        assert_eq!(detect(&[b'a', 0xE8, b'b']), Encoding::Iso8859_1);
        assert_eq!(detect(&[b'a', 0xA4]), Encoding::Iso8859_15);
        assert_eq!(detect(&[0x93, b'a', 0x94]), Encoding::Windows1252);
        let bom_less: Vec<u8> = "hello world".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(detect(&bom_less), Encoding::Utf16Le);
    }

    #[test]
    fn eol() {
        let (t, e) = normalize_eol("a\r\nb\r\n".into());
        assert_eq!((t.as_str(), e), ("a\nb\n", Eol::CrLf));
        assert_eq!(apply_eol(&t, e), "a\r\nb\r\n");
        assert_eq!(normalize_eol("a\rb\r\r\nc\r".into()).0, "a\nb\n\nc\n");
        assert_eq!(normalize_eol("è\r\nà".into()), ("è\nà".to_string(), Eol::CrLf));
        assert_eq!(decode_owned(vec![0xEF, 0xBB, 0xBF, b'a'], Encoding::Utf8Bom), "a");
        assert_eq!(decode_owned(vec![b'a', 0xFF], Encoding::Utf8), "a\u{FFFD}");

        let mut scratch = String::new();
        let samples: Vec<Vec<u8>> = vec![
            b"ciao\nmondo\n".to_vec(),
            b"ciao\r\nmondo\rfin".to_vec(),
            vec![0xEF, 0xBB, 0xBF, b'a', b'\r', b'\n', b'b'],
            vec![0xEF, 0xBB, 0xBF, b'a', 0xFF, b'\r', b'\n'],
            vec![b'a', 0xE8, b'\r', b'\n', b'b'],
            vec![b'a', 0xA4, b'\n'],
            vec![0x93, b'a', 0x94],
            encode("riga 1\r\nriga 2 €", Encoding::Utf16Le).0,
            encode("riga 1\r\nriga 2 €", Encoding::Utf16Be).0,
        ];
        for raw in samples {
            let expected = normalize_eol(decode_owned(raw.clone(), detect(&raw))).0;
            assert_eq!(decode_auto_owned(raw.clone()), expected);
            assert_eq!(decode_auto_slice(&raw, &mut scratch), expected);
        }
    }
}
