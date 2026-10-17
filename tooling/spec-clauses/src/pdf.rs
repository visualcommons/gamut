//! The PDF object layer (ISO 32000-1 §7): just enough to reach every page's content stream and the
//! fonts it draws with.
//!
//! Objects are located by scanning the file for `N G obj` rather than by reading the
//! cross-reference table. Both vendored ICC PDFs keep most objects inside compressed object
//! streams (`/Type /ObjStm`, §7.5.7) addressed by a cross-reference *stream* with PNG predictors;
//! a scan reaches the same objects with none of that machinery, and an incremental update is
//! honoured because a later definition of a number replaces an earlier one. Objects inside an
//! object stream fill only numbers no top-level definition claims.

use std::collections::HashMap;

/// A PDF object. `Op` is a bare keyword, which only a content stream contains.
#[derive(Debug, Clone, PartialEq)]
pub enum Obj {
    /// `null`, and anything a reference resolves to that does not exist (§7.3.10).
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// An integer or real number.
    Num(f64),
    /// `/Name`, with `#xx` escapes decoded.
    Name(Vec<u8>),
    /// A literal or hexadecimal string's bytes.
    Str(Vec<u8>),
    /// `[ ... ]`.
    Array(Vec<Obj>),
    /// `<< ... >>`.
    Dict(Dict),
    /// `N G R`.
    Ref(u32),
    /// A stream: its dictionary and its still-encoded data.
    Stream(Dict, Vec<u8>),
    /// A content-stream operator.
    Op(Vec<u8>),
}

/// A dictionary, in source order.
pub type Dict = Vec<(Vec<u8>, Obj)>;

/// Looks `key` up in a dictionary.
pub fn get<'a>(d: &'a Dict, key: &[u8]) -> Option<&'a Obj> {
    d.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

impl Obj {
    /// The number, if this is one.
    pub fn num(&self) -> Option<f64> {
        match self {
            Obj::Num(n) => Some(*n),
            _ => None,
        }
    }
    /// The name's bytes, if this is a name.
    pub fn name(&self) -> Option<&[u8]> {
        match self {
            Obj::Name(n) => Some(n),
            _ => None,
        }
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0')
}

fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// A cursor over PDF syntax.
pub struct Lexer<'a> {
    b: &'a [u8],
    /// Current byte offset.
    pub pos: usize,
}

impl<'a> Lexer<'a> {
    /// A lexer at offset `pos` of `b`.
    pub fn new(b: &'a [u8], pos: usize) -> Self {
        Lexer { b, pos }
    }

