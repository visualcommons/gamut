//! The PDF text layer: runs every page's content stream (ISO 32000-1 §9) far enough to know what
//! text is drawn, where, and in which font — then reads clause headings off the resulting lines.
//!
//! Position matters because PDF writers split a word across several show operators and join
//! words with positioning instead of a space glyph (`[(DToB0T)]TJ 23.64 0 TD [(ag)]TJ`). So a
//! glyph's advance is computed from the font's widths, and a gap is a space only where the next
//! run starts visibly to the right of where the previous one ended.
//!
//! What a heading is: a line that starts with a clause number (`9.2.10`, `A.3`, `Annex A`) in a
//! **bold** font and goes on to a title, and that is not a table-of-contents entry (those end in a
//! dot leader and a page number). Form XObjects (`Do`) are not entered: the vendored PDFs draw
//! their headings in page content, and a running header inside a form is exactly what the index
//! must not contain.

use std::collections::HashMap;

use crate::pdf::{Dict, Document, Lexer, Obj, get};
use crate::{Clause, collapse_ws};

/// A visual line of text.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// The text, runs joined with a space wherever a gap separates them.
    pub text: String,
    /// Whether the line's first non-blank glyph is in a bold font.
    pub bold: bool,
}

/// A font's mapping from codes to text, and the widths that advance the pen.
struct Font {
    /// Two-byte codes (a `Type0` font over `Identity-H`) instead of one.
    two_byte: bool,
    to_unicode: HashMap<u32, String>,
    widths: HashMap<u32, f64>,
    default_width: f64,
    bold: bool,
}

impl Font {
    fn load(doc: &Document, o: &Obj) -> Result<Font, String> {
        let d = doc.dict(o).ok_or("font is not a dictionary")?;
        let subtype = get(d, b"Subtype").and_then(Obj::name).unwrap_or(b"");
        let base = get(d, b"BaseFont")
            .and_then(Obj::name)
            .unwrap_or(b"")
            .to_vec();
        let mut weight_bold = false;
        let mut widths = HashMap::new();
        let mut default_width = 0.0;
        let two_byte = subtype == b"Type0";
        if two_byte {
            let enc = get(d, b"Encoding").map(|e| doc.resolve(e));
            if enc.and_then(Obj::name) != Some(b"Identity-H") {
                return Err(format!(
                    "Type0 font {} without Identity-H",
                    String::from_utf8_lossy(&base)
                ));
            }
            let desc = match get(d, b"DescendantFonts").map(|x| doc.resolve(x)) {
                Some(Obj::Array(a)) if !a.is_empty() => {
                    doc.dict(&a[0]).ok_or("bad descendant font")?
                }
                _ => return Err("Type0 font without DescendantFonts".into()),
            };
            default_width = get(desc, b"DW")
                .and_then(|w| doc.resolve(w).num())
                .unwrap_or(1000.0);
            if let Some(Obj::Array(w)) = get(desc, b"W").map(|w| doc.resolve(w)) {
                cid_widths(doc, w, &mut widths);
            }
            weight_bold |= descriptor_bold(doc, desc);
        } else {
            let first = get(d, b"FirstChar")
                .and_then(|f| doc.resolve(f).num())
                .unwrap_or(0.0) as u32;
            if let Some(Obj::Array(w)) = get(d, b"Widths").map(|w| doc.resolve(w)) {
                for (i, wi) in w.iter().enumerate() {
                    if let Some(v) = doc.resolve(wi).num() {
                        widths.insert(first + i as u32, v);
                    }
                }
            }
            if let Some(fd) = get(d, b"FontDescriptor").and_then(|f| doc.dict(f)) {
                default_width = get(fd, b"MissingWidth")
                    .and_then(|w| doc.resolve(w).num())
                    .unwrap_or(0.0);
            }
            weight_bold |= descriptor_bold(doc, d);
            if widths.is_empty() {
                standard_widths(&base, &mut widths);
            }
        }
        let mut to_unicode = HashMap::new();
        if !two_byte {
            simple_encoding(doc, d, &mut to_unicode);
        }
        if let Some(tu) = get(d, b"ToUnicode") {
            parse_cmap(&doc.stream(tu)?, &mut to_unicode)?;
        }
        let lower = String::from_utf8_lossy(&base).to_ascii_lowercase();
        let bold = weight_bold
            || lower.contains("bold")
            || lower.contains("black")
            || lower.contains("heavy");
        Ok(Font {
            two_byte,
            to_unicode,
            widths,
            default_width,
            bold,
        })
    }