    /// Skips whitespace and `%` comments.
    pub fn skip_ws(&mut self) {
        while let Some(&c) = self.b.get(self.pos) {
            if is_ws(c) {
                self.pos += 1;
            } else if c == b'%' {
                while self
                    .b
                    .get(self.pos)
                    .is_some_and(|&c| c != b'\n' && c != b'\r')
                {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn regular_token(&mut self) -> &'a [u8] {
        let start = self.pos;
        while self
            .b
            .get(self.pos)
            .is_some_and(|&c| !is_ws(c) && !is_delim(c))
        {
            self.pos += 1;
        }
        &self.b[start..self.pos]
    }

    /// The next object, or `None` at the end of input. `R` references are folded: a number
    /// followed by a number and `R` reads as one [`Obj::Ref`].
    pub fn next(&mut self) -> Result<Option<Obj>, String> {
        self.skip_ws();
        let Some(&c) = self.b.get(self.pos) else {
            return Ok(None);
        };
        let obj = match c {
            b'/' => {
                self.pos += 1;
                Obj::Name(decode_name(self.regular_token()))
            }
            b'(' => Obj::Str(self.literal_string()?),
            b'<' if self.b.get(self.pos + 1) == Some(&b'<') => {
                self.pos += 2;
                let mut d = Dict::new();
                loop {
                    self.skip_ws();
                    if self.b[self.pos..].starts_with(b">>") {
                        self.pos += 2;
                        break;
                    }
                    let key = match self.next()? {
                        Some(Obj::Name(k)) => k,
                        other => return Err(format!("dictionary key {other:?} at {}", self.pos)),
                    };
                    let val = self.next()?.ok_or("unterminated dictionary")?;
                    d.push((key, val));
                }
                Obj::Dict(d)
            }
            b'<' => {
                self.pos += 1;
                let start = self.pos;
                let end = self.b[start..]
                    .iter()
                    .position(|&c| c == b'>')
                    .ok_or("unterminated hex string")?;
                self.pos = start + end + 1;
                Obj::Str(decode_hex(&self.b[start..start + end]))
            }
            b'[' => {
                self.pos += 1;
                let mut a = Vec::new();
                loop {
                    self.skip_ws();
                    if self.b.get(self.pos) == Some(&b']') {
                        self.pos += 1;
                        break;
                    }
                    a.push(self.next()?.ok_or("unterminated array")?);
                }
                Obj::Array(a)
            }
            b')' | b'>' | b']' | b'{' | b'}' => {
                self.pos += 1;
                Obj::Op(vec![c])
            }
            _ => {
                let tok = self.regular_token();
                if tok.is_empty() {
                    return Err(format!("unexpected byte {c:#04x} at {}", self.pos));
                }
                match tok {
                    b"true" => Obj::Bool(true),
                    b"false" => Obj::Bool(false),
                    b"null" => Obj::Null,
                    _ => match parse_number(tok) {
                        Some(n) => self.maybe_ref(n),
                        None => Obj::Op(tok.to_vec()),
                    },
                }
            }
        };
        Ok(Some(obj))
    }

    /// After a number: if `G R` follows and the number is a non-negative integer, it was an
    /// indirect reference.
    fn maybe_ref(&mut self, n: f64) -> Obj {
        let save = self.pos;
        if n >= 0.0 && n.fract() == 0.0 {
            self.skip_ws();
            let g = self.regular_token();
            if !g.is_empty() && g.iter().all(u8::is_ascii_digit) {
                self.skip_ws();
                if self.b.get(self.pos) == Some(&b'R')
                    && self
                        .b
                        .get(self.pos + 1)
                        .is_none_or(|&c| is_ws(c) || is_delim(c))
                {
                    self.pos += 1;
                    return Obj::Ref(n as u32);
                }
            }
        }
        self.pos = save;
        Obj::Num(n)
    }

    fn literal_string(&mut self) -> Result<Vec<u8>, String> {
        self.pos += 1;
        let mut out = Vec::new();
        let mut depth = 1u32;
        loop {
            let c = *self.b.get(self.pos).ok_or("unterminated string")?;
            self.pos += 1;
            match c {
                b'(' => {
                    depth += 1;
                    out.push(c);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(out);
                    }
                    out.push(c);
                }
                b'\\' => {
                    let e = *self.b.get(self.pos).ok_or("unterminated escape")?;
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'\r' => {
                            if self.b.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut v = u32::from(e - b'0');
                            for _ in 0..2 {
                                match self.b.get(self.pos) {
                                    Some(&d @ b'0'..=b'7') => {
                                        v = v * 8 + u32::from(d - b'0');
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push((v & 0xff) as u8);
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(c),
            }
        }
    }
}

fn parse_number(tok: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(tok).ok()?;
    if !s
        .bytes()
        .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+'))
    {
        return None;
    }
    if !s.bytes().any(|c| c.is_ascii_digit()) {
        return None;
    }
    // PDF writers emit forms Rust rejects ("-.5" parses, "5." parses, "--5" does not exist).
    s.parse().ok()
}

fn decode_name(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'#'
            && let Some(v) = raw
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    out
}

/// Hex digits to bytes, whitespace ignored, an odd final digit padded with 0 (§7.3.4.3).
pub fn decode_hex(h: &[u8]) -> Vec<u8> {
    let digits: Vec<u8> = h
        .iter()
        .filter_map(|&c| (c as char).to_digit(16).map(|d| d as u8))
        .collect();
    digits
        .chunks(2)
        .map(|p| p[0] << 4 | p.get(1).copied().unwrap_or(0))
        .collect()
}

/// A parsed document: every object by number.
pub struct Document {
    objects: HashMap<u32, Obj>,
}

impl Document {
    /// Locates and parses every object in `b`, expanding object streams.
    pub fn parse(b: &[u8]) -> Result<Self, String> {
        let mut objects = HashMap::new();
        let mut i = 0;
        while let Some(off) = find(b, b"obj", i) {
            i = off + 3;
            if b.get(off + 3).is_some_and(|&c| !is_ws(c) && !is_delim(c)) {
                continue; // "objx", e.g. part of "endobj"'s neighbour or a name
            }
            let Some(num) = header_number(b, off) else {
                continue;
            };
            let mut lx = Lexer::new(b, off + 3);
            let Ok(Some(obj)) = lx.next() else { continue };
            let obj = match obj {
                Obj::Dict(d) => {
                    lx.skip_ws();
                    if b[lx.pos..].starts_with(b"stream") {
                        let data = stream_data(b, lx.pos + 6, &d);
                        // Resume the scan past the data, so binary bytes that happen to spell
                        // `N G obj` are never read as an object header.
                        i = i.max(data.end);
                        Obj::Stream(d, b[data].to_vec())
                    } else {
                        Obj::Dict(d)
                    }
                }
                other => other,
            };
            objects.insert(num, obj);
        }
        let mut doc = Document { objects };
        doc.expand_object_streams()?;
        Ok(doc)
    }

    fn expand_object_streams(&mut self) -> Result<(), String> {
        let streams: Vec<(Dict, Vec<u8>)> = self
            .objects
            .values()
            .filter_map(|o| match o {
                Obj::Stream(d, data) if get(d, b"Type").and_then(Obj::name) == Some(b"ObjStm") => {
                    Some((d.clone(), data.clone()))
                }
                _ => None,
            })
            .collect();
        for (d, raw) in streams {
            let data = self.decode_stream(&d, &raw)?;
            let n = get(&d, b"N")
                .and_then(Obj::num)
                .ok_or("ObjStm without /N")? as usize;
            let first = get(&d, b"First")
                .and_then(Obj::num)
                .ok_or("ObjStm without /First")? as usize;
            let mut header = Lexer::new(&data, 0);
            for _ in 0..n {
                let (Some(Obj::Num(num)), Some(Obj::Num(rel))) = (header.next()?, header.next()?)
                else {
                    return Err("malformed ObjStm header".into());
                };
                let mut lx = Lexer::new(&data, first + rel as usize);
                if let Some(obj) = lx.next()? {
                    self.objects.entry(num as u32).or_insert(obj);
                }
            }
        }
        Ok(())
    }

    /// Follows a reference (repeatedly); a non-reference is returned as is.
    pub fn resolve<'a>(&'a self, o: &'a Obj) -> &'a Obj {
        let mut cur = o;
        for _ in 0..32 {
            match cur {
                Obj::Ref(n) => cur = self.objects.get(n).unwrap_or(&Obj::Null),
                _ => return cur,
            }
        }
        &Obj::Null
    }

    /// The dictionary `o` is or resolves to (a stream's dictionary included).
    pub fn dict<'a>(&'a self, o: &'a Obj) -> Option<&'a Dict> {
        match self.resolve(o) {
            Obj::Dict(d) | Obj::Stream(d, _) => Some(d),
            _ => None,
        }
    }

    /// The decoded bytes of the stream `o` is or resolves to.
    pub fn stream(&self, o: &Obj) -> Result<Vec<u8>, String> {
        match self.resolve(o) {
            Obj::Stream(d, raw) => self.decode_stream(d, raw),
            other => Err(format!("expected a stream, found {other:?}")),
        }
    }

    fn decode_stream(&self, d: &Dict, raw: &[u8]) -> Result<Vec<u8>, String> {
        let filters: Vec<Vec<u8>> = match get(d, b"Filter").map(|f| self.resolve(f)) {
            None | Some(Obj::Null) => Vec::new(),
            Some(Obj::Name(n)) => vec![n.clone()],
            Some(Obj::Array(a)) => a
                .iter()
                .filter_map(|f| self.resolve(f).name().map(<[u8]>::to_vec))
                .collect(),
            Some(other) => return Err(format!("unsupported /Filter {other:?}")),
        };
        if get(d, b"DecodeParms").is_some_and(|p| !matches!(self.resolve(p), Obj::Null)) {
            return Err("stream /DecodeParms (predictors) are not supported".into());
        }
        let mut data = raw.to_vec();
        for f in filters {
            data = match f.as_slice() {
                b"FlateDecode" => miniz_oxide::inflate::decompress_to_vec_zlib(&data)
                    .map_err(|e| format!("FlateDecode: {e:?}"))?,
                other => {
                    return Err(format!(
                        "unsupported filter /{}",
                        String::from_utf8_lossy(other)
                    ));
                }
            };
        }
        Ok(data)
    }

    /// The document catalog (`/Type /Catalog`).
    pub fn catalog(&self) -> Option<&Dict> {
        let mut nums: Vec<&u32> = self.objects.keys().collect();
        nums.sort();
        nums.into_iter().find_map(|n| match &self.objects[n] {
            Obj::Dict(d) if get(d, b"Type").and_then(Obj::name) == Some(b"Catalog") => Some(d),
            _ => None,
        })
    }
}

/// The object number of an `N G obj` header ending at `obj_off`, if the bytes before it are one.
fn header_number(b: &[u8], obj_off: usize) -> Option<u32> {
    let mut p = obj_off;
    let mut nums = [0u32; 2];
    for slot in nums.iter_mut().rev() {
        let end = {
            while p > 0 && is_ws(b[p - 1]) {
                p -= 1;
            }
            p
        };
        while p > 0 && b[p - 1].is_ascii_digit() {
            p -= 1;
        }
        if p == end {
            return None;
        }
        *slot = std::str::from_utf8(&b[p..end]).ok()?.parse().ok()?;
    }
    (p == 0 || is_ws(b[p - 1]) || is_delim(b[p - 1])).then_some(nums[0])
}

/// The byte range of a stream's raw data, whose `stream` keyword ends at `after_kw`: `/Length` when
/// it is a direct integer that lands on `endstream`, otherwise up to the next `endstream`.
fn stream_data(b: &[u8], after_kw: usize, d: &Dict) -> std::ops::Range<usize> {
    let mut start = after_kw;
    if b.get(start) == Some(&b'\r') {
        start += 1;
    }
    if b.get(start) == Some(&b'\n') {
        start += 1;
    }
    if let Some(len) = get(d, b"Length").and_then(Obj::num) {
        let end = start + len as usize;
        if let Some(tail) = b.get(end..) {
            let ws = tail.iter().take_while(|&&c| is_ws(c)).count();
            if tail[ws..].starts_with(b"endstream") {
                return start..end;
            }
        }
    }
    start..find(b, b"endstream", start).unwrap_or(b.len())
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex_all(s: &[u8]) -> Vec<Obj> {
        let mut lx = Lexer::new(s, 0);
        let mut v = Vec::new();
        while let Some(o) = lx.next().unwrap() {
            v.push(o);
        }
        v
    }

    #[test]
    fn lexer_reads_references_numbers_names_and_operators() {
        let got = lex_all(b"12 0 R 3 4 /A#20B -.5 Tf [(a\\)b) <4142>] % c\n");
        assert_eq!(
            got,
            vec![
                Obj::Ref(12),
                Obj::Num(3.0),
                Obj::Num(4.0),
                Obj::Name(b"A B".to_vec()),
                Obj::Num(-0.5),
                Obj::Op(b"Tf".to_vec()),
                Obj::Array(vec![Obj::Str(b"a)b".to_vec()), Obj::Str(b"AB".to_vec())]),
            ]
        );
    }

    #[test]
    fn literal_strings_nest_and_decode_octal() {
        assert_eq!(
            lex_all(b"(a(b)c\\101\\0501)"),
            vec![Obj::Str(b"a(b)cA(1".to_vec())]
        );
    }

    #[test]
    fn hex_strings_pad_an_odd_digit() {
        assert_eq!(decode_hex(b"4 1 4"), vec![0x41, 0x40]);
    }

    #[test]
    fn object_streams_expand_and_references_resolve() {
        // Object 2 is a stream holding objects 5 and 6; object 1 is a plain dict pointing at 6.
        let inner = b"5 0 6 4 (x) <</K 5 0 R>>";
        let z = miniz_oxide::deflate::compress_to_vec_zlib(inner, 6);
        let mut pdf = b"%PDF-1.5\n1 0 obj <</Type/Catalog/P 6 0 R>> endobj\n".to_vec();
        pdf.extend_from_slice(
            format!(
                "2 0 obj <</Type/ObjStm/N 2/First 8/Filter/FlateDecode/Length {}>>stream\n",
                z.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&z);
        pdf.extend_from_slice(b"\nendstream endobj\n");
        let doc = Document::parse(&pdf).unwrap();
        let cat = doc.catalog().unwrap();
        let p = doc.dict(get(cat, b"P").unwrap()).unwrap();
        assert_eq!(doc.resolve(get(p, b"K").unwrap()), &Obj::Str(b"x".to_vec()));
    }

    #[test]
    fn header_number_needs_two_integers() {
        assert_eq!(header_number(b"\n17 0 obj", 6), Some(17));
        assert_eq!(header_number(b"\n0 obj", 3), None);
        assert_eq!(header_number(b"x17 0 obj", 6), None);
    }
}