    fn codes(&self, s: &[u8]) -> Vec<u32> {
        if self.two_byte {
            s.chunks(2)
                .map(|c| u32::from(c[0]) << 8 | u32::from(*c.get(1).unwrap_or(&0)))
                .collect()
        } else {
            s.iter().map(|&c| u32::from(c)).collect()
        }
    }

    fn width(&self, code: u32) -> f64 {
        self.widths
            .get(&code)
            .copied()
            .unwrap_or(self.default_width)
    }
}

/// Helvetica's AFM widths for codes 32..=126.
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

/// Helvetica-Bold's AFM widths for codes 32..=126.
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667,
    611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556,
    278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

/// Widths for a standard-14 font that, as PDF 1.4 allowed, carries no `/Widths` (the ICC.1:2001
/// PDF sets its headings in such a Helvetica-Bold). Courier is monospaced; every other face is
/// approximated by the Helvetica of its weight. The widths only decide where a run ends, so an
/// approximation costs at worst a space misplaced in body text, which no heading test reads.
fn standard_widths(base: &[u8], out: &mut HashMap<u32, f64>) {
    let name = String::from_utf8_lossy(base);
    let table: Option<&[u16; 95]> = if name.starts_with("Courier") {
        None
    } else if name.contains("Bold") {
        Some(&HELVETICA_BOLD)
    } else {
        Some(&HELVETICA)
    };
    for c in 32u32..=126 {
        let w = table.map_or(600, |t| t[(c - 32) as usize]);
        out.insert(c, f64::from(w));
    }
}

fn descriptor_bold(doc: &Document, font: &Dict) -> bool {
    get(font, b"FontDescriptor")
        .and_then(|f| doc.dict(f))
        .and_then(|fd| get(fd, b"FontWeight"))
        .and_then(|w| doc.resolve(w).num())
        .is_some_and(|w| w >= 600.0)
}

/// `/W [c [w1 w2 ...] cfirst clast w ...]` (§9.7.4.3).
fn cid_widths(doc: &Document, w: &[Obj], out: &mut HashMap<u32, f64>) {
    let mut i = 0;
    while i < w.len() {
        let Some(c) = doc.resolve(&w[i]).num() else {
            return;
        };
        match w.get(i + 1).map(|x| doc.resolve(x)) {
            Some(Obj::Array(ws)) => {
                for (k, wk) in ws.iter().enumerate() {
                    if let Some(v) = doc.resolve(wk).num() {
                        out.insert(c as u32 + k as u32, v);
                    }
                }
                i += 2;
            }
            Some(Obj::Num(last)) => {
                let Some(v) = w.get(i + 2).and_then(|x| doc.resolve(x).num()) else {
                    return;
                };
                for code in c as u32..=*last as u32 {
                    out.insert(code, v);
                }
                i += 3;
            }
            _ => return,
        }
    }
}

/// WinAnsi (and, close enough for headings, Standard/MacRoman) base encoding plus `/Differences`.
/// A heading's characters are ASCII; the few WinAnsi punctuation codes above 0x7F are mapped so
/// a title does not carry mojibake.
fn simple_encoding(doc: &Document, font: &Dict, out: &mut HashMap<u32, String>) {
    for c in 0x20u32..0x7f {
        out.insert(c, char::from_u32(c).map(String::from).unwrap_or_default());
    }
    for (c, s) in [
        (0x91, "\u{2018}"),
        (0x92, "\u{2019}"),
        (0x93, "\u{201c}"),
        (0x94, "\u{201d}"),
        (0x95, "\u{2022}"),
        (0x96, "\u{2013}"),
        (0x97, "\u{2014}"),
        (0xa0, " "),
        (0xad, "-"),
    ] {
        out.insert(c, s.to_string());
    }
    for c in 0xa1u32..=0xff {
        if c != 0xad {
            out.insert(c, char::from_u32(c).map(String::from).unwrap_or_default());
        }
    }
    let Some(enc) = get(font, b"Encoding").and_then(|e| doc.dict(e)) else {
        return;
    };
    let Some(Obj::Array(diffs)) = get(enc, b"Differences").map(|d| doc.resolve(d)) else {
        return;
    };
    let mut code = 0u32;
    for item in diffs {
        match doc.resolve(item) {
            Obj::Num(n) => code = *n as u32,
            Obj::Name(glyph) => {
                if let Some(s) = glyph_text(glyph) {
                    out.insert(code, s);
                }
                code += 1;
            }
            _ => {}
        }
    }
}

/// Text for the glyph names a `/Differences` array in these specifications uses.
fn glyph_text(name: &[u8]) -> Option<String> {
    let n = std::str::from_utf8(name).ok()?;
    if n.len() == 1 && n.as_bytes()[0].is_ascii_alphabetic() {
        return Some(n.to_string());
    }
    if let Some(hex) = n.strip_prefix("uni").filter(|h| h.len() == 4) {
        return u32::from_str_radix(hex, 16)
            .ok()
            .and_then(char::from_u32)
            .map(String::from);
    }
    let digits = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ];
    if let Some(d) = digits.iter().position(|d| *d == n) {
        return Some(d.to_string());
    }
    let s = match n {
        "space" => " ",
        "period" => ".",
        "comma" => ",",
        "hyphen" | "minus" => "-",
        "colon" => ":",
        "parenleft" => "(",
        "parenright" => ")",
        "slash" => "/",
        "endash" => "\u{2013}",
        "emdash" => "\u{2014}",
        "quoteright" => "\u{2019}",
        _ => return None,
    };
    Some(s.to_string())
}

/// A ToUnicode CMap's `bfchar` and `bfrange` entries (§9.10.3).
fn parse_cmap(cmap: &[u8], out: &mut HashMap<u32, String>) -> Result<(), String> {
    let mut lx = Lexer::new(cmap, 0);
    let mut toks = Vec::new();
    while let Some(t) = lx.next()? {
        toks.push(t);
    }
    let code = |o: &Obj| match o {
        Obj::Str(b) => Some(b.iter().fold(0u32, |a, &x| a << 8 | u32::from(x))),
        _ => None,
    };
    let text = |o: &Obj| match o {
        Obj::Str(b) => Some(utf16be(b)),
        _ => None,
    };
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Obj::Op(op) if op == b"beginbfchar" => {
                i += 1;
                while i + 1 < toks.len() && toks[i] != Obj::Op(b"endbfchar".to_vec()) {
                    if let (Some(c), Some(t)) = (code(&toks[i]), text(&toks[i + 1])) {
                        out.insert(c, t);
                    }
                    i += 2;
                }
            }
            Obj::Op(op) if op == b"beginbfrange" => {
                i += 1;
                while i + 2 < toks.len() && toks[i] != Obj::Op(b"endbfrange".to_vec()) {
                    if let (Some(lo), Some(hi)) = (code(&toks[i]), code(&toks[i + 1])) {
                        match &toks[i + 2] {
                            Obj::Str(dst) => {
                                let mut units: Vec<u16> = dst
                                    .chunks(2)
                                    .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
                                    .collect();
                                for c in lo..=hi.min(lo + 0xffff) {
                                    out.insert(c, String::from_utf16_lossy(&units));
                                    if let Some(last) = units.last_mut() {
                                        *last = last.wrapping_add(1);
                                    }
                                }
                            }
                            Obj::Array(dsts) => {
                                for (k, d) in dsts.iter().enumerate() {
                                    if let Some(t) = text(d) {
                                        out.insert(lo + k as u32, t);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    i += 3;
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

fn utf16be(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// A 2-D affine matrix `[a b c d e f]` (§8.3.3).
#[derive(Clone, Copy, Debug, PartialEq)]
struct M([f64; 6]);

impl M {
    const ID: M = M([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    /// `self × other`: apply `self` first, then `other`.
    fn mul(self, o: M) -> M {
        let [a, b, c, d, e, f] = self.0;
        let [oa, ob, oc, od, oe, of] = o.0;
        M([
            a * oa + b * oc,
            a * ob + b * od,
            c * oa + d * oc,
            c * ob + d * od,
            e * oa + f * oc + oe,
            e * ob + f * od + of,
        ])
    }
    fn translate(tx: f64, ty: f64) -> M {
        M([1.0, 0.0, 0.0, 1.0, tx, ty])
    }
}

/// Text state that `q`/`Q` save with the graphics state (§9.3).
#[derive(Clone)]
struct GState {
    ctm: M,
    font: Option<u32>,
    size: f64,
    char_spacing: f64,
    word_spacing: f64,
    hscale: f64,
    leading: f64,
}

struct Builder {
    lines: Vec<Line>,
    cur: String,
    cur_bold: Option<bool>,
    /// Device-space baseline y of the current line and where its last glyph ended.
    y: f64,
    end_x: f64,
}

impl Builder {
    fn flush(&mut self) {
        let text = collapse_ws(&self.cur);
        if !text.is_empty() {
            self.lines.push(Line {
                text,
                bold: self.cur_bold.unwrap_or(false),
            });
        }
        self.cur.clear();
        self.cur_bold = None;
    }

    /// Places a run starting at device `(x, y)` with font size `em` (device units).
    fn place(&mut self, x: f64, y: f64, em: f64) {
        let tol = (em * 0.3).max(0.5);
        // A fresh page starts from NaN, which always opens a line.
        if self.y.is_nan() || (y - self.y).abs() > tol {
            self.flush();
            self.y = y;
        } else if x > self.end_x + em * 0.15 || x < self.end_x - em {
            self.cur.push(' ');
        }
    }
}

/// Every page's text, as lines in drawing order.
pub fn lines(doc: &Document) -> Result<Vec<Line>, String> {
    let cat = doc.catalog().ok_or("no document catalog")?;
    let root = get(cat, b"Pages").ok_or("catalog without /Pages")?;
    let mut pages = Vec::new();
    collect_pages(doc, root, None, &mut pages, 0)?;
    let mut b = Builder {
        lines: Vec::new(),
        cur: String::new(),
        cur_bold: None,
        y: f64::NAN,
        end_x: 0.0,
    };
    for (page, resources) in pages {
        let fonts = load_fonts(doc, resources)?;
        let content = match get(page, b"Contents").map(|c| doc.resolve(c)) {
            None | Some(Obj::Null) => continue,
            Some(Obj::Array(parts)) => {
                let mut all = Vec::new();
                for p in parts {
                    all.extend(doc.stream(p)?);
                    all.push(b'\n');
                }
                all
            }
            Some(c) => doc.stream(c)?,
        };
        run(&content, &fonts, &mut b)?;
        b.flush();
        b.y = f64::NAN;
    }
    Ok(b.lines)
}

fn collect_pages<'a>(
    doc: &'a Document,
    node: &'a Obj,
    inherited: Option<&'a Obj>,
    out: &mut Vec<(&'a Dict, Option<&'a Obj>)>,
    depth: u32,
) -> Result<(), String> {
    if depth > 64 {
        return Err("page tree too deep".into());
    }
    let d = doc.dict(node).ok_or("page tree node is not a dictionary")?;
    let res = get(d, b"Resources").or(inherited);
    match get(d, b"Type").and_then(Obj::name) {
        Some(b"Pages") => {
            if let Some(Obj::Array(kids)) = get(d, b"Kids").map(|k| doc.resolve(k)) {
                for k in kids {
                    collect_pages(doc, k, res, out, depth + 1)?;
                }
            }
        }
        _ => out.push((d, res)),
    }
    Ok(())
}

fn load_fonts(doc: &Document, resources: Option<&Obj>) -> Result<Vec<(Vec<u8>, Font)>, String> {
    let mut fonts = Vec::new();
    let Some(res) = resources.and_then(|r| doc.dict(r)) else {
        return Ok(fonts);
    };
    let Some(fd) = get(res, b"Font").and_then(|f| doc.dict(f)) else {
        return Ok(fonts);
    };
    for (name, f) in fd {
        fonts.push((name.clone(), Font::load(doc, f)?));
    }
    Ok(fonts)
}

fn run(content: &[u8], fonts: &[(Vec<u8>, Font)], b: &mut Builder) -> Result<(), String> {
    let mut lx = Lexer::new(content, 0);
    let mut ops: Vec<Obj> = Vec::new();
    let mut gs = GState {
        ctm: M::ID,
        font: None,
        size: 0.0,
        char_spacing: 0.0,
        word_spacing: 0.0,
        hscale: 1.0,
        leading: 0.0,
    };
    let mut stack: Vec<GState> = Vec::new();
    let mut tm = M::ID;
    let mut tlm = M::ID;
    while let Some(tok) = lx.next()? {
        let Obj::Op(op) = tok else {
            ops.push(tok);
            continue;
        };
        let n = |i: usize| ops.get(i).and_then(Obj::num).unwrap_or(0.0);
        match op.as_slice() {
            b"q" => stack.push(gs.clone()),
            b"Q" => {
                if let Some(s) = stack.pop() {
                    gs = s;
                }
            }
            b"cm" => gs.ctm = M([n(0), n(1), n(2), n(3), n(4), n(5)]).mul(gs.ctm),
            b"BT" => {
                tm = M::ID;
                tlm = M::ID;
            }
            b"Tf" => {
                let name = ops.first().and_then(Obj::name).unwrap_or(b"");
                gs.font = fonts.iter().position(|(k, _)| k == name).map(|i| i as u32);
                gs.size = n(1);
            }
            b"Tc" => gs.char_spacing = n(0),
            b"Tw" => gs.word_spacing = n(0),
            b"Tz" => gs.hscale = n(0) / 100.0,
            b"TL" => gs.leading = n(0),
            b"Td" | b"TD" => {
                if op == b"TD" {
                    gs.leading = -n(1);
                }
                tlm = M::translate(n(0), n(1)).mul(tlm);
                tm = tlm;
            }
            b"Tm" => {
                tlm = M([n(0), n(1), n(2), n(3), n(4), n(5)]);
                tm = tlm;
            }
            b"T*" => {
                tlm = M::translate(0.0, -gs.leading).mul(tlm);
                tm = tlm;
            }
            b"Tj" | b"'" | b"\"" | b"TJ" => {
                if op == b"'" || op == b"\"" {
                    if op == b"\"" {
                        gs.word_spacing = n(0);
                        gs.char_spacing = n(1);
                    }
                    tlm = M::translate(0.0, -gs.leading).mul(tlm);
                    tm = tlm;
                }
                let font = gs.font.map(|i| &fonts[i as usize].1);
                let items: Vec<&Obj> = match ops.last() {
                    Some(Obj::Array(a)) if op == b"TJ" => a.iter().collect(),
                    Some(s @ Obj::Str(_)) => vec![s],
                    _ => Vec::new(),
                };
                for item in items {
                    match item {
                        Obj::Num(adj) => {
                            tm = M::translate(-adj / 1000.0 * gs.size * gs.hscale, 0.0).mul(tm);
                        }
                        Obj::Str(s) => show(s, font, &gs, &mut tm, b),
                        _ => {}
                    }
                }
            }
            b"BI" => skip_inline_image(&mut lx, content),
            _ => {}
        }
        ops.clear();
    }
    Ok(())
}

/// Draws one string: places it on the line builder and advances the text matrix.
fn show(s: &[u8], font: Option<&Font>, gs: &GState, tm: &mut M, b: &mut Builder) {
    let Some(font) = font else { return };
    let trm = M([gs.size * gs.hscale, 0.0, 0.0, gs.size, 0.0, 0.0])
        .mul(*tm)
        .mul(gs.ctm);
    let em = (trm.0[2].powi(2) + trm.0[3].powi(2)).sqrt();
    let (x, y) = (trm.0[4], trm.0[5]);
    let codes = font.codes(s);
    let text: String = codes
        .iter()
        .map(|c| font.to_unicode.get(c).map_or("\u{fffd}", String::as_str))
        .collect();
    if text.is_empty() {
        return;
    }
    b.place(x, y, em);
    if b.cur_bold.is_none() && !text.trim().is_empty() {
        b.cur_bold = Some(font.bold);
    }
    b.cur.push_str(&text);
    for &c in &codes {
        let space = !font.two_byte && c == 32;
        let w = font.width(c) / 1000.0 * gs.size
            + gs.char_spacing
            + if space { gs.word_spacing } else { 0.0 };
        *tm = M::translate(w * gs.hscale, 0.0).mul(*tm);
    }
    let end = M([gs.size * gs.hscale, 0.0, 0.0, gs.size, 0.0, 0.0])
        .mul(*tm)
        .mul(gs.ctm);
    b.end_x = end.0[4];
}

/// Skips `BI <dict> ID <binary> EI` (§8.9.7), whose data the lexer cannot tokenise.
fn skip_inline_image(lx: &mut Lexer<'_>, content: &[u8]) {
    let from = lx.pos;
    let Some(id) = content[from..].windows(2).position(|w| w == b"ID") else {
        lx.pos = content.len();
        return;
    };
    let data = from + id + 3;
    let mut i = data;
    while i + 2 <= content.len() {
        if &content[i..i + 2] == b"EI"
            && content
                .get(i.wrapping_sub(1))
                .is_some_and(u8::is_ascii_whitespace)
            && content.get(i + 2).is_none_or(u8::is_ascii_whitespace)
        {
            lx.pos = i + 2;
            return;
        }
        i += 1;
    }
    lx.pos = content.len();
}

/// The clause headings among `lines`.
///
/// An ISO-style annex heading is set on up to three lines — `Annex A`, `(informative)`, then the
/// title — so a bold line that is only `Annex X` takes its title from the next bold line, past the
/// normative/informative marker. Either way the marker is not part of the title.
pub fn headings(lines: &[Line]) -> Vec<Clause> {
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if !l.bold {
            continue;
        }
        if let Some(letter) = annex_letter(&l.text) {
            let title = lines[i + 1..]
                .iter()
                .take(2)
                .find(|n| n.bold && !is_annex_marker(&n.text))
                .map(|n| n.text.clone());
            if let Some(title) = title {
                out.push(Clause {
                    number: letter.to_string(),
                    title,
                });
            }
            continue;
        }
        out.extend(heading(&l.text));
    }
    out
}

/// `X` for a line that is exactly `Annex X`.
fn annex_letter(text: &str) -> Option<char> {
    let r = text.strip_prefix("Annex ")?;
    let mut cs = r.chars();
    let c = cs.next().filter(char::is_ascii_uppercase)?;
    cs.next().is_none().then_some(c)
}

fn is_annex_marker(text: &str) -> bool {
    matches!(text, "(informative)" | "(normative)")
}

/// Reads `<number> <title>` off one line, or `Annex X (informative) <title>`.
fn heading(text: &str) -> Option<Clause> {
    if text.contains("....") || text.contains(". . .") {
        return None; // a table-of-contents entry
    }
    let (number, rest) = if let Some(r) = text.strip_prefix("Annex ") {
        let (letter, rest) = r.split_once(' ').unwrap_or((r, ""));
        let ok = letter.len() == 1 && letter.as_bytes()[0].is_ascii_uppercase();
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix("(informative)")
            .or_else(|| rest.strip_prefix("(normative)"))
            .unwrap_or(rest);
        (ok.then(|| letter.to_string())?, rest)
    } else {
        let (tok, rest) = text.split_once(' ')?;
        let tok = tok.trim_end_matches('.');
        (is_clause_number(tok).then(|| tok.to_string())?, rest)
    };
    let title = collapse_ws(rest);
    let first = title.chars().next()?;
    // A title opens with a word, a parenthesis, or a quote (ICC.1:2022 10.12.2 `“A” curves`); a
    // digit or other punctuation there is a wrapped body line that begins with a number.
    let opens = first.is_alphabetic() || matches!(first, '(' | '"' | '\u{201c}' | '\u{2018}');
    opens.then_some(Clause { number, title })
}

/// `7`, `9.2.10`, `0.1`, `A.1`, `B.4.2` — never a bare letter (that is `Annex X`'s job), never a
/// bare `0` (the Introduction's subclauses are `0.N`, the Introduction itself is unnumbered), and
/// never a number with an empty or non-numeric part.
fn is_clause_number(tok: &str) -> bool {
    let mut parts = tok.split('.');
    let Some(head) = parts.next() else {
        return false;
    };
    let rest: Vec<&str> = parts.collect();
    let lettered = head.len() == 1 && head.as_bytes()[0].is_ascii_uppercase();
    let numeric = !head.is_empty() && head.len() <= 2 && head.bytes().all(|c| c.is_ascii_digit());
    let rest_ok = rest
        .iter()
        .all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|c| c.is_ascii_digit()));
    let needs_rest = lettered || head == "0";
    rest_ok && (numeric || lettered) && (!needs_rest || !rest.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, bold: bool) -> Line {
        Line {
            text: text.into(),
            bold,
        }
    }

    #[test]
    fn bold_numbered_lines_are_headings() {
        let got = headings(&[
            line("9.2.10 BToD1Tag", true),
            line("9.2.10), BToD2Tag (see 9.2.11)", true),
            line("9.2.10 BToD1Tag", false),
            line("10.7 dataType ........ 49", true),
            line("Annex A (informative) Profile connection", true),
            line("A.1 General", true),
            line("2022 International Color Consortium", true),
            line("A Bold sentence", true),
            line("Annex B", true),
            line("(normative)", false),
            line("Embedding profiles", true),
            line("0.1 General", true),
            line("10.12.2 \u{201c}A\u{201d} curves", true),
            line("4.2 1,5 times", true),
        ]);
        let c = |n: &str, t: &str| Clause {
            number: n.into(),
            title: t.into(),
        };
        assert_eq!(
            got,
            vec![
                c("9.2.10", "BToD1Tag"),
                c("A", "Profile connection"),
                c("A.1", "General"),
                c("B", "Embedding profiles"),
                c("0.1", "General"),
                c("10.12.2", "\u{201c}A\u{201d} curves"),
            ]
        );
    }

    #[test]
    fn clause_number_shapes() {
        for ok in ["7", "9.2.10", "0.1", "A.1", "B.4.2", "11.3.2.1"] {
            assert!(is_clause_number(ok), "{ok}");
        }
        for bad in ["A", "0", "2022", "9..2", "9.", "a.1", "9.x"] {
            assert!(!is_clause_number(bad), "{bad}");
        }
    }

    #[test]
    fn cmap_ranges_and_chars_map_codes() {
        let cmap = b"2 beginbfchar <0003> <0020> <0011> <0041> endbfchar
1 beginbfrange <0044> <0046> <0061> endbfrange
1 beginbfrange <0050> <0051> [<0058> <0059>] endbfrange";
        let mut m = HashMap::new();
        parse_cmap(cmap, &mut m).unwrap();
        let want = [
            (3, " "),
            (0x11, "A"),
            (0x44, "a"),
            (0x45, "b"),
            (0x46, "c"),
            (0x50, "X"),
            (0x51, "Y"),
        ];
        for (k, v) in want {
            assert_eq!(m.get(&k).map(String::as_str), Some(v), "{k:#x}");
        }
    }

    fn simple_font(bold: bool) -> Font {
        let mut to_unicode = HashMap::new();
        for c in 0x20u32..0x7f {
            to_unicode.insert(c, char::from_u32(c).unwrap().to_string());
        }
        Font {
            two_byte: false,
            to_unicode,
            widths: HashMap::new(),
            default_width: 500.0,
            bold,
        }
    }

    #[test]
    fn positioned_runs_join_words_and_split_lines() {
        // "DToB0T" + "ag" abut (x advances exactly the run width: 6 glyphs × 0.5 em × 10);
        // the second Td leaves a gap (a word break); the TD drops a line.
        let content = b"BT /F1 10 Tf 0 100 Td (9.2.10) Tj 40 0 Td (DToB0T) Tj 30 0 Td (ag) Tj 0 -12 TD (next) Tj ET";
        let fonts = vec![(b"F1".to_vec(), simple_font(true))];
        let mut b = Builder {
            lines: Vec::new(),
            cur: String::new(),
            cur_bold: None,
            y: f64::NAN,
            end_x: 0.0,
        };
        run(content, &fonts, &mut b).unwrap();
        b.flush();
        assert_eq!(
            b.lines,
            vec![line("9.2.10 DToB0Tag", true), line("next", true)]
        );
    }
}
